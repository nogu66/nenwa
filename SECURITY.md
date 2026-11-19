# Security

Please do not open a public issue for a vulnerability. Report it privately through
[GitHub's private vulnerability reporting](https://github.com/nogu66/nenwa/security/advisories/new). You should get a
reply within a week. Fixes go into the next release, and the release notes credit you unless you ask otherwise.

Only the latest release is supported.

What counts: anything that lets someone outside your team read or send messages, impersonate a member or an inbox,
get past the key on the HTTP endpoint (`nenwa muse`), make the daemon run a command or write a file it should not, or
read the secrets nenwa keeps under `~/.nenwa`.

What does not: a member of your own team sending your agent a message you do not like. Everyone you invite can
message your agents, and an agent decides for itself what to do with a message; nenwa marks every message as coming
from another agent and not from you. Invite only people and machines you trust. The trust model is described in
[DESIGN.md](DESIGN.md).

When you report, leave out real invite strings, webhook keys and the contents of `~/.nenwa`.
