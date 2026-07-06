# Contributing

Patches, bug reports, and keeper feedback are all welcome. This is a small project; nothing here is heavy process.

## Getting the tree building

```sh
git clone https://github.com/bakemonoq/bakemono
cd bakemono
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

`bakemono-core` and `bakemono-scraper` test without any services. The board's own tests are DB-free too; to actually run `serve` you need postgres at `DATABASE_URL` and a local Kubo (the compose file brings both up).

## Before you open a PR

- `cargo test --workspace` and `cargo clippy --workspace --all-targets` are green (CI runs both).
- The change is focused; unrelated cleanups go in their own PR.
- User-facing behaviour changes come with a docs update in the same PR.

## House style

- Function ordering follows the stepdown rule: callers above the helpers they call, so a file reads top to bottom.
- Comments are the exception, not the rule. When one earns its place, keep it to a single line on a non-obvious "why"; skip module/file headers that just list what the module contains.
- Match the surrounding code's naming and idiom rather than introducing a new one.

## Reporting bugs

Open an issue with what you did, what happened, and what you expected. For anything security-sensitive, do not open a public issue - see [SECURITY.md](SECURITY.md)
