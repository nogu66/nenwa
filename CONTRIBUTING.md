# Contributing

nenwa tries to stay small: one executable, no accounts, no server to run. A change that removes code or a step is
worth more than a new option. [DESIGN.md](DESIGN.md) explains how the pieces fit, and the README says how to build
and test.

- Open an issue first for anything larger than a fix, so we can agree on the shape before you write it.
- One change per pull request, saying what it changes and why.
- Before you push, run what CI runs: `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` and
  `cargo test --locked`. CI runs them on Linux and Windows; macOS is checked locally.
- Tests must never touch a real `~/.nenwa` or a real agent's config. The end-to-end tests point `NENWA_HOME`, `HOME`
  and the per-agent `NENWA_*_DIR` variables at a temporary folder; new tests do the same.
- Adding an agent means: a `setup_*` function in `src/setup.rs` (and its removal), a branch in `hook_event` in
  `src/daemon.rs` for the output that agent expects, a row in `nenwa doctor`, an end-to-end test with a fake of that
  agent, and the README in all three languages (`README.md` is the source; `README.ja.md` and `README.zh-CN.md`
  follow it). Say in the pull request whether you checked it against the real agent and which version.
- Dependencies are pinned to exact versions in `Cargo.toml`. Do not add one for something the standard library or an
  existing dependency already does.
- Do not change what peers send each other (`src/net.rs`, invite strings, the message frame) without saying so in the
  pull request: every machine in a team speaks it, and they are not all updated at once.
- By submitting a contribution you agree that it is licensed under the [MIT license](LICENSE), the same as the rest
  of the project.

Security issues go through private reporting instead: see [SECURITY.md](SECURITY.md).
