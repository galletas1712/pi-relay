# π-relay desktop

This package is a thin Electron shell around the deployed web application. It
does not bundle the frontend and has no backend, database, workspace, or
session-format code. The default frontend is `https://pi-relay.pages.dev`.
Packaging and supported desktop use are currently macOS-only.

## Prerequisites

- Node.js 22.12 or newer
- npm dependencies installed from the repository root (`npm ci`)
- macOS with Electron's native prerequisites

## Local development

Run the deployed app:

```sh
npm run dev:electron
```

Point the shell at a local Vite server or another deployment:

```sh
PI_RELAY_WEB_URL=http://127.0.0.1:8788 npm run dev:electron
```

The override must be an HTTP(S) URL. Same-origin navigation stays in the
window. External HTTP(S) links, including OAuth links opened in a new window,
are sent to the operating system browser; same-origin popups and other URL
schemes are denied.

## Packaging

Build the macOS installers:

```sh
npm run package:electron
```

The configured production targets are macOS DMG and ZIP. The ZIP is retained
as the macOS distribution artifact needed for a future auto-update flow; this
workflow does not publish the ZIP, add an auto-updater, or sign the app. To
validate the packaged app layout without creating an installer:

```sh
npm run package:electron:dir
```

Packaging only wraps the remote URL; it does not bundle `packages/web/dist`.
A frontend deployment is picked up on the next desktop launch. While the app
stays open, the web UI soft-reconciles session data when the window returns to
the foreground (same path as the browser/PWA). After display sleep or a failed
load, the shell invalidates the compositor or retries a normal `loadURL` so a
blank window is not left as the terminal state. Reinstalling the Electron
binary is not normally required for frontend updates.

## GitHub Release distribution

The [`Electron macOS release`](../../.github/workflows/electron-macos-artifact.yml)
workflow builds a universal DMG on a GitHub-hosted macOS runner and publishes
it to the rolling [`electron-latest` GitHub Release](https://github.com/galletas1712/pi-relay/releases/tag/electron-latest).
It runs automatically after a push to `main` changes any of the following:

- `.github/workflows/electron-macos-artifact.yml`
- `packages/electron/**`
- `packages/web/public/icons/icon-512.png`
- `package.json` or `package-lock.json`

Frontend-only changes outside the icon path and Rust-only changes do not start
this workflow.

To download the latest build:

1. Open the repository's **Releases** page.
2. Open **π-relay macOS (latest)**.
3. Download `pi-relay-macos.dmg` and, optionally, verify it with
   `pi-relay-macos.dmg.sha256`.
   From the directory containing both files, run
   `shasum -a 256 -c pi-relay-macos.dmg.sha256`.

This is a GitHub Release, not GitHub Packages. Each successful build replaces
the two fixed-name assets in that release and moves its rolling tag to the
build's commit. Unexpected old assets are removed only after both new assets
are uploaded; the fixed-name assets are replaced with `--clobber` during that
upload. The workflow does not create a new Actions artifact; it also cleans up
legacy Actions artifacts named `electron-macos-dmg` after a successful release
upload.

To build on demand from any selected ref:

1. Open the repository's **Actions** tab.
2. Select **Electron macOS release**.
3. Select **Run workflow**, choose the ref, and select **Run workflow** again.
4. After the run succeeds, open **Releases** and download the fixed-name DMG
   and checksum from **π-relay macOS (latest)**.

The DMG is not signed or notarized. macOS Gatekeeper will likely warn that the
developer cannot be verified and may quarantine or block the app on first
launch. Use Finder's **Open** action or approve the app under **System
Settings → Privacy & Security** if macOS offers that option. This workflow
does not configure auto-updating or provide Apple signing/notarization.
