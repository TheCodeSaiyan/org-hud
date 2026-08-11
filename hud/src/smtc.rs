//! Windows media-key control of the op's music bed (roadmap **C11**), via the System Media
//! Transport Controls.
//!
//! # 🔴 The HUD is not a media player
//!
//! The bed is not rendered on this machine at all. `voice-worker` plays `voice_asset` bytes into a
//! **Discord voice channel** and the operator hears it through Discord; the HUD only sends
//! declarative intents over `/ws/member` and the server writes desired state to `voice_assignment`.
//! So everything here bolts a Windows media-session *façade* onto a **remote** transport. The OS's
//! assumptions — that this process produces audio, that pausing silences something local — are
//! false, and that mismatch is what decided the mechanism:
//!
//! - **SMTC, not a `WH_KEYBOARD_LL` hook on `VK_MEDIA_*`.** The hook route is a smaller diff (the
//!   HUD already owns such a hook) but it is global and binary: swallow the key and Spotify never
//!   sees it, with no OS-level UI explaining why the user's music stopped responding; chain it and
//!   *both* react to every press. There is no third behaviour. It would also mean deliberately
//!   breaking the never-swallow invariant that `keyboard_hook_proc` is pinned to by a source test.
//! - **SMTC is cooperative.** Windows is explicitly multi-session and arbitrates which player a
//!   media key reaches, so registering here does not seize the keys from Spotify.
//!
//! # Why the session is lead-scoped and default-off
//!
//! The transport messages are lead-gated server-side (`live_op_led_by`), and a failed gate is a
//! bare `return false` with **no reply**. So a non-lead pressing a media key would get silence from
//! the bed *and*, if the HUD happened to hold the current session, silence from Spotify — with
//! nothing on screen explaining either. Registering only while the member **leads a live op with an
//! audible bed** makes that state unreachable, and keeps the machine from advertising a media
//! session for audio it is not producing any longer than necessary.
//!
//! # Split
//!
//! [`plan`] and [`message_for`] are pure and unit-tested on every platform — they are called
//! unconditionally from the context handler so they stay live on non-Windows CI (a bin crate gets
//! no liveness from tests, so anything reachable only from `#[cfg(windows)]` code is dead there).
//! [`Smtc`] is the OS edge: real on Windows, a no-op elsewhere. Mirrors `rich_presence.rs`.

use hud_protocol::{ClientMessage, OpContext};

/// What the OS-facing media session should look like right now.
///
/// `Absent` is not "do nothing" — it means *tear the session down if one exists*, which is how the
/// lead-scoped guarantee above is enforced on every context push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPlan {
    Absent,
    Present(SessionMeta),
}

/// What the flyout should show. Everything here comes from `MusicState`, which the HUD already
/// receives — C11 needs no new server data and no `hud-protocol` bump.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionMeta {
    pub title: String,
    pub artist: String,
    /// Mirrors `MusicState.paused` — the state the server and worker agree on, not an optimistic
    /// local guess. It can lag by up to one context push; that is honest, because the HUD does not
    /// otherwise know what the worker is doing. An optimistic value would drift permanently on any
    /// dropped intent.
    pub paused: bool,
    pub duration_ms: Option<i32>,
    pub elapsed_ms: Option<i64>,
}

/// Shown when a bed is audible but its track metadata has not arrived. A session with an empty
/// title renders as a blank row in the flyout, which reads as broken rather than as loading.
const UNKNOWN_TITLE: &str = "Music bed";

