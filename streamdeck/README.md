# StarPlatform HUD — Stream Deck plugin

Run a live op from an Elgato Stream Deck: check in, advance objectives, control the music bed, fire
soundboard clips.

## How it actually fits together

There is **no "point the Stream Deck at my app" API**, and that shapes everything here. A plugin is a
separate Node.js process that the Stream Deck app launches and supervises, and the WebSocket it
speaks is a private plugin↔Stream Deck channel — not a bus other applications can join. So the
hardware cannot reach the HUD; the HUD has to offer somewhere to be reached.

```
Stream Deck app  <--ws-->  this plugin  <--ws-->  StarPlatform HUD local API  --> member socket --> server
                                                  (127.0.0.1:48291)
```

**The plugin holds no org credential.** The pairing key authorises reaching *the HUD on this
machine* and nothing else. Every action is relayed over the HUD's own member socket, so it lands on
exactly the same server-side permission gate the on-screen controls do. A plugin that authenticated
directly to the server would be a second credential sitting in a user-readable Node process.

## Setup

1. **In the HUD:** Settings → tick **Local control API**, then restart the HUD. (It is off by
   default because it opens a listening socket, and it is read once at startup — a security-relevant
   listener appearing mid-session on a settings save would be a surprising thing to happen.)
2. Copy the **pairing key** shown there.
3. **Install the plugin** — double-click `com.thecodesaiyan.starplatformhud.streamDeckPlugin`, or for
   development: `streamdeck link streamdeck/com.thecodesaiyan.starplatformhud.sdPlugin`
4. Drag any StarPlatform HUD action onto a key, open its property inspector, and paste the pairing key. It is
   stored once globally, so you only do this for the first key.

To revoke: HUD → Settings → **Revoke**. That invalidates the key for **new** connections. A tool
already connected keeps its session until its socket drops or the HUD restarts — the token is checked
at the handshake, not per message. Restart the HUD if you need a tool cut off now.

## The keys

Two of these need **no** leadership, which is the point — a deck is worth owning even if you never
lead an op.

| Action | Needs | Notes |
|---|---|---|
| Check In | a joinable live op | not lead-gated. Greys once you are on the op |
| **Quick Action** | **any member on the op** | posts a call ("Downed", "En route") into the ready room, attributed to you. Pick which in the property inspector |
| **Member Clip** | **any member on the op** | plays a clip into the ready room. Greys when the lead has clips off |
| Ready Toggle | to be checked in | two-state key reflects your current state |
| Objective Next / Previous | **lead** | title shows `current/count`; the rail is `0..=count`, where `count` is the all-complete sentinel |
| Bed Pause/Resume | **lead** + an audible bed | two-state |
| Bed Next / Previous Track | **lead** + an audible bed | |
| Soundboard Clip | **lead** | the picker lists only clips you may actually fire |
| Complete Op | **lead** | **press twice within 5s.** Irreversible |
| Bed Volume (dial) | **lead** + an audible bed | turn to set, push to pause; touch strip shows now-playing |

**Complete Op is confirmed on both sides deliberately.** The plugin arms on the first press and
fires on the second, *and* the HUD refuses the action without `confirm: true`. Either alone is a
single stray press away from ending someone's op, and the plugin is a process the HUD does not
control.

## Why a key might be dark

A failed permission check on the server is a bare refusal with no reply — survivable on screen,
where lead-only controls simply are not drawn, but on hardware it is a key that lights up and does
nothing. So the HUD **pushes a state board** and the plugin greys keys it knows will fail; pressing
one anyway shows an alert with a specific reason (`not_lead`, `no_bed`, `no_live_op`,
`needs_confirm`, …) rather than silence.

If every key is dark: the HUD is not running, the local API is off, or the pairing key is wrong.

## Development

**No build step and no dependencies.** `bin/plugin.js` is the entire plugin — Node 22+ ships a global
`WebSocket`, and the manifest pins Node 24, so the usual `@elgato/streamdeck` + TypeScript + npm +
rollup toolchain is not needed. Nothing to install, nothing to compile, no lockfile to keep in step
with the Rust side.

Drift is guarded **from Rust**, in `crates/hud/src/local_api.rs`, because CI runs `cargo` and not
`node` — a guard the build never executes is decoration. Those tests fail if the plugin sends an
action the HUD does not know, if the subprotocol or default port drift apart, if a manifest action
loses its handler, or if the confirm-on-complete rule is weakened on either side.

Package for distribution with `streamdeck pack com.thecodesaiyan.starplatformhud.sdPlugin`. Marketplace
publishing is a separate, later decision — Elgato explicitly allows distributing the
`.streamDeckPlugin` file directly.

## 🔴 A newly linked plugin needs a FULL app restart

`streamdeck restart <uuid>` only restarts an **already-loaded** plugin — it does not make Stream Deck
rescan the plugins folder, and it reports success either way. After a first `streamdeck link`, quit
Stream Deck completely and relaunch it, or the plugin silently never loads and nothing is logged.

## Verified on real hardware (2026-08-02)

- **Stream Deck loads a plain-JS plugin under `SDKVersion: 3`.** No `@elgato/streamdeck`, no npm
  project, no build step. This was the design's biggest open question and the answer is yes.
- **Stream Deck fetches the pinned runtime itself.** With no Node 24 present it logged
  `Node.js version 24 is not available. Fetching...` → `Successfully fetched Node.js 24`, then ran
  `NodeJS.13.1
ode.exe … bin/plugin.js -port … -pluginUUID … -registerEvent registerPlugin`.
  Nothing needs installing by hand.
- **The registration handshake works** — `[com.thecodesaiyan.starplatformhud] Plugin connected`.

## Still not verified

- Key behaviour against a live op: whether keys grey correctly, and whether refusals surface as
  alerts with the right reason.
- Dial/touch-strip behaviour (`$B1` layout, `setFeedback` payload shape).
- Whether Elgato requires any signing or notarization of plugin binaries — the distribution docs
  cover packaging, DRM and review, and say nothing about signing.
- The icons in `imgs/` are generated placeholders, not designed art.
