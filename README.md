# Valolysis Desktop

A Windows notification-area app that watches VALORANT replay downloads and uploads them to [Valolysis](https://valolysis.odinnichols.dev).

## Download and run

Download `valolysis-windows-x64.exe` from the [latest release](https://github.com/B1Fr0st/valolysis-desktop/releases/latest) and run it. Windows may ask you to approve running this unsigned executable. The first run installs the app for your Windows user in `%LOCALAPPDATA%\Programs\Valolysis`, adds **Valolysis** to the Start menu and to **Settings > Apps > Installed apps**, and starts the installed copy. No administrator rights are needed, and the downloaded file can be deleted afterwards. The app lives in the notification area; right-click its icon to sign in with Google and choose your upload and privacy settings.

To remove it, uninstall **Valolysis** from **Installed apps**. This closes the app and removes it, its Start menu entry, Start with Windows, and the saved sign-in. You choose whether to also delete settings and upload history; replays are never touched.

Running an older download never replaces a newer installed version. Copies previously saved elsewhere move into the install folder the next time they start, and Start with Windows follows them. To run without installing, start the executable with `--portable` or set `VALOLYSIS_PORTABLE=1`; debug builds always run in place.

- Watches `%LOCALAPPDATA%\VALORANT\Saved\Demos`, waits for finished downloads, and skips duplicate replays.
- Replays already present on first launch are uploaded only when you choose **Upload earlier replays**.
- Choose publication, name/player ID redaction, notifications, and **Start with Windows** in the tray menu.
- Optional deletion moves successfully uploaded or processed replays to the Recycle Bin.
- Settings, upload history, and logs live in `%LOCALAPPDATA%\Valolysis`; login sessions are stored in Windows Credential Manager.

This repository contains the Windows desktop client only. The API and parsing service are maintained separately.

## Automatic updates

The app checks this repository's latest stable GitHub release at each startup. The check and download run in the background, so the tray and uploads stay responsive. A newer Windows x64 executable is downloaded over HTTPS, checked against GitHub's SHA-256 digest and expected size, and checked for the correct executable architecture. Drafts, prereleases, older versions, and incomplete releases are ignored or rejected.

The upload engine finishes its current operation and saves its history before exiting. A separate helper waits for the app process to exit, replaces the executable at its original path, and restarts it. The original binary is backed up during replacement and restored if launching the new executable fails. Settings, credentials, replay files, and the **Start with Windows** path are preserved.

If GitHub is offline, rate limited, or the folder is protected, the current app keeps running and retries at its next startup. Update events and failures appear in `%LOCALAPPDATA%\Valolysis\valolysis.log`. A failed installation restarts the previous app without immediately retrying, to avoid a restart loop. Completed update staging files are removed after startup; failed-installation backups are retained for recovery.

## Build and test

Install Rust and the Visual Studio C++ build tools on Windows, then:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
.\target\release\valolysis.exe --portable
```

The unit tests cover version selection, rejected release metadata and downloads, replacement of locked files, and rollback after a failed restart. To also download and verify the current public release:

```powershell
cargo test --locked live_release_download_verifies -- --ignored
```

After a release build, `powershell -File scripts/Test-UpdateHelper.ps1` checks the real helper's parent-exit handshake, executable replacement, backup, and restart launch. After publishing an updater-enabled release, `powershell -File scripts/Test-StartupUpdate.ps1` builds an isolated older fixture and verifies that startup automatically installs the live GitHub release. These opt-in checks use scratch directories, an empty replay folder, and isolated settings; they do not stop an existing user app.

## Automated releases

GitHub Actions checks formatting, runs Clippy and tests, builds Windows x64, and verifies the update helper against the published baseline on pushes to `main`, pull requests, version tags, and manual workflow runs. Successful main/tag/manual builds publish a release for the version in `Cargo.toml` if that release does not already exist. Pull requests produce artifacts only. A retry resumes an interrupted draft release; published assets are never overwritten.

To publish an update, increase `version` in `Cargo.toml`, run `cargo check` to update `Cargo.lock`, commit both, and push to `main`. Tags must match the manifest version (`v0.1.0`, for example). Each release contains the executable, a ZIP, and `SHA256SUMS`. Assets are attached to a draft before it is published.