/// Decide the session from the pushed context.
///
/// Every arm that returns [`SessionPlan::Absent`] is load-bearing; see the module docs for why the
/// lead and audible-bed conditions in particular are not merely cosmetic.
pub fn plan(enabled: bool, ctx: Option<&OpContext>) -> SessionPlan {
    if !enabled {
        return SessionPlan::Absent;
    }
    let Some(ctx) = ctx else {
        return SessionPlan::Absent;
    };
    // Not the lead ⇒ every transport message would be silently refused server-side.
    if !ctx.is_lead {
        return SessionPlan::Absent;
    }
    let Some(music) = ctx.music.as_ref() else {
        return SessionPlan::Absent;
    };
    // `active` is "a bed is set AND a pool bot is claimed" — i.e. actually audible. A session for
    // an inaudible bed would put controls in the flyout that change nothing anyone can hear.
    if !music.active {
        return SessionPlan::Absent;
    }

    let track = music.now_playing.as_ref();
    let title = track
        .and_then(|t| t.title.clone())
        .or_else(|| music.bed.clone())
        .unwrap_or_else(|| UNKNOWN_TITLE.to_string());
    let artist = track.and_then(|t| t.artist.clone()).unwrap_or_default();

    SessionPlan::Present(SessionMeta {
        title,
        artist,
        paused: music.paused,
        duration_ms: track.and_then(|t| t.duration_ms),
        elapsed_ms: music.elapsed_ms,
    })
}

/// WinRT `TimeSpan` counts 100-nanosecond ticks.
const TICKS_PER_MS: i64 = 10_000;

/// Flyout scrub position as `(end, position)` in ticks, or `None` when there is nothing sensible
/// to draw.
///
/// 🔴 **The bed LOOPS**, so `elapsed_ms` is time since the track started and can exceed the track
/// length. Feeding that straight to `SetPosition` parks the scrubber past the end of the bar and it
/// stays there. Wrapping is what `MusicState.elapsed_ms` documents the HUD's own progress bar
/// doing, so the flyout matches the overlay instead of disagreeing with it.
/// Only the Windows edge consumes this, and a bin crate gets no liveness from tests — so on the
/// ubuntu CI runner it is genuinely dead. Scope the allow to the platform where that is true, so
/// the lint stays strict on Windows where deadness would be a real defect.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn timeline(duration_ms: Option<i32>, elapsed_ms: Option<i64>) -> Option<(i64, i64)> {
    let duration = i64::from(duration_ms?);
    // A zero/negative length is "unknown", not "a zero-length track" — and it would divide by zero.
    if duration <= 0 {
        return None;
    }
    let elapsed = elapsed_ms.unwrap_or(0).max(0);
    Some((duration * TICKS_PER_MS, (elapsed % duration) * TICKS_PER_MS))
}

/// The buttons this session accepts. Deliberately not the full SMTC set:
///
/// - **No `Stop`.** The only thing it could mean is "release the op", which is destructive and
///   would sit behind an unlabelled hardware key.
/// - **No volume.** SMTC has no volume control, so `SetBedVolume` would have to ride a media key
///   the OS routes elsewhere entirely.
///
/// The variants are CONSTRUCTED only by the Windows edge (`imp::button_of`) — `message_for` merely
/// matches on them, which dead-code analysis counts as a read, not a construction. So on the ubuntu
/// CI runner they are genuinely dead, and `-D warnings` says so; it is invisible on a Windows dev
/// box, where the code is live. Scope the allow to the platform where the deadness is real.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Play,
    Pause,
    Next,
    Previous,
}

/// Map a button to the intent the on-screen transport already sends.
///
/// 🔴 **No authority change.** These are the same `ClientMessage`s the HUD's own transport emits,
/// all lead-gated server-side. A media key is a new *input device*, not a new *permission*.
pub fn message_for(button: Button) -> ClientMessage {
    match button {
        Button::Play => ClientMessage::SetBedPaused { paused: false },
        Button::Pause => ClientMessage::SetBedPaused { paused: true },
        Button::Next => ClientMessage::TransportNext,
        Button::Previous => ClientMessage::TransportPrev,
    }
}

/// The OS edge. Real on Windows; a no-op elsewhere so the context handler can call it
/// unconditionally and the pure half above stays live on non-Windows CI.
#[derive(Default)]
pub struct Smtc {
    #[cfg(windows)]
    inner: std::sync::Mutex<imp::Inner>,
}

