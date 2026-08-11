# Org HUD — desktop overlay

A transparent, always-on-top, click-through in-game overlay for the org platform. Two layers:

- **Public** (always on): the org's live identity card, fed by the unauthenticated `/ws`.
- **Member** (opt-in): after signing in with Discord, an interactive panel of internal live data
  — your recognition standing, upcoming ops, open proposals, and the treasury (if you hold
  `treasury.view`).

## Hotkeys

All three are served by one low-level keyboard hook, so they still fire under exclusive fullscreen.
None of them swallows the key — it also reaches the game.

The overlay has **three modes**, and one of the hotkeys is a separate show/hide axis:

| Default key | Mode | Clicks | Backdrop |
|-----|------|--------|----------|
| **Ctrl + Alt + X** | **Interact** — widgets come forward and can be dragged to move, resized, scaled or clicked (see below). Same as clicking the tray icon. | captured by the overlay | game visible under the theme tint + scrim (**Interact tint**, default 30%) |
| **Ctrl + Alt + C** | **Settings** — opens the settings panel. Same as the **⚙** button. | captured by the overlay | **none** — the game stays fully visible |
| **Ctrl + Alt + Z** | **Show / hide** the whole overlay. Sticky: auto-hide will not re-show it until you press it again or enter a mode. | pass straight through to the game | n/a |

A one-handed bottom-left cluster on ordinary letter keys: no scan-code special cases, no `Fn`
requirement, present on every keyboard, and Star Citizen binds almost nothing on Ctrl+Alt.

Escape hatches never depend on the hotkey: **Esc** returns to pass-through display from *either*
mode in one press, and so do the **Done ✕** button and every tray item.

Pressing a mode's own key again returns to pass-through display; pressing the *other* mode's key
switches straight to it (so **Ctrl+Alt+X** from settings closes the panel and brings the tint back,
but keeps interacting).
**Hiding always drops back to pass-through display**, so the overlay is never
capturing-the-mouse-but-invisible — a state in which Esc could never fire, because the window is not
focused.

### Rebinding

**Settings → Keys** shows the three live combos. Click one, press the combo you want. Rules, all
enforced in both the UI and Rust:

- **At least one of Ctrl or Alt.** A bare key — or Shift alone — would fire while you play or type.
- **No collisions.** Two actions on one combo would leave one of them permanently unreachable.
- **No AltGr** (see below), and no OEM punctuation (its virtual-key code is layout-dependent, so the
  label shown would be a US-layout guess).

While a capture is armed the hotkeys are suspended, so pressing the combo *describes* it instead of
*firing* it. **Reset keys to defaults** is on the same tab — a binding you can no longer produce is
always recoverable from there, and the **⚙** button and every tray item reach the panel with no
hotkey at all.

### Why AltGr can't be used, and why Ctrl+Alt means the *left-hand* pair

**Windows models AltGr as Ctrl+Alt.** On this machine's layout (`KLID 00000809`, UK), twelve
characters — `€ é á í ó ú ¦` and their capitals — are reported by `VkKeyScanExW` with *both* the Ctrl
and Alt shift bits set. At the hook level, pressing AltGr emits a **phantom left Ctrl** (with the
impossible scan code `0x21D`) before `VK_RMENU`, so at the instant an AltGr'd letter arrives,
`GetAsyncKeyState` says Ctrl *and* Alt are down.

A naive `ctrl && alt` test therefore fires on every AltGr character typed. The hook instead requires
`ctrl && VK_LMENU && !VK_RMENU`: `VK_RMENU` down vetoes every binding, because on an AltGr layout it
is indistinguishable from "the user is typing an accented character while holding Ctrl". The cost is
that a Ctrl+Alt combo only fires from the **left-hand** Ctrl and Alt. See `binding_matches` in
`src/main.rs` for the measured truth table.

All three toggles are also on the tray menu, which is the only route on macOS/Linux — the keyboard
hook is Windows-only and compiles to a no-op stub elsewhere. The tray labels deliberately do *not*
name a key: the menu is built once at launch, so a key-naming label would go stale on the first
rebind.

### Arranging widgets

In interact mode (and settings mode), every widget takes three gestures. Edges and corners do
**different** things:

| Gesture | Does |
|---------|------|
| **Drag the widget** | Move it. Its position is remembered and it drops out of the default column layout. |
| **Drag an edge** (top/bottom/left/right) | Resize the **box**; the contents reflow into it. Height stays automatic until you drag a top or bottom edge, so a widget you only widened still grows and shrinks with its own content. |
| **Drag a corner** | **Scale** the widget *and everything inside it* — text, buttons and artwork all shrink or grow together. The opposite corner stays put. This is the same value as that widget's **Size** slider under **Settings → Widgets** (0.70–1.50): drag a corner and the slider moves, move the slider and the widget scales. |

**Reset layout** (Settings footer) clears every remembered position *and* size; per-widget scales stay
where you set them, and are cleared by putting the sliders back to 1.00.

### Interact tint

**Settings → General → Interact tint** sets how strongly interact mode washes the game behind the
widgets: **0–60%, default 30%**, and it updates live as you drag so you can judge it against whatever
is on screen. `0%` leaves the game completely untouched. The colour is always the current theme's
accent (or your org's live accent colour) — the slider only moves the opacity. It applies to
**interact mode only**: settings mode never paints a backdrop, so the game is fully visible while you
are in here changing it.

This crate depends ONLY on `hud-protocol` from the workspace and reaches the server purely over the
network (HTTP + WebSocket), so it can be lifted into its own public repo unchanged.

## Architecture

| Concern | How |
|---------|-----|
| Window shell | Tauri v2 webview window: `transparent`, `decorations:false`, `always_on_top`, click-through via `set_ignore_cursor_events` |
| Public data | webview JS opens `wss?://<server>/ws`, renders `identity_changed` |
| Auth | `/auth/login?desktop=1` → Discord → callback mints a bearer token, returned in the URL **fragment** (never logged); captured by `on_navigation`, stored in the OS secret store (keyring) |
| Member data | **pulled** from the bearer-authed REST API via Rust `fetch` command — never broadcast; the permission-gated REST layer stays the only path to internal data |
| Privileged ops | all in Rust `#[tauri::command]`s, so the bundled frontend needs no Tauri ACL/capabilities |

## Configure

The server origin defaults to `http://localhost:8080`. Point it elsewhere with an env var:

```sh
HUD_SERVER=https://org.example.com   # http(s):// — the app derives ws(s):// for the live socket
```

## Run (dev)

```sh
cargo run -p hud                     # uses target/debug; no tauri-cli needed
HUD_SERVER=https://org.example.com cargo run -p hud
```

## Build an installer (release)

Producing installers needs the Tauri CLI (one-time):

```sh
cargo install tauri-cli --version "^2"
```

Then, from `crates/hud`:

```sh
cargo tauri build                    # Windows: NSIS .exe installer + standalone .exe
                                     # Linux:   .AppImage     macOS: .app/.dmg
```

Artifacts land under `target/release/bundle/`. Bundle targets and icons are configured in
`tauri.conf.json`.

## Notes

- A topmost overlay draws over **borderless/windowed-fullscreen** games, not *exclusive* fullscreen
  — run Star Citizen in borderless fullscreen (same constraint as Discord's overlay).
- Windows needs the WebView2 runtime (present on Windows 11). Linux needs `webkit2gtk`.
