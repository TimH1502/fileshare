# Contributing to fileshare

Thanks for your interest! Contributions are welcome — bug fixes, features, documentation improvements, or anything else that makes the tool better.

## Getting started

```bash
git clone https://github.com/TimH1502/fileshare
cd fileshare
cargo build
cargo run
```

Rust stable is sufficient. No nightly features are used.

## How to contribute

1. **Open an issue first** for anything non-trivial — a quick discussion before writing code saves time if the direction needs adjusting.
2. Fork the repo, create a branch (`git checkout -b my-feature`).
3. Make your changes. Keep commits focused and the diff readable.
4. Open a pull request against `main`. Describe what changed and why.

## Code style

- `cargo fmt` before committing (standard rustfmt, no custom config)
- `cargo clippy` should pass without warnings
- No unsafe code

## Areas where help is especially welcome

- **Windows testing** — drag & drop path detection and terminal behaviour vary across Windows versions
- **mDNS edge cases** — corporate networks, VPNs, Docker bridge interfaces
- **Themes** — new colour schemes are easy to add (see `THEMES` in `src/tui/app.rs`)
- **Compression** — the parallel zip approach has room for further tuning
- **Tests** — coverage is thin; unit tests for `shares.rs` and `client.rs` would be valuable

## Security issues

Please don't open public issues for security vulnerabilities. Open a [GitHub Security Advisory](https://github.com/TimH1502/fileshare/security/advisories/new) instead.

## License

By contributing you agree that your changes will be licensed under the [Apache 2.0 License](LICENSE).