impl Smtc {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile the OS session against `plan`. `hwnd` must be a **top-level window owned by this
    /// process** — the interop rejects anything else.
    ///
    /// `on_button` is taken here rather than at construction because `AppState` is built before the
    /// Tauri app handle exists, and the handler needs one to send. It is installed **once**, on the
    /// first session created; later calls pass a closure that is simply dropped. Keeping it a
    /// parameter is also what lets this module stay free of any Tauri dependency.
    pub fn apply(
        &self,
        hwnd: isize,
        plan: &SessionPlan,
        on_button: impl Fn(Button) + Send + Sync + 'static,
    ) {
        #[cfg(windows)]
        imp::apply(&self.inner, hwnd, plan, on_button);
        #[cfg(not(windows))]
        let _ = (hwnd, plan, on_button);
    }
}

#[cfg(windows)]
mod imp {
    use super::{Button, SessionMeta, SessionPlan};
    use std::sync::{Arc, Mutex};
    use windows::Foundation::{TimeSpan, TypedEventHandler};
    use windows::Media::{
        MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsTimelineProperties,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;

    /// Live session state. `controls` is created once per HWND and then enabled/disabled — the
    /// interop has no "destroy", so tearing a session down means `SetIsEnabled(false)`, which is
    /// also what avoids flyout flicker when an op ends and another begins.
    #[derive(Default)]
    pub(super) struct Inner {
        controls: Option<SystemMediaTransportControls>,
        last: Option<SessionMeta>,
    }

    /// Attach to a top-level window owned by this process, as
    /// `ISystemMediaTransportControlsInterop::GetForWindow` requires. (`GetForCurrentView` is the
    /// UWP-view path and is not usable from an unpackaged desktop app.)
    fn make_controls(hwnd: isize) -> windows::core::Result<SystemMediaTransportControls> {
        let interop: ISystemMediaTransportControlsInterop = windows::core::factory::<
            SystemMediaTransportControls,
            ISystemMediaTransportControlsInterop,
        >()?;
        unsafe { interop.GetForWindow(HWND(hwnd as *mut core::ffi::c_void)) }
    }

    fn button_of(raw: SystemMediaTransportControlsButton) -> Option<Button> {
        match raw {
            SystemMediaTransportControlsButton::Play => Some(Button::Play),
            SystemMediaTransportControlsButton::Pause => Some(Button::Pause),
            SystemMediaTransportControlsButton::Next => Some(Button::Next),
            SystemMediaTransportControlsButton::Previous => Some(Button::Previous),
            // Stop/Record/FastForward/Rewind are never enabled; ignore rather than guess an intent.
            _ => None,
        }
    }

    /// Reconcile the OS session against `plan`. Best-effort throughout: every failure is logged
    /// and swallowed, because a media-key convenience must never take the overlay down with it.
    pub(super) fn apply(
        inner: &Mutex<Inner>,
        hwnd: isize,
        plan: &SessionPlan,
        on_button: impl Fn(Button) + Send + Sync + 'static,
    ) {
        let Ok(mut guard) = inner.lock() else {
            return;
        };
        match plan {
            SessionPlan::Absent => {
                if let Some(controls) = guard.controls.as_ref() {
                    if let Err(e) = teardown(controls) {
                        eprintln!("smtc: teardown failed: {e}");
                    }
                }
                guard.last = None;
            }
            SessionPlan::Present(meta) => {
                if guard.controls.is_none() {
                    match make_controls(hwnd) {
                        Ok(controls) => {
                            if let Err(e) = install(&controls, Arc::new(on_button)) {
                                eprintln!("smtc: button handler failed to install: {e}");
                                return;
                            }
                            guard.controls = Some(controls);
                        }
                        Err(e) => {
                            eprintln!("smtc: GetForWindow failed: {e}");
                            return;
                        }
                    }
                }
                // Skip a redundant push: `Update()` refreshes the flyout, and pushing an unchanged
                // payload on every 15s context tick makes it flicker for no reason.
                if guard.last.as_ref() == Some(meta) {
                    return;
                }
                if let Some(controls) = guard.controls.as_ref() {
                    if let Err(e) = push(controls, meta) {
                        eprintln!("smtc: update failed: {e}");
                        return;
                    }
                    guard.last = Some(meta.clone());
                }
            }
        }
    }

    /// Wire the button handler once, the first time a session is created.
    pub(super) fn install(
        controls: &SystemMediaTransportControls,
        on_button: Arc<dyn Fn(Button) + Send + Sync + 'static>,
    ) -> windows::core::Result<()> {
        controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            if let Some(args) = args.as_ref() {
                if let Ok(raw) = args.Button() {
                    if let Some(b) = button_of(raw) {
                        on_button(b);
                    }
                }
            }
            Ok(())
        }))?;
        Ok(())
    }

    pub(super) fn push(
        controls: &SystemMediaTransportControls,
        meta: &SessionMeta,
    ) -> windows::core::Result<()> {
        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsNextEnabled(true)?;
        controls.SetIsPreviousEnabled(true)?;
        controls.SetPlaybackStatus(if meta.paused {
            MediaPlaybackStatus::Paused
        } else {
            MediaPlaybackStatus::Playing
        })?;

        if let Some((end, position)) = super::timeline(meta.duration_ms, meta.elapsed_ms) {
            let tl = SystemMediaTransportControlsTimelineProperties::new()?;
            tl.SetStartTime(TimeSpan { Duration: 0 })?;
            tl.SetMinSeekTime(TimeSpan { Duration: 0 })?;
            tl.SetEndTime(TimeSpan { Duration: end })?;
            tl.SetMaxSeekTime(TimeSpan { Duration: end })?;
            tl.SetPosition(TimeSpan { Duration: position })?;
            controls.UpdateTimelineProperties(&tl)?;
        }

        let updater = controls.DisplayUpdater()?;
        updater.SetType(MediaPlaybackType::Music)?;
        let music = updater.MusicProperties()?;
        music.SetTitle(&windows::core::HSTRING::from(meta.title.as_str()))?;
        music.SetArtist(&windows::core::HSTRING::from(meta.artist.as_str()))?;
        updater.Update()?;
        Ok(())
    }

    pub(super) fn teardown(controls: &SystemMediaTransportControls) -> windows::core::Result<()> {
        controls.SetIsEnabled(false)?;
        let updater = controls.DisplayUpdater()?;
        updater.ClearAll()?;
        updater.Update()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hud_protocol::{MusicState, TrackMeta};

    fn music(active: bool) -> MusicState {
        MusicState {
            bed: Some("bed/combat".into()),
            volume: 60,
            paused: false,
            active,
            tags: vec![],
            stage_tags: vec![],
            now_playing: None,
            playlist: None,
            elapsed_ms: None,
        }
    }

    fn ctx(is_lead: bool, music: Option<MusicState>) -> OpContext {
        OpContext {
            is_lead,
            music,
            ..Default::default()
        }
    }

    #[test]
    fn disabled_never_registers_a_session() {
        // The whole feature is default-off; the setting must dominate every other condition.
        let c = ctx(true, Some(music(true)));
        assert_eq!(plan(false, Some(&c)), SessionPlan::Absent);
    }

    #[test]
    fn no_context_is_absent() {
        assert_eq!(plan(true, None), SessionPlan::Absent);
    }

    #[test]
    fn a_non_lead_never_registers_a_session() {
        // Load-bearing: transport messages are lead-gated server-side and a failed gate replies
        // with nothing, so a non-lead's media key would silently do nothing to the bed AND
        // (if this session were current) nothing to Spotify.
        let c = ctx(false, Some(music(true)));
        assert_eq!(plan(true, Some(&c)), SessionPlan::Absent);
    }

    #[test]
    fn a_lead_with_no_music_block_is_absent() {
        let c = ctx(true, None);
        assert_eq!(plan(true, Some(&c)), SessionPlan::Absent);
    }

    #[test]
    fn an_inaudible_bed_never_registers_a_session() {
        // `active` false = bed set but no pool bot claimed. Controls that change nothing anyone
        // can hear are worse than no controls.
        let c = ctx(true, Some(music(false)));
        assert_eq!(plan(true, Some(&c)), SessionPlan::Absent);
    }

    #[test]
    fn a_lead_with_an_audible_bed_registers() {
        let c = ctx(true, Some(music(true)));
        match plan(true, Some(&c)) {
            SessionPlan::Present(m) => {
                // No track metadata yet: fall back to the bed key, never an empty title.
                assert_eq!(m.title, "bed/combat");
                assert_eq!(m.artist, "");
                assert!(!m.paused);
            }
            other => panic!("expected a session, got {other:?}"),
        }
    }

    #[test]
    fn track_metadata_populates_the_flyout() {
        let mut ms = music(true);
        ms.now_playing = Some(TrackMeta {
            key: "bed/combat/01".into(),
            title: Some("Hostile Approach".into()),
            artist: Some("Tatux".into()),
            genre: None,
            duration_ms: Some(184_000),
        });
        ms.elapsed_ms = Some(12_000);
        ms.paused = true;
        let c = ctx(true, Some(ms));
        assert_eq!(
            plan(true, Some(&c)),
            SessionPlan::Present(SessionMeta {
                title: "Hostile Approach".into(),
                artist: "Tatux".into(),
                paused: true,
                duration_ms: Some(184_000),
                elapsed_ms: Some(12_000),
            })
        );
    }

    #[test]
    fn a_titleless_track_falls_back_rather_than_showing_a_blank_row() {
        let mut ms = music(true);
        ms.bed = None;
        ms.now_playing = Some(TrackMeta {
            key: "k".into(),
            title: None,
            artist: None,
            genre: None,
            duration_ms: None,
        });
        let c = ctx(true, Some(ms));
        match plan(true, Some(&c)) {
            SessionPlan::Present(m) => assert_eq!(m.title, UNKNOWN_TITLE),
            other => panic!("expected a session, got {other:?}"),
        }
    }

    #[test]
    fn paused_state_mirrors_the_pushed_music_state() {
        let mut ms = music(true);
        ms.paused = true;
        let c = ctx(true, Some(ms));
        match plan(true, Some(&c)) {
            SessionPlan::Present(m) => assert!(m.paused),
            other => panic!("expected a session, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_track_length_draws_no_scrubber() {
        assert_eq!(timeline(None, Some(5_000)), None);
        // Zero is "unknown", not a zero-length track — and it would divide by zero.
        assert_eq!(timeline(Some(0), Some(5_000)), None);
        assert_eq!(timeline(Some(-1), Some(5_000)), None);
    }

    #[test]
    fn a_looping_bed_wraps_instead_of_running_off_the_end_of_the_bar() {
        // 184s track, 200s elapsed => 16s in on the second pass, NOT 200s past a 184s bar.
        let (end, pos) = timeline(Some(184_000), Some(200_000)).expect("a scrubber");
        assert_eq!(end, 184_000 * TICKS_PER_MS);
        assert_eq!(pos, 16_000 * TICKS_PER_MS);
    }

    #[test]
    fn a_missing_or_negative_elapsed_starts_at_zero() {
        assert_eq!(timeline(Some(1_000), None), Some((10_000_000, 0)));
        assert_eq!(timeline(Some(1_000), Some(-5)), Some((10_000_000, 0)));
    }

    #[test]
    fn buttons_map_to_the_same_intents_the_on_screen_transport_sends() {
        // If this drifts, a media key would do something the visible control does not — the one
        // outcome that would make the feature untrustworthy.
        assert_eq!(
            message_for(Button::Play),
            ClientMessage::SetBedPaused { paused: false }
        );
        assert_eq!(
            message_for(Button::Pause),
            ClientMessage::SetBedPaused { paused: true }
        );
        assert_eq!(message_for(Button::Next), ClientMessage::TransportNext);
        assert_eq!(message_for(Button::Previous), ClientMessage::TransportPrev);
    }
}
