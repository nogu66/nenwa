# nenwa design

This document describes how nenwa works inside. For how to use it, see [README.md](README.md).

nenwa lets coding agents that belong to one person or a small team send messages to each other. It is one Rust
binary. Each machine runs one daemon. Machines join a team by invite and connect peer to peer with
[iroh](https://github.com/n0-computer/iroh). Local agents reach the daemon through their own MCP config and hooks.
Cloud agents use a named inbox.

## Goals and non-goals

Goals:

- One executable, no runtime to install. macOS, Linux and Windows.
- Work on a single machine with no network at all. P2P is off until the first `nenwa invite`.
- Put a message into the conversation of the target agent, using whatever that agent offers (a socket, a CLI, a
  local HTTP server, hooks). No terminal scraping.
- Never block an agent. If the daemon is down, hooks print nothing and exit 0.
- No server of our own. Peers talk directly, or through public iroh relays when a direct path fails.
- Make it clear to the receiving agent that a message is not an instruction from its user.

Non-goals (not implemented):

- Accounts, sign-in, a signed member list, or removing one member from a team.
- File attachments, approval pop-ups, a menu bar display.
- A self-hosted relay.
- ACP. Agents are wired through MCP, hooks, a plugin or an extension.

## Concepts

| Term | Meaning |
|---|---|
| Team | The machines that share one 32-byte team key. The key is created by `init` (or the first `install`) and carried inside an invite. |
| Machine | One computer running one daemon. Its identity is an iroh key pair derived from `seed` in `config.json`. The seed is kept when the team key changes. |
| Person | A name in `config.json`. Default: the OS user name. |
| Session | One open conversation of a local agent (Claude Code, Codex, Cursor, ...). |
| Inbox | A named queue on a machine for a peer that cannot be pushed to (Grok Bot, Muse, the terminal). |

### Addresses

Every destination has the form `person/label@machine`. No part may contain `/`, `@` or whitespace.

| Destination | Label |
|---|---|
| Claude Code session | `<folder>-<first 4 chars of the session ID>` |
| Hook-wired session (Codex, Cursor, ...) | `<folder>-<last 4 alphanumeric chars of the conversation ID, lowercase>` |
| Inbox | the inbox name, `^[a-z0-9][a-z0-9._-]{0,39}$` |
| Terminal | `terminal` (an inbox that always exists) |

`<folder>` is the base name of the working directory, lowercased, with runs of other characters replaced by `-`, cut
to 40 characters (`session` if empty). Person and machine names are cleaned the same way. The machine default is the
host name up to the first dot.

Claude Code `/clear` changes the session ID, so the label changes. The daemon remembers which process used an old
label and delivers mail for the old label to the current session of that process.

## Architecture

```
agent ── nenwa mcp ───┐
agent ── nenwa hook ──┤  local socket          iroh, ALPN nenwa/1
you ──── nenwa send ──┼────────────▶ daemon ◀══════════════════▶ daemon on another machine
Muse ─── POST /mcp ───┘  (one JSON line each way)
```

All commands are subcommands of the same binary.

| Process | Lifetime | Role |
|---|---|---|
| `nenwa daemon` | One per machine, started at login | Holds all state. Owns the iroh endpoint, the local socket, the inboxes, the outbox and the optional HTTP endpoint. |
| `nenwa mcp` | Started by an agent, lives as long as the agent keeps it | stdio MCP server. Translates tool calls into daemon requests. Each input line is answered on its own thread, so a waiting `check_inbox` does not block other calls. |
| `nenwa hook <agent> <event>` | One per hook call | Reads the hook JSON from stdin (3 second limit), asks the daemon, prints the daemon's answer if there is one, exits 0. |
| CLI (`who`, `send`, `inbox`, ...) | One per command | Asks the daemon and prints the result. |

`who`, `send`, `inbox` and the MCP server start the daemon in the background if the socket is missing or refuses the
connection (they then retry for 5 seconds). `nenwa hook` never starts it. A second daemon detects the first one by
connecting to the socket and exits normally, so autostart does not loop.

### Local IPC

The daemon listens on `~/.nenwa/sock` (mode 600, directory mode 700). On Windows it is the named pipe
`\\.\pipe\nenwa-<user>`, with a hash of the path appended when `NENWA_HOME` is not the default.

A client connects, writes one JSON line (at most 1 MiB is read), reads one JSON line, and closes. The reply is
`{"ok":true,"result":...}` or `{"ok":false,"error":"..."}`.

| `op` | Fields | Result |
|---|---|---|
| `who` | none | `me`, `network`, `local` (sessions and inboxes here), `peers`, `outbox` (count) |
| `send` | `to`, `body`, optional `reply_to`, and one sender hint: `agent`+`cwd`+`ancestors`, or `as`, or `fromSession` | `id`, `from`, `state` |
| `register` | `name` | Creates the inbox, returns its `address` |
| `inbox` | `name` or `agent`+`cwd`+`ancestors`, optional `wait` (seconds, capped at 50) | `address`, `messages`. Reading removes the messages. With `name`, the inbox is created if missing. |
| `hook` | `agent`, `event`, `input` (the agent's hook JSON), `ancestors` | The JSON the agent expects, or null |
| `http` | `on`, `port`, `expires`, `funnel` | Opens (or reopens) or closes the HTTP endpoint |
| `addr` | none | This machine's iroh address, for an invite. Fails when P2P is off. |
| `shutdown` | none | The daemon closes the socket and the endpoint and exits |

### Sending

The daemon picks the sender, in this order: the hook-wired session resolved from `agent`; the inbox named by `as`
(created if missing); the Claude Code session named by `fromSession`; otherwise `terminal`. The caller never supplies
a `from` address.

Then:

1. The body must be non-empty and at most 16384 UTF-16 code units (`MAX_BODY`).
2. Depth is computed (see Trust and safety). At depth 10 the send fails.
3. If the target is this machine, deliver locally: Claude Code session, then hook-wired session, then inbox. If none
   matches, the state is `no_session`.
4. If the target machine is online, send a `msg` line and wait up to 10 seconds for its `ack`. The ack state is the
   result.
5. Otherwise (offline, or no ack) append the message to `outbox.json` and return `queued`.

Result states: `delivered`, `inbox`, `queued`, `no_session`, `refused`, `rate_limited`.

When a peer connects, the daemon resends every outbox entry addressed to it. An entry leaves the outbox when any ack
comes back. The receiver remembers the ids it accepted and acks a repeated id as `delivered` without delivering again.

## Network layer

- Transport: one iroh endpoint per machine, ALPN `nenwa/1`. iroh gives QUIC with TLS and addresses machines by public
  key. The default setup uses n0's DNS discovery and relays, with hole punching for direct paths.
- `NENWA_RELAY=disabled` turns off relays and DNS. Peers are then reached only through the IP addresses stored in the
  invite and in `peers.json`. The tests use this.
- With `network: false` in `config.json` no endpoint is created at all. `nenwa install` creates the config in this
  state. `nenwa invite` flips it to true and restarts the daemon.

### Invites

`nenwa invite` prints an install line that ends in an invite string. The string is base64url (no padding) of:

```
team key (32) | public key (32) | relay length (1) | relay URL | address count (1) | addresses
address = 4 or 6 (1) | IP (4 or 16) | port (2, big endian)
```

The invite contains the team key, so it is a secret. `nenwa join <invite>` writes a config with that key and
`network: true`, stores the inviter in `peers.json`, and runs `install`.

### Connection and authentication

The daemon dials every entry of `peers.json` at start and every 10 seconds (15 second connect timeout). The dialer
opens one bidirectional stream. All traffic is JSON lines on that stream. A line longer than 256 KiB, or a line that
is not JSON, closes the connection.

Both sides first send `hello`:

```json
{"t":"hello","person":"...","machine":"...","mac":"<hex>","addr":{"id":"...","relay":"...","addrs":["..."]}}
```

`mac` is `HMAC-SHA256(team key, "nenwa/hello/" + own public key + peer public key)`. The receiver recomputes it with
the keys of the QUIC connection. A missing, late (10 seconds) or wrong hello closes the connection. A machine without
the team key cannot pass this step.

If two machines dial each other at once, both keep the connection dialed by the smaller key. A redial from the same
side replaces the old connection.

### Lines after hello

| `t` | Content | Notes |
|---|---|---|
| `presence` | `sessions`: list of `{address, title, status}` | Sent after hello and whenever the local list changes (checked every 2 seconds). At most 100 entries are kept. |
| `peers` | Addresses of the other machines the sender knows | The receiver dials the ones it is not connected to (at most 100). This is how a third machine finds the rest. |
| `msg` | `id`, `from`, `to`, `reply_to`, `body`, `sent_at`, `depth` | One message |
| `ack` | `id`, `state` | Answer to a `msg` |

Message bodies cross the network only inside this stream. The names and addresses of peers are saved in `peers.json`.

## Getting messages into agents

`nenwa install` registers nenwa with every agent whose config folder exists. Each file is copied to
`<file>.bak-nenwa` once before the first change. Other hooks and MCP servers are kept. A JSON config that cannot be
parsed (comments, for example) is left untouched. `nenwa setup --remove` undoes the registration.

### Registration

| Agent | What `install` writes |
|---|---|
| Claude Code | `claude mcp add -s user nenwa -- <exe> mcp`. No hooks. |
| Codex | `~/.codex/config.toml`: an `[mcp_servers.nenwa]` block between `# nenwa:start` and `# nenwa:end` (`mcp --agent codex`). `~/.codex/hooks.json`: `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Stop`, `SessionEnd`. |
| Cursor | `~/.cursor/mcp.json` and `~/.cursor/hooks.json`: `sessionStart`, `beforeSubmitPrompt`, `postToolUse`, `stop`, `sessionEnd`. |
| Gemini CLI | `~/.gemini/settings.json`: `mcpServers.nenwa` and hooks `SessionStart`, `BeforeAgent`, `AfterTool`, `AfterAgent`, `SessionEnd`. |
| Qwen Code | `~/.qwen/settings.json`: `mcpServers.nenwa` and hooks `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Stop`, `SessionEnd`. |
| OpenCode | `~/.config/opencode/opencode.json` (or `.jsonc`): `mcp.nenwa`. A plugin at `plugins/nenwa.js` (source: `src/opencode_plugin.js`) plays the role of hooks. |
| Pi | `~/.pi/agent/extensions/nenwa.ts` (source: `src/pi_extension.ts`). Pi has no MCP, so the extension provides the tools `nenwa_list_team` and `nenwa_send_message` by running a short-lived `nenwa mcp --agent pi` per call. |
| Copilot CLI | `~/.copilot/mcp-config.json` and `~/.copilot/hooks/nenwa.json`: `sessionStart`, `userPromptSubmitted`, `postToolUse`, `agentStop`, `sessionEnd`. |
| Antigravity CLI | `~/.gemini/config/mcp_config.json` and `hooks.json` (under the key `nenwa`): `SessionStart`, `PreInvocation`, `Stop`. |

All hook commands have the form `<exe> hook <agent> <event>`. The agent-specific event names map to six nenwa events:
`session-start`, `user-prompt-submit`, `post-tool-use`, `stop`, `session-end`, and `wait`.

### Delivery

| Agent | Push into the conversation | Fallback through hooks |
|---|---|---|
| Claude Code | The daemon reads `~/.claude/sessions/<pid>.json`, takes `messagingSocketPath`, and writes one `{"type":"user","message":{...}}` line to that socket (a named pipe on Windows). | None. A failed write gives `no_session`. |
| Codex | `codex queue --thread <id> --message <frame>` (20 second limit). | The message goes to the session's inbox and is injected at `PostToolUse` or continued at `Stop`. |
| OpenCode | `POST <server>/session/<id>/prompt_async` on OpenCode's local server (10 second limit). | Inbox. The plugin long-polls `wait`, appends at `tool.execute.after`, and continues at `session.idle`. |
| Pi | None. The extension long-polls `wait` and puts the message in as it arrives. The sender sees `inbox`. | Continued at `agent_settled`. |
| Cursor | None. | Injected at `postToolUse` (`additional_context`), continued at `stop` (`followup_message`). `beforeSubmitPrompt` cannot add text, so nothing is taken there. |
| Copilot CLI | None. | `additionalContext` at `postToolUse`, `decision: "block"` at `agentStop`. |
| Gemini CLI | None. | `additionalContext` at `BeforeAgent` and `AfterTool`, `decision: "deny"` at `AfterAgent`. |
| Qwen Code | None. | `additionalContext` at `UserPromptSubmit` and `PostToolUse`, `decision: "block"` at `Stop`. |
| Antigravity CLI | None. | `injectSteps` at `PreInvocation`, `decision: "continue"` at `Stop`. |

An agent without a push path receives mail only the next time one of its hooks runs. An idle Cursor, Copilot CLI,
Gemini CLI, Qwen Code or Antigravity CLI session does not wake up.

At `session-start` the hook output also tells the conversation its own address. OpenCode is the exception: adding
text there would start a reply by itself.

### The long-poll `wait`

The Pi extension and the OpenCode plugin keep one `nenwa hook <agent> wait` process running per open conversation.
The daemon holds that request for up to 25 seconds and answers as soon as mail arrives. The client then inserts the
text (Pi: `sendMessage` with `triggerTurn`; OpenCode: `promptAsync`) and calls `wait` again. If a call returns empty
in under 2 seconds (no daemon, for example), the client sleeps 5 seconds before retrying. One-shot runs (`opencode
run`, Pi outside `tui` and `rpc` mode) do not listen. `wait` does not change the busy or idle status of the session.

### Identifying the session

Claude Code: `nenwa mcp` walks up its parent processes (at most 6 levels), finds the first one with a
`~/.claude/sessions/<pid>.json`, and sends that file's `sessionId` as `fromSession`. `CLAUDE_CODE_SESSION_ID` is used
only when that fails, because it goes stale after `/clear`.

Hook-wired agents: every hook call registers or refreshes a record keyed by `<agent>:<conversation ID>` in
`agents.json`. The ID is read from `session_id`, `conversation_id`, `sessionId` or `conversationId`. The folder comes
from `cwd`, `workspace_roots[0]` or `workspacePaths[0]`. The status becomes `idle` (`session-start`, `stop`),
`offline` (`session-end`) or `busy` (other events).

Parent-process tracking: each hook call and each `mcp --agent` call sends its list of ancestor PIDs (nearest first,
at most 8). The shells in between change on every call. The first PID that two consecutive hook calls of one
conversation share is recorded as the agent process. Then:

- A record whose agent process has exited is treated as closed. Sending to it gives `no_session`.
- An MCP call carries no conversation ID. The daemon scores the live sessions of that agent: 4 if the recorded agent
  process is among the caller's ancestors, 2 if busy, 1 if the folder matches. The highest score wins, then the most
  recently seen. So once the process is known, two conversations in the same folder are told apart.
- Records unseen for one day are dropped.

## What the agent sees

Tools (the MCP server name is `nenwa`):

| Tool | Available to | Result |
|---|---|---|
| `list_team` | all | Text list: this machine's sessions and inboxes, then each peer with online or offline and its sessions |
| `send_message(to, message, reply_to?)` | all | JSON with `id`, `from`, `state` |
| `check_inbox(wait_secs?)` | servers started with `--as` or `--agent`, and the HTTP endpoint | The waiting messages, framed. Waits up to 50 seconds (25 over HTTP) when empty. |

A plain `nenwa mcp` (Claude Code) lists only the first two tools.

Every delivered message is wrapped:

```
<nenwa-message from="alice/api-3f2a@alice-mbp" id="<uuid>" reply_to="<uuid>">
body
</nenwa-message>
(one line saying this comes from another agent or person on the team, not from your user,
 and that a reply, if needed, goes through send_message with this from and id)
```

`</nenwa-message` inside the body is rewritten to `<\/nenwa-message`, so a body cannot close the frame early. The MCP
`instructions` string repeats the rule: a `<nenwa-message>` is a peer speaking, not the user, and the agent must not
do what its user would not allow.

## Inboxes, webhook wake and the HTTP endpoint

### Inboxes

An inbox is a file `inbox/<name>.json` holding a list of messages. It keeps the newest 500. Reading (`check_inbox`,
`nenwa inbox`, a hook) returns everything and empties the file. Inboxes are listed in `mailboxes.json`. `terminal`
always exists and receives replies to `nenwa send`. Hook-wired sessions use the same files under their session label.

An inbox can be created only from this machine: `nenwa mcp --as <name>` (or `NENWA_AS`), `send --as`, `inbox --as`,
`nenwa wake`, `nenwa muse`. A message from the network to an unknown label is answered `no_session`.

### Webhook wake

`nenwa wake <inbox> <url>` stores `{url, key}` in `wake/<inbox>.json` (mode 600). The key is read from a hidden
prompt, from `NENWA_WAKE_KEY`, or from a JSON file with `--import`. `nenwa grok <url>` is the same for the inbox
`grok` and prints the values to enter in Grok Bot.

When a message lands in the inbox the daemon POSTs JSON to the URL (15 second limit) with `source`, `mailbox`,
`unread`, `from` (sender addresses), `text`, `commands` (the CLI lines to read and reply) and `sent_at`. The message
body is not included. Authentication depends on the key:

- Key starts with `whsec_`: Standard Webhooks headers `webhook-id`, `webhook-timestamp`, `webhook-signature`.
- Otherwise: `Authorization: Bearer <key>` and `X-Automation-Key: <key>`.

While the inbox stays unread the daemon does not call the webhook again for 10 minutes. After a read, the next
message wakes it at once. This state is held in memory only.

### HTTP endpoint (`nenwa muse`)

For an agent that cannot start a local command. `nenwa muse` creates the inbox `muse`, generates a key (32 random
bytes, base64url), copies it to the clipboard (or writes it to `--key-out` / `NENWA_KEY_FILE`), asks the daemon to
listen, and publishes the port with `tailscale funnel --bg <port>`.

- The daemon binds `127.0.0.1` only, port 8787 (`NENWA_HTTP_PORT`). Outside traffic arrives through Funnel.
- Only `POST /mcp` is served. One JSON-RPC request in, one JSON response out. No SSE, no session ID. Notifications
  get 202.
- `Authorization: Bearer <key>` selects the inbox. Only the SHA-256 of the key is stored (`http/keys.json`), and
  hashes are compared without early exit. The sender is fixed to that inbox.
- A new `nenwa muse` replaces the key. The old one stops working when the endpoint reopens, which the same command
  triggers.

| Limit | Value | Response |
|---|---|---|
| Missing or wrong key | | 401 |
| Request body | 64 KiB | 413 |
| Requests per key | 30 per minute | 429, `Retry-After: 60` |
| Wrong-key requests, all clients | more than 60 per minute | 503 for every request for 10 minutes |
| Body read | 15 seconds | 408 |
| Header read | 10 seconds | connection closed |
| Open connections | 64 | further connections are dropped |

Auto-close: the endpoint has an expiry time (default 2 hours, `--hours`, 0 for none). The daemon checks every second.
At expiry it stops the listener, deletes `http/keys.json` and `http/enabled`, and turns off the Funnel it opened
(only the one that proxies to this port). `http/enabled` lets a restarted daemon reopen the endpoint, or close it if
the time has passed. `nenwa muse --off` does the same by hand. Messages in the inbox are kept.

## Trust and safety

Who can send:

- Any process of the same OS user that can open the local socket can send as `terminal`, as any inbox name, or as a
  local session. There is no authentication on the local socket beyond file permissions.
- A remote machine must hold the team key. After hello, every `msg` from it must have a `from` whose person and
  machine equal the names it gave in hello, and a `to` on this machine. Anything else is acked `refused`.
- The person and machine names in hello are self-declared. The team key is the only credential, so every key holder
  is trusted to name itself honestly.
- An HTTP client can send only as the inbox its key maps to.

What a message can do:

- A message is text placed in a conversation. nenwa does not execute anything from it. What happens next is decided
  by the receiving agent and its own permission settings. The frame and the instructions tell the agent to treat it
  as a peer's request. In a session that skips permission prompts, that request can still lead to actions nobody
  reviews.
- A message can wake an idle Claude Code, Codex, OpenCode or Pi session.

Runaway guards:

- Depth: a send from an agent that carries `reply_to` gets the depth of that message plus one. A send from the
  terminal, or without a known `reply_to`, starts at 0. At depth 10 the sending daemon refuses. The receiver stores
  the depth the peer reports.
- Rate: at most 20 messages per minute from one sender address are accepted from other machines. More are acked
  `rate_limited`.
- Size: 16384 UTF-16 code units per body, checked on send and on receive.
- Memory: the tables of seen ids and depths hold 5000 entries each and forget the oldest.

Loopback-only rules:

- The HTTP endpoint binds `127.0.0.1`.
- An OpenCode `server_url` reported by the plugin is accepted only for `127.0.0.1`, `localhost` or `[::1]` with a
  port, so hook input cannot make the daemon post messages elsewhere.

Outbound calls made by the daemon: iroh traffic, the configured webhook (no message body), the local `codex` binary,
the local OpenCode server, and `tailscale` when the endpoint closes.

On disk: state files are written with mode 600 and the directories with mode 700 on macOS and Linux. On Windows the
files rely on the user profile's permissions. `config.json` holds the team key and the machine seed in clear text.
`wake/<name>.json` holds the webhook key in clear text. Undelivered and unread message bodies sit in `outbox.json`
and `inbox/*.json` until sent or read.

## Install and autostart

`nenwa install`:

1. Creates `config.json` if missing (random team key, `network: false`).
2. Stops a running daemon (`shutdown` op; an old daemon that does not know it gets SIGTERM).
3. Registers autostart.
4. Registers with Claude Code and runs the setup for the other agents.

| OS | Mechanism | Details |
|---|---|---|
| macOS | launchd LaunchAgent `me.nenwa.daemon` | `~/Library/LaunchAgents/me.nenwa.daemon.plist`. `RunAtLoad`, `KeepAlive` only after a failed exit. Passes `NENWA_HOME` and the `PATH` of the installing shell (so `codex` is found). An unchanged plist is restarted with `kickstart`, not reloaded. |
| Linux | `systemd --user` unit `nenwa.service` | `$XDG_CONFIG_HOME/systemd/user/`. `Restart=on-failure`. Passes `NENWA_HOME` and `PATH`. Output appended to `daemon.log`. |
| Windows | Task Scheduler task `nenwa` | Logon trigger, least privilege, no time limit, restart after 1 minute on failure. Only with the default `NENWA_HOME`, because a task cannot carry environment variables. The daemon detaches from its console and writes `daemon.log` itself. |

Config changes (`init`, `invite`, `join`) restart the daemon through the same mechanism when the registered state
folder matches the current `NENWA_HOME`. Otherwise the daemon is shut down and the next command starts it.

`nenwa uninstall` removes autostart and all registrations. `~/.nenwa` stays.

`nenwa doctor` prints whether the daemon answers and, per agent: whether its binary is on `PATH`, whether nenwa is
registered in its config, and how many of its conversations are visible. It adds a hint when an agent is installed
but not registered, or registered with no visible conversation. It also lists the `grok` and `muse` inboxes when
they are set up.

## State files

All under `~/.nenwa`, or `NENWA_HOME`.

| File | Contents |
|---|---|
| `config.json` | `person`, `machine`, `teamKey`, `seed`, `network` |
| `peers.json` | Public key to `person`, `machine`, `relay`, `addrs`, `seen` |
| `outbox.json` | Messages for machines that are offline |
| `mailboxes.json` | Inbox names and creation times |
| `inbox/<name>.json` | Unread messages for an inbox or a hook-wired session |
| `agents.json` | Hook-wired sessions: agent, ID, folder, label, status, last seen, agent PID, last ancestors, OpenCode URL |
| `wake/<name>.json` | Webhook `url` and `key` |
| `http/keys.json` | Inbox name to SHA-256 of its HTTP key |
| `http/enabled` | Port, expiry and Funnel flag while the HTTP endpoint is open |
| `sock` | The local socket (not on Windows) |
| `daemon.log` | Log |

Environment variables read by the binary: `NENWA_HOME`, `NENWA_AS`, `NENWA_AGENT`, `NENWA_RELAY`, `NENWA_CODEX_BIN`,
`NENWA_WAKE_KEY`, `NENWA_HTTP_PORT`, `NENWA_HTTP_TTL_SECS`, `NENWA_KEY_FILE`, and one config folder override per
agent (`NENWA_CODEX_DIR`, `NENWA_CURSOR_DIR`, `NENWA_GEMINI_DIR`, `NENWA_QWEN_DIR`, `NENWA_OPENCODE_DIR`,
`NENWA_COPILOT_DIR`, `NENWA_PI_DIR`, `NENWA_ANTIGRAVITY_DIR`). `CLAUDE_CONFIG_DIR`, `XDG_CONFIG_HOME`,
`COPILOT_HOME` and `PI_CODING_AGENT_DIR` are honored where the agents use them.

## Testing

- Unit tests live next to the code: hello HMAC, the tie-break for double connections, invite encoding, frame
  escaping, address parsing, name cleaning, Standard Webhooks signing, HTTP key hashing, recognizing nenwa's own
  hooks, the systemd unit, the Windows pipe name.
- `tests/e2e.rs` drives the built binary. It has its own runner (`harness = false`) because the cases run in order
  and share state.
- It never touches real state: `NENWA_HOME`, `HOME` (`USERPROFILE` on Windows) and `CLAUDE_CONFIG_DIR` point to a
  temporary folder. `NENWA_TEST_NO_INSTALL` skips autostart on `join`. `NENWA_TEST_NO_TAILSCALE` skips Tailscale and
  the clipboard.
- Two daemons (two people) run on one host with `NENWA_RELAY=disabled`. Claude Code sessions are faked with unix
  sockets or named pipes, the webhook with a local HTTP server, and `codex` with the test binary itself
  (`NENWA_CODEX_BIN`).
- Covered: listing, delivery and framing, `/clear`, `no_session`, inboxes and wake, the MCP tool list, the HTTP
  endpoint (401, 413, 429, 503, expiry, reopen after restart), hook output for each agent, parent-process sender
  resolution, setup and `--remove`, the outbox, and running with P2P off.
- CI (`.github/workflows/ci.yml`) runs fmt, clippy and the tests on Linux and Windows. macOS is tested locally.

## Known limits

- Claude Code delivery depends on `messagingSocketPath`, which is not a documented interface and may change.
- It is not confirmed whether Claude Code on Windows exposes a named pipe there.
- Gemini CLI, Qwen Code, Antigravity CLI and Copilot CLI support follows their documentation and is tested with fake
  hook input only.
- Codex runs nenwa's hooks only after the user trusts them in `/hooks`, and asks for approval on each tool call by
  default.
- Agents without a push path receive mail only when a hook runs.
- Until the agent process of a hook-wired session is known, the sender of an MCP call is a best guess.
- The frame text, tool descriptions, CLI output and log messages are in Japanese.
- No member removal. Rotating the team key means `init` and new invites.
- Messages to a closed session are not kept. Only messages to an offline machine are queued.
- Peer discovery uses n0's public DNS and relays. They cannot read content but can see which key uses which relay.
- The local socket trusts every process of the same OS user.
- The HTTP endpoint works only while the machine is awake, and needs Tailscale Funnel.
