//! Best-effort Discord Rich Presence mirroring.
//!
//! The server privacy-gates and fully composes the `DiscordPresence` block server-side
//! (`hud_protocol::DiscordPresence`, protocol v14) and pushes it on every `OrgContext`; this module
//! only mirrors whatever it's told onto the local Discord client via classic Discord IPC
//! (`discord-rich-presence`). It never evaluates privacy prefs itself — it has no DB access to.
//!
//! EVERYTHING here is best-effort: a Discord IPC failure (Discord not running, no IPC pipe, a
//! transient write error, …) is logged and swallowed. Rich Presence must never break or block the
//! HUD's own function.

use discord_rich_presence::{
    activity::{Activity, Button},
    DiscordIpc, DiscordIpcClient,
};
use std::sync::Mutex;

/// Holds the lazily-connected IPC client behind a plain (non-async) mutex — every operation here is
/// synchronous, local IPC (a named pipe on Windows / a Unix domain socket elsewhere), so the lock is
/// never held across an `.await`.
#[derive(Default)]
pub struct RichPresence {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    client: Option<DiscordIpcClient>,
    /// The client_id the current `client` (if any) connected with — compared against each push so a
    /// changed client_id forces a reconnect instead of silently keeping the stale connection.
    client_id: Option<String>,
    /// Whether we believe Discord currently shows an active presence for us (so a repeated "not
    /// allowed" push doesn't call `clear_activity` every single time).
    active: bool,
    /// The last (details, state) actually pushed to Discord, so the per-apply logging only fires on
    /// a real change instead of spamming on every ~15s refresh with identical content.
    last_sent: Option<(String, String)>,
    /// Whether we've already logged the current "not showing" reason (so a steady stream of
    /// None/not-allowed pushes logs once, not every ~15s). Reset when a presence is shown.
    not_showing_logged: bool,
}

