# Releasing Mimir

## Current automation

CI is paused for pushes and pull requests. When manually dispatched, it verifies Linux and macOS in isolated native jobs. A failed platform remains visible on its matrix row without cancelling the healthy platform or failing the aggregate CI workflow.

Releases also run only when manually dispatched. The release workflow creates a tagged commit with the next minor version in `Cargo.toml` and `Cargo.lock`. Each native target independently passes Clippy, tests, and a release build before uploading its archive and SHA-256 checksum to the GitHub Releases page.

With the manifest at `0.9.0` and the latest local release tag at `v0.2.0`, the next manual release starts at `v0.9.0`. A platform failure withholds only that platform's artifacts. The generated version commit is kept on the release tag instead of being pushed back to `main`. Windows verification and artifacts are temporarily disabled.

## Packages and versions

The publishable crates.io package is named `mimir-ai`; the library and installed executable remain `mimir`. The package has not been published yet. After its first publication, Rust users will be able to install it with:

```bash
cargo install mimir-ai
```

Releases are started manually from the GitHub Actions **Release** workflow. To start a new major release line, first set the package version to the next `<major>.0.0`; the workflow preserves that manual major version instead of incrementing it.

Confirm the package metadata before dispatching a release:

```bash
cargo metadata --no-deps --format-version 1
```

## Artifacts

Current release targets are:

- Linux x86_64: `x86_64-unknown-linux-gnu`
- macOS Apple Silicon: `aarch64-apple-darwin`
- macOS Intel: `x86_64-apple-darwin`

Each successful target publishes its native archive and SHA-256 checksum to [GitHub Releases](https://github.com/shivendrasoni/mimir/releases/latest).
