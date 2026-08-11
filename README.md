# StarPlatform HUD

A non-invasive, in-game overlay for our Star Citizen org. It draws your org's own
information — events, status, recognition — in a transparent, click-through window you can
position over the game. It **never touches the game** (see the
[compliance statement](docs/COMPLIANCE.md)).

> This is the public client members install. The org platform that serves it lives in a
> separate private repository; this repo depends only on the published `hud-protocol` wire
> contract and gets no special access — it authenticates as any member does.

## Status

🚧 Early scaffold. The overlay (winit + wgpu + egui, websocket + REST client) is built in
**Step 5** of the platform plan. This repo currently holds the docs, licence, and CI so the
compliance and privacy promises are inspectable from day one.

## Install

_Coming with the first release._ Signed binaries will be published under
[Releases](../../releases) for Windows.

## Hotkeys

_Coming with the first release._

## Trust & transparency

- [Compliance statement](docs/COMPLIANCE.md) — what it does and does not do to the game.
- [Privacy](docs/PRIVACY.md) — what it sends and stores.
- [Contributing](CONTRIBUTING.md) — how to build and propose changes.
- Licensed under [MIT](LICENSE).
