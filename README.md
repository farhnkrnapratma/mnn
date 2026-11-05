# mnn

Easily update all your packages with a single command and keep your system up to date.

`mnn` is a cross-platform Rust CLI for updating the package managers installed on the current machine. It runs as a regular user, detects available managers by default, and uses `sudo` only for Unix system-level managers. In terminals it shows a live status row per target with the current operation and the final success, skipped or failure state; progress output is hidden in non-interactive sessions and dry-runs.

## Status

Alpha (`v1.0.0-alpha1`). The crate is published as [`mnn-cli`](https://crates.io/crates/mnn-cli) because the `mnn` name on crates.io belongs to another project; the installed binary is still called `mnn`.

## Install

```sh
cargo install mnn-cli
```

From source:

```sh
cargo install --git https://github.com/farhnkrnapratma/mnn
```

Then `mnn --install` installs the executable and its completions, and `mnn --install-completions detect` installs completions for the shells it finds.

## Usage

```text
mnn                   # detect and update supported package managers
mnn --dry-run         # preview commands without running them
mnn --parallel        # run user-level managers concurrently
mnn --dry-run pip     # preview Python package updates
```

Supported targets: `apk`, `apt` (`debian` is an alias), `aur`, `brew`, `bun`, `choco`, `determinate`, `dnf`, `emerge`, `eopkg`, `flatpak`, `pacman`, `pip`, `pkg`, `pnpm`, `rpm-ostree`, `snap`, `soar`, `transactional-update`, `uv`, `winget`, `xbps`, `yum` and `zypper`. `auto` (the default) detects what is installed and `all` selects every known target. Missing managers are skipped. A failed manager makes the command exit nonzero.

Other options: `--silent`, `--verbose`, `--no-parallel`, `--disable-log`, `--gen-config`, and the install, update, reinstall, uninstall and purge maintenance actions. Shell completion scripts can be printed with `--print-completion bash|fish|zsh|nu`. See `mnn --help` for the full list.

## Configuration

The configuration defaults to `~/.config/mnn/config`; logs and a single-run lock are kept under the state directory. `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME` are honored on Unix-like systems. On Windows, `%APPDATA%` and `%LOCALAPPDATA%` provide the corresponding defaults.

## Repository layout

```
src/main.rs     the CLI: argument parsing, target detection and execution, progress rendering
resources/      shell completion (mnn.nu) and man page (mnn.1) embedded into the binary
```

## Development

Requirements: Rust (stable) and Cargo; [Bun](https://bun.sh) only for the commitlint tooling.

```sh
bun install                                   # commitlint and git hooks
cargo build --release
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
cargo deny check
```

Releases are cut with git-flow (`git-flow release start/finish`), tagged `vX.Y.Z`, and published with `cargo publish`.

## Contributing

This repository uses [git-flow-next](https://git-flow.sh/) (classic preset): `main` is release-only, work happens on `feature/*` and `bugfix/*` branches off `develop`. Commits are signed off (`git commit -s`) and follow [Conventional Commits](https://www.conventionalcommits.org), enforced by commitlint in a `commit-msg` hook and in CI. Install the hooks with `bash .githooks/install.sh` (`bun install` does it through the `prepare` script). CI runs on CircleCI: `reuse lint`, format and lint, tests, `cargo deny`, a TruffleHog secret scan and commitlint on every push. The branch rules are written out in [`CLAUDE.md`](CLAUDE.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT License ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

Copyright © 2026 Farhan Kurnia Pratama
