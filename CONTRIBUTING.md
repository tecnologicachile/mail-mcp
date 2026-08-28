# Contributing to mail-mcp

Thanks for contributing — recent releases have shipped almost entirely
community fixes, and PRs here get reviewed and tested quickly.

## Before opening a PR

Run these locally; all four must be clean:

```bash
cargo build
cargo test
cargo fmt -- --check
cargo clippy --all-targets
```

Toolchain: Rust 1.88+ (the codebase uses edition 2024 with let-chains).

To exercise a change against a real server, point a `.env` at a test
mailbox (see `.env.example` and `docs/account-setup.md`) and run the binary
over stdio from any MCP client.

## What makes a PR easy to merge

- **One change per PR.** A fix plus an unrelated script or doc is two PRs.
- **Tests for logic changes.** Protocol behavior is best covered with a
  mock-server test (see the RFC 2971 `ID` tests in `src/imap.rs` for the
  pattern); pure helpers with plain unit tests.
- **Rebase on current `main`** before asking for review.
- **English** for code comments, docs, and release notes.
- Say **which provider/server you validated against** if the change is
  provider-specific (e.g. "tested against a real 126.com mailbox").

## Conventions

- Match the surrounding code's style and comment density; every public
  item carries a doc comment.
- Input structs live in `src/models.rs`, validation in `src/server.rs`,
  transport in `src/imap.rs` / `src/smtp.rs` / `src/graph.rs` / `src/ews.rs`.
- Never log or echo credentials; passwords ride in `SecretString`.
- New env vars: document them in `README.md`, `.env.example`, and the
  `--help` output in `src/main.rs`.

## Bugs and features

Open a GitHub issue. For bugs, include the provider (Gmail, Zoho, iCloud,
NetEase, Exchange…), the tool called, and the `issues` array from the tool
response if there is one. Do not include real credentials or message
contents.

## What to expect

Maintainers review within days, test your branch locally on top of
`main`, and credit you by name in the release notes. Merged fixes ship in
the next patch release.
