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
PR does not add an auto-updater or release workflow. To validate the packaged
app layout without creating an installer:

```sh
npm run package:electron:dir
```

Packaging only wraps the remote URL; it does not bundle `packages/web/dist`.
A frontend deployment is picked up on the next desktop launch, or when an
existing window returns to the foreground after five minutes hidden. That
refresh revalidates the document and ignores the HTTP cache without clearing
localStorage or service-worker state, so reinstalling the Electron binary is
not normally required for frontend updates.
