# oxide-util

- Ask before taking any action in this project: edits, builds, installs, commits, anything.
- Never push, tag or create GitHub releases. The owner does all final pushes and releases.
- Never commit, and don't ask whether to commit. The owner commits manually.
- After every code change, rebuild and install it on this PC so the owner can test before pushing:
  `cargo build --release`, then `wix build installer.wxs -d Version=<Cargo.toml version> -arch x64 -o target/oxide-util.msi`,
  close any running `oxide-util.exe`, then `msiexec /i target\oxide-util.msi /passive` (needs UAC), then launch
  `C:\Program Files\Oxide Util\oxide-util.exe`.
- Do not add `Co-Authored-By` or any other Claude attribution lines to commits or PRs.
