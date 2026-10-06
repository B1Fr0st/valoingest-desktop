# Valoingest Desktop

A Windows notification-area app that watches VALORANT replay downloads and uploads them to [Valoingest](https://valoingest.billowing-violet-1c47.workers.dev).

## Download and run

Download `valoingest-windows-x64.exe` from the [latest release](https://github.com/B1Fr0st/valoingest-desktop/releases/latest), save it in a permanent folder you can write to (for example `%LOCALAPPDATA%\Programs\Valoingest`), and run it. The app lives in the notification area; right-click its icon to sign in with Google and choose your upload and privacy settings. Windows may ask you to approve running this unsigned executable.

- Watches `%LOCALAPPDATA%\VALORANT\Saved\Demos`, waits for finished downloads, and skips duplicate replays.
- Replays already present on first launch are uploaded only when you choose **Upload earlier replays**.
- Choose publication, name/player ID redaction, notifications, and **Start with Windows** in the tray menu.
- Optional deletion moves successfully uploaded or processed replays to the Recycle Bin.
- Settings, upload history, and logs live in `%LOCALAPPDATA%\Valoingest`; login sessions are stored in Windows Credential Manager.

This repository contains the Windows desktop client only. The API and parsing service are maintained separately.

## Build and test

Install Rust and the Visual Studio C++ build tools on Windows, then:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
.\target\release\valoingest.exe
```

Set `VALOINGEST_API` at build time to use a different default API server.

## Automated releases

GitHub Actions checks formatting, runs Clippy and tests, and builds Windows x64 on pushes to `main`, pull requests, version tags, and manual workflow runs. Successful main/tag/manual builds publish a release for the version in `Cargo.toml` if that release does not already exist. Pull requests produce artifacts only.

To publish an update, increase `version` in `Cargo.toml`, run `cargo check` to update `Cargo.lock`, commit both, and push to `main`. Tags must match the manifest version (`v0.1.0`, for example). Each release contains the executable, a ZIP, and `SHA256SUMS`. Assets are attached to a draft before it is published.
