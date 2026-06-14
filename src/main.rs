//! Org HUD — non-invasive in-game overlay client.
//!
//! Scaffold only. The overlay (winit + wgpu + egui, with a websocket + REST client speaking
//! `hud-protocol`) is built in Step 5 of the platform plan. See `docs/COMPLIANCE.md` for the
//! constraints this client is built within: no injection, no memory reads, no input
//! automation, no network interception of the game.

fn main() {
    println!("Org HUD scaffold — overlay arrives in Step 5. See docs/COMPLIANCE.md.");
}
