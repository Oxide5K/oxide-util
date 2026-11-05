# oxide-util
Software used to check for updates across your PC without having to manually check everything.

A Rust (egui) desktop app that asks **winget** which installed apps have updates, and lets you update them one at a time or all at once.

## Build

```
cargo run --release
```

## Releasing

Bump `version` in `Cargo.toml`, commit, then tag and push:

```
git tag v0.1.0
git push origin v0.1.0
```

GitHub Actions builds `oxide-util.exe` and attaches it to a new GitHub Release.