impl RichPresence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a freshly-pushed presence block. `None` or `allowed: false` clears any active presence;
    /// `Some` with `allowed: true` lazily (re)connects and sets the activity. Best-effort throughout —
    /// every IPC error is logged and swallowed, never propagated or panicked on.
    pub fn apply(&self, presence: Option<&hud_protocol::DiscordPresence>) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match presence {
            Some(dp) if dp.allowed => {
                inner.not_showing_logged = false;
                Self::apply_allowed(&mut inner, dp)
            }
            other => {
                // Make "nothing shows" observable: log WHY the HUD isn't showing a presence, once
                // per state (not every ~15s refresh). This is the common failure — the server sends
                // no allowed presence because the member hasn't opted in (presence.discord), has DND
                // on, or the server has no DISCORD_CLIENT_ID.
                if !inner.not_showing_logged {
                    let reason = match other {
                        Some(_) => "server marked presence NOT allowed (opt-in off, DND on, or no server DISCORD_CLIENT_ID)",
                        None => "server sent no presence block (member off-op with no presence, or an older server protocol)",
                    };
                    eprintln!("[rich-presence] not showing a presence: {reason}");
                    inner.not_showing_logged = true;
                }
                Self::clear(&mut inner);
            }
        }
    }

    fn apply_allowed(inner: &mut Inner, dp: &hud_protocol::DiscordPresence) {
        // A changed (or first-seen) client_id invalidates any existing connection.
        if inner.client_id.as_deref() != Some(dp.client_id.as_str()) {
            if let Some(mut old) = inner.client.take() {
                let _ = old.close();
            }
            inner.client_id = Some(dp.client_id.clone());
            inner.active = false;
        }

        if inner.client.is_none() {
            let mut client = DiscordIpcClient::new(&dp.client_id);
            match client.connect() {
                Ok(()) => {
                    eprintln!(
                        "[rich-presence] connected to Discord IPC (client_id={})",
                        dp.client_id
                    );
                    inner.client = Some(client);
                }
                Err(e) => {
                    eprintln!("[rich-presence] connect failed (Discord not running?): {e}");
                    return;
                }
            }
        }

        let Some(client) = inner.client.as_mut() else {
            return;
        };
        let (details, state) = activity_fields(dp);
        let mut activity = Activity::new().details(&details).state(&state);
        // Optional profile link button (visible to others viewing the profile). Only when the
        // server sent a non-empty label + url.
        if !dp.button_url.is_empty() && !dp.button_label.is_empty() {
            activity = activity.buttons(vec![Button::new(
                dp.button_label.as_str(),
                dp.button_url.as_str(),
            )]);
        }
        match client.set_activity(activity) {
            Ok(()) => {
                inner.active = true;
                // Log only on a real change, so a steady 15s refresh with identical content is quiet.
                // Seeing this line confirms the HUD received an allowed presence AND the IPC write
                // succeeded — so if nothing shows in Discord after this, it's a Discord-client
                // display setting ("Activity Privacy → display current activity"), not the HUD.
                let now = (details.clone(), state.clone());
                if inner.last_sent.as_ref() != Some(&now) {
                    eprintln!("[rich-presence] set activity: details={details:?} state={state:?}");
                    inner.last_sent = Some(now);
                }
            }
            Err(e) => {
                eprintln!("[rich-presence] set_activity failed: {e}");
                // Drop the client so the next apply() reconnects fresh rather than repeatedly
                // writing to a dead pipe.
                inner.client = None;
                inner.active = false;
                inner.last_sent = None;
            }
        }
    }

    fn clear(inner: &mut Inner) {
        if !inner.active {
            return;
        }
        eprintln!("[rich-presence] clearing presence (server sent no allowed presence)");
        if let Some(client) = inner.client.as_mut() {
            if let Err(e) = client.clear_activity() {
                eprintln!("[rich-presence] clear_activity failed: {e}");
            }
        }
        inner.active = false;
        inner.last_sent = None;
    }

    /// Best-effort cleanup on HUD shutdown: clear the activity and close the IPC connection so
    /// Discord doesn't keep showing a stale presence after the HUD process exits (Discord clears on
    /// IPC disconnect too, but an explicit clear is the documented-safe pattern for this class of
    /// crate — see the plan).
    pub fn shutdown(&self) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::clear(&mut inner);
        if let Some(mut client) = inner.client.take() {
            let _ = client.close();
        }
        inner.client_id = None;
    }
}

/// Pure mapping from a pushed `DiscordPresence` to the two text fields sent in the IPC activity
/// payload — split out so it's unit-testable without a live Discord client.
fn activity_fields(dp: &hud_protocol::DiscordPresence) -> (String, String) {
    (dp.details.clone(), dp.state.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dp(details: &str, state: &str) -> hud_protocol::DiscordPresence {
        hud_protocol::DiscordPresence {
            allowed: true,
            client_id: "123456789".to_string(),
            details: details.to_string(),
            state: state.to_string(),
            button_label: String::new(),
            button_url: String::new(),
        }
    }

    #[test]
    fn activity_fields_map_details_and_state_verbatim() {
        let presence = dp("Playing StarPlatform", "In an operation");
        let (details, state) = activity_fields(&presence);
        assert_eq!(details, "Playing StarPlatform");
        assert_eq!(state, "In an operation");
    }

    #[test]
    fn activity_fields_preserve_empty_state() {
        let presence = dp("Playing StarPlatform", "");
        let (details, state) = activity_fields(&presence);
        assert_eq!(details, "Playing StarPlatform");
        assert_eq!(state, "");
    }

    #[test]
    fn apply_none_on_a_fresh_client_is_a_no_op() {
        // No connection ever attempted (inner.active starts false) — must not panic or block.
        let rp = RichPresence::new();
        rp.apply(None);
    }

    #[test]
    fn apply_not_allowed_on_a_fresh_client_is_a_no_op() {
        let rp = RichPresence::new();
        let mut presence = dp("Playing StarPlatform", "Idle");
        presence.allowed = false;
        rp.apply(Some(&presence));
    }

    #[test]
    fn shutdown_on_a_fresh_client_is_a_no_op() {
        let rp = RichPresence::new();
        rp.shutdown();
    }
}
