//! # hud-protocol
//!
//! The wire contract shared between the private `starplatform` server and the public
//! `starplatform-hud` overlay client. This crate is the ONLY part of the platform the public
//! repo is allowed to depend on.
//!
//! Invariants (enforced by review, not the compiler):
//! - No secrets, credentials, or org-internal rules.
//! - No database, no I/O framework, no domain logic — just serialisable types.
//! - Every type here is part of a public API contract: change it deliberately.

use serde::{Deserialize, Serialize};

/// Protocol version. Bump on any breaking change to a type in this crate so the
/// client can refuse to talk to an incompatible server (and vice versa).
///
/// v2: added the authenticated member channel — `ClientMessage::Telemetry` (StarStats → platform
/// self-reported game state) and `ServerMessage::OrgContext` (platform → StarStats org enrichment).
/// v3: `OrgContext.fleet` — org fleet movement (signed-out vehicles) in the enrichment feed.
/// v4: `OpContext.objectives` / `current_stage` / `is_lead` + `ClientMessage::SetObjective` —
///     leader-driven objective stage switching on a live op.
/// v5: `OpContext.squad` / `my_ready` / `ready_count` + `ClientMessage::SetReady` —
///     per-op "ready now" check-in surfaced on the HUD squad widget.
/// v6: `ClientMessage::CheckIn` — a member checks into a live op directly from the HUD.
/// v7: `ClientMessage::CompleteOp` — the op lead completes (closes) their live op from the HUD.
/// v8: `ObjectiveStep.detail` / `location` / `tip` — rich active-step card when the op's objective
///     source is a Field Guide (codex). Optional + serde-default, so v7 peers ignore them.
/// v9: `OpContext.phases` + `PhaseWire` — named objective grouping over the objective rail.
/// v10: `ClientMessage::{SetBed, SetBedVolume, SetBedPaused}` — the op lead controls the live
///      music bed (mood/volume/pause) for their op from the HUD.
/// v11: `OpContext.music` — pushed `MusicState` play-status (bed/volume/paused/active) so the
///      whole squad sees what's currently playing, not just the lead who set it.
/// v12: `MusicState.{tags, stage_tags}` — the current bed's tags and the active phase's tags, for
///      tag-driven bed selection on the HUD. `OpContext.soundboard` + `SoundboardClip` — the lead's
///      tappable clip board. `ClientMessage::TriggerClip` — the op lead fires a one-shot clip.
/// v13: `MusicState.{now_playing, playlist}` — the currently-playing track's metadata (`TrackMeta`)
///      and the bed playlist position (`PlaylistPos`), for a HUD now-playing widget.
///      `ClientMessage::{TransportNext, TransportPrev}` — the op lead skips the bed playlist
///      forward/back. Optional + serde-default, so v12 peers ignore/omit them.
/// v14: `OrgContext.discord_presence` — server-computed, privacy-gated Discord Rich Presence block
///      (`DiscordPresence`) for the HUD to mirror onto the member's public Discord profile. Optional
///      + serde-default, so v13 peers ignore/omit it.
/// v15: `OpContext.comms` — a member-facing channel-comms board (`CommsBoard`: a fixed quick-action
///      catalog + the member's allowed channel picker list), populated for ANY member checked into a
///      live op (not lead-only, unlike `soundboard`). `ClientMessage::{SendQuickAction, SendText}` —
///      a member posts an attributed quick-action or free-text message to the op's ready-room (the
///      default, `channel: None`) or another channel from the picker. The server re-verifies the
///      member's live Discord SEND_MESSAGES permission in a non-default target channel before
///      posting — the wire `channel` carries no authority on its own. Optional + serde-default, so
///      v14 peers ignore/omit `comms`. Same-version addition (slice 4b — member clip triggers):
///      `OpContext.member_clips` (`MemberClipBoard`: the op's current `clip_policy` plus the clips
///      a member may fire under it), populated for ANY member checked into a live op when
///      clip-triggering is available (`voice.enabled` AND a claimed pool bot — the same gate
///      re-checked at trigger time). `ClientMessage::TriggerMemberClip { key }` — a member fires a
///      hud-tagged clip into their op, subject to the policy gate, a 1s roll-up (an identical
///      repeat within 1s is dropped), and a 30s-per-rolling-60s room budget.
///      `ClientMessage::SetClipPolicy { policy }` sets the policy to off/urgent/all (the op lead
///      only). Both purely additive + serde-default, so this stays wire-compatible with the rest
///      of v15 (no version bump needed). Same-version addition (slice 5 — ticker + auto-status):
///      `CommsBoard.ticker` (`Vec<TickerMessage>`) — the last few messages in the member's own
///      ready-room text channel, oldest first, cache-fronted + refreshed on a slow cadence
///      server-side so the HUD shows live room chatter without alt-tabbing. Auto-status (telemetry
///      `downed`/`death` auto-firing the existing "Downed — need backup" quick action) is a
///      server-side-only gate/reuse of the v15 comms machinery and adds no new wire shape at all.
///      `CommsBoard.ticker` is `#[serde(default)]`, so a pre-slice-5 peer round-trips fine.
/// v16: multi-contract ops (C3) — an op's objective rail is now the ordered concatenation of N
///      attached contracts, then the Field Guide, then the op's ad-hoc steps, instead of one
///      mutually-exclusive source. `ObjectiveStep.{source, source_kind}` stamp each step with the
///      attached source that produced it, so a merged rail can be attributed per step.
///      `OpContext.sources` (`Vec<SourceWire>`) is that attach list, and `OpContext.can_edit_sources`
///      is the viewer's own lead flag for the in-HUD source editor. All four are `Option`/`Vec` +
///      serde-default and would NOT have needed a bump on their own.
///      **The bump is forced by `ClientMessage::{AttachSource, DetachSource, AddAdhocStep,
///      RemoveAdhocStep}`** — four new externally-tagged variants a v15 server cannot deserialize.
///      Skew is graceful in both directions: the member socket's receive loop matches on
///      `serde_json::from_str::<ClientMessage>` with a catch-all arm, so a v15 server *ignores* an
///      unknown tag rather than dropping the connection, and `Hello.protocol_version` is never
///      compared against `PROTOCOL_VERSION` on either side — no peer refuses on mismatch.
///      Same-version addition (D5 — the dead Details button): `OpContext.id`, the op's event id, so
///      the overlay can deep-link to `/panel/events/{id}` instead of the `/panel/ops` it shipped
///      pointing at, which has never been a route. `Option` + serde-default + `skip_serializing_if`,
///      so a server that omits it is indistinguishable on the wire from one predating it — and the
///      overlay hides the button on `None` rather than guessing a URL.
/// v17: contract step metadata on the rail. Contracts are now synced from StarStats with real
///      structure — `step_type`, risk, required vehicle/equipment, failure conditions — instead of
///      a flat list of objective strings, and `ObjectiveStep` carries that through so the overlay
///      can show a lead WHY a step is dangerous and WHAT it needs, not just its label.
///      Every new field is `Option`/`bool` + serde-default + `skip_serializing_if`, so this is
///      wire-compatible in both directions: a v16 server omits them and a v17 overlay renders the
///      step exactly as before, while a v16 overlay ignores what it does not know. The bump is
///      therefore INFORMATIONAL — nothing refuses on it — but it is taken anyway because the
///      rail's semantic content changed and a version is how that gets communicated.
///      Also v17: the server derives default phase spans from a contract's step types (see
///      `starcore::kb::phases`), so `OpContext.phases` is now usually populated without a lead
///      authoring it. That is a server-side behaviour change with no wire-shape change.
pub const PROTOCOL_VERSION: u32 = 17;

/// Messages the server pushes to a connected overlay over the websocket.
///
/// Every live consumer reconnects with a full-state resync (see the master prompt's
/// "live layer" rule), so the first message after connect is always a `Snapshot`.
///
/// `OrgContext` carries the largest payload (the op picture, incl. tags/soundboard) but this enum
/// is a low-frequency websocket push, not a hot allocation path — boxing it would ripple field
/// access through every server/overlay call site for no runtime benefit, so the size lint is
/// deliberately allowed here rather than upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerMessage {
    /// Full current state, sent on connect and on resync.
    Snapshot { protocol_version: u32 },
    /// The org's public identity changed (name/mantra/colours voted in).
    IdentityChanged(OrgIdentity),
    /// Org-level context for an authenticated, linked member — the org around them. Scoped
    /// server-side to the member's permissions; op-driven (the op picture on a live op, else their
    /// personal slice). Sent only on the authenticated member channel, never the public socket.
    OrgContext(OrgContext),
    /// Keep-alive / liveness ping.
    Heartbeat,
}

/// Messages the overlay sends to the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Sent immediately after connecting to request a full snapshot.
    Hello { protocol_version: u32 },
    /// Self-reported game state from the member's own `Game.log` tail (read-only, ToS-safe — the
    /// same boundary as StarStats' existing log source). Location is a zone/state label, never
    /// memory-read coordinates. Only honoured on the authenticated member channel.
    Telemetry(Telemetry),
    /// Liveness response.
    Heartbeat,
    /// The op lead switches the active objective stage (0-based index). Authorised server-side —
    /// the sender must be the lead of their current live op. Only honoured on the member channel.
    SetObjective { index: usize },
    /// A member toggles their "ready now" state for the op they're checked into. Authorised
    /// server-side (the sender must be on a live op). Only honoured on the member channel.
    SetReady { ready: bool },
    /// A member checks into a live op straight from the HUD, by the op's `event_id` (the
    /// machine-facing id from `/api/events`). Authorised server-side — honoured only when that event
    /// is currently live. Only honoured on the member channel.
    CheckIn { event_id: String },
    /// The op lead completes (closes) their current live op from the HUD: marks it completed, strips
    /// the board's RSVP controls, and posts the debrief. Authorised server-side — honoured only for
    /// the lead of their own live op. Only honoured on the member channel.
    CompleteOp,
    /// The op lead switches the live music bed mood (e.g. "combat", "staging") for their op.
    /// Authorised server-side — the sender must be the lead of their current live op. Only honoured
    /// on the member channel.
    SetBed { key: String },
    /// The op lead sets the live music bed volume (0-100). Authorised server-side — the sender must
    /// be the lead of their current live op. Only honoured on the member channel.
    SetBedVolume { percent: i64 },
    /// The op lead pauses/resumes the live music bed. Authorised server-side — the sender must be
    /// the lead of their current live op. Only honoured on the member channel.
    SetBedPaused { paused: bool },
    /// The op lead fires a one-shot soundboard clip for their op. Authorised server-side — the
    /// sender must be the lead of their current live op. Only honoured on the member channel.
    TriggerClip { key: String },
    /// The op lead skips the live music bed playlist forward one track. Authorised server-side —
    /// the sender must be the lead of their current live op. Only honoured on the member channel.
    TransportNext,
    /// The op lead skips the live music bed playlist back one track. Authorised server-side — the
    /// sender must be the lead of their current live op. Only honoured on the member channel.
    TransportPrev,
    /// A member fires a fixed quick-action (e.g. "En route") into a channel, posted as an
    /// attributed bot message. `channel` is a Discord channel id from the pushed `CommsBoard`
    /// picker; `None` posts to the op's ready room. Authorised server-side — the sender must be
    /// checked into a live op, and a non-default `channel` is re-verified live before posting.
    /// Only honoured on the member channel.
    SendQuickAction {
        channel: Option<u64>,
        action: String,
    },
    /// A member posts free text into a channel, posted as an attributed bot message. Same
    /// targeting/authorisation rules as [`ClientMessage::SendQuickAction`]. Only honoured on the
    /// member channel.
    SendText { channel: Option<u64>, text: String },
    /// A member fires a clip from their op's member clip board. Authorised server-side — the
    /// sender must be checked into a live op with clip-triggering available, `key` must be one of
    /// the hud-tagged clips the op's current `clip_policy` allows, and the fire is subject to a 1s
    /// roll-up (an identical repeat within a second is dropped) and a 30s-per-rolling-60s room
    /// budget. Only honoured on the member channel.
    TriggerMemberClip { key: String },
    /// The op lead sets the member clip policy (`'off' | 'urgent' | 'all'`). Authorised
    /// server-side — the sender must be the lead of their current live op. Only honoured on the
    /// member channel.
    SetClipPolicy { policy: String },
    /// The op lead attaches an objective source to their current live op. `kind` is a
    /// [`SourceWire::kind`] tag (`"contract"` | `"guide"`); `id` is that source's machine-facing id
    /// as it appears in the search API. **Attaching is only a tail append when the new source is
    /// the last group** — the rail is contracts → guide → ad-hoc, so a contract attached to an op
    /// that also carries a guide or ad-hoc steps lands mid-rail and **renumbers** everything after
    /// it (and attaching a guide over a different one is a silent replace). The server therefore
    /// remaps the op's active stage on both edges, exactly as it does for
    /// [`ClientMessage::DetachSource`]. Authorised server-side — the sender must be the lead of
    /// their current live op AND still hold op-management permission, both re-resolved live: the
    /// wire `kind`/`id` carry no authority, and an unrecognised `kind` is denied, never assumed.
    /// Only honoured on the member channel.
    AttachSource { kind: String, id: String },
    /// The op lead detaches an objective source from their current live op. Same fields and same
    /// live-authorisation rules as [`ClientMessage::AttachSource`]. Detaching a source that is not
    /// last **renumbers** every objective after it, so the server remaps the op's active stage in
    /// the same transaction rather than leaving the lead pointing at a shifted step. Only honoured
    /// on the member channel.
    DetachSource { kind: String, id: String },
    /// The op lead appends a free-text ad-hoc objective to their current live op's tail. Ad-hoc
    /// steps are per-op scratch, never org catalog content. Authorised server-side — same live
    /// lead + op-management gate as [`ClientMessage::AttachSource`]. Only honoured on the member
    /// channel.
    AddAdhocStep { label: String },
    /// The op lead removes one ad-hoc objective by its 0-based index **within the ad-hoc tail**
    /// (not its index on the merged rail). Same live authorisation gate as
    /// [`ClientMessage::AddAdhocStep`]; an out-of-range index is a no-op server-side, never a
    /// panic. Only honoured on the member channel.
    RemoveAdhocStep { index: usize },
}

/// Self-reported, read-only game state pushed by a linked member's StarStats client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Telemetry {
    /// Zone / location label (e.g. "Crusader", "Daymar") — never coordinates.
    pub zone: Option<String>,
    /// Quantum travel state label (e.g. "travelling", "idle").
    pub quantum_state: Option<String>,
    /// A discrete event since the last message: "spawn" | "kill" | "death" | "downed".
    pub event: Option<String>,
}

/// The org around a linked member, scoped to what they're permitted to see.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OrgContext {
    /// True when the member is on a live op (the picture is the op's); false = personal slice.
    pub on_op: bool,
    /// The live op picture, when on one and permitted to see it.
    pub op: Option<OpContext>,
    /// Presence of other linked members the viewer is permitted to see.
    pub roster: Vec<PresenceEntry>,
    /// Fleet movement — org vehicles currently signed out (name/status/location), permission-scoped.
    #[serde(default)]
    pub fleet: Vec<FleetEntry>,
    /// Org alert posture label, if any (e.g. "green", "elevated").
    pub alert_posture: Option<String>,
    /// Server-computed, privacy-gated Discord Rich Presence block. `None` when the member hasn't
    /// opted in (or the org has no `DISCORD_CLIENT_ID` configured) — the HUD clears any active
    /// presence in that case. Absent entirely on a pre-v14 peer, defaulting to `None`.
    #[serde(default)]
    pub discord_presence: Option<DiscordPresence>,
}

/// A privacy-gated, server-computed Discord Rich Presence block, pushed so the HUD can mirror it
/// onto the member's public Discord profile without ever evaluating privacy prefs itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordPresence {
    /// Whether the member currently permits Rich Presence to be shown (privacy signal +
    /// `DISCORD_CLIENT_ID` configured). `false` means the HUD should clear any active presence.
    pub allowed: bool,
    /// The Discord application id to register the presence under.
    pub client_id: String,
    /// The "details" line (e.g. "Playing StarPlatform").
    pub details: String,
    /// The "state" line (e.g. the current op's name, or "In an operation"/"Idle").
    pub state: String,
    /// Optional link button shown on the profile (label + https URL). Empty `button_url` = no
    /// button. Rich Presence buttons are visible to OTHERS viewing the profile, not to the member.
    #[serde(default)]
    pub button_label: String,
    #[serde(default)]
    pub button_url: String,
}

/// One org vehicle's current disposition (no holder id on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetEntry {
    pub name: String,
    pub status: String,
    pub location: String,
}

/// A live op's shared picture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OpContext {
    /// The op's event id, so the overlay can deep-link to `/panel/events/{id}`.
    ///
    /// `None` on a peer predating this field — and the overlay must **hide** the link rather than
    /// guess a URL, which is what this field exists to stop: the Details button shipped pointing
    /// at `/panel/ops`, a route that has never existed.
    ///
    /// A `String`, not a `Uuid`, to keep `uuid` out of this crate's dependency set; it is only
    /// ever pasted into a path. Carrying it discloses nothing — the member is checked into this op
    /// and already receives its name, objectives and squad, and `/panel/events/{id}` runs its own
    /// `member()` gate regardless of who holds the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub status: String,
    /// Active orders / assignments for the op.
    pub orders: Vec<String>,
    /// Ordered objective checklist (from the op's attached contract), with done/active flags.
    #[serde(default)]
    pub objectives: Vec<ObjectiveStep>,
    /// Index of the active objective stage (0-based).
    #[serde(default)]
    pub current_stage: usize,
    /// True when the viewing member is the op lead — the HUD shows the stage-switch buttons.
    #[serde(default)]
    pub is_lead: bool,
    /// Members checked into this op — live location/status + ready state (permission-narrow:
    /// op-membership scoped).
    #[serde(default)]
    pub squad: Vec<SquadMember>,
    /// The viewing member's own ready state for this op.
    #[serde(default)]
    pub my_ready: bool,
    /// How many squad members have marked ready.
    #[serde(default)]
    pub ready_count: usize,
    /// Phase grouping over the objective rail (empty = flat rail). Overlay groups client-side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseWire>,
    /// Live music-bed play status, when applicable. None on peers/ops predating v11.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music: Option<MusicState>,
    /// The lead's tappable soundboard clips for this op (empty = no soundboard shown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub soundboard: Vec<SoundboardClip>,
    /// The member-facing channel-comms board (quick-action catalog + allowed channel picker),
    /// populated for any member checked into this op. `None` on a pre-v15 peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comms: Option<CommsBoard>,
    /// The member-facing clip board (current clip policy + the clips a member may fire under it),
    /// populated for any member checked into this op WHEN clip-triggering is available
    /// (`voice.enabled` and a claimed pool bot). `None` when unavailable, or on a peer predating
    /// this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_clips: Option<MemberClipBoard>,
    /// The op's attached objective sources, in rail order (contracts by attach position, then the
    /// Field Guide, then the ad-hoc tail). Empty on an op with nothing attached, and on a peer
    /// predating this field. Carries no more than the rail already does — the source names are the
    /// same strings already pushed as `PhaseWire.name` — so it widens no member's view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceWire>,
    /// True only for the op lead: the HUD shows the attach/detach + ad-hoc source editor. UX only —
    /// every `AttachSource`/`DetachSource`/`AddAdhocStep`/`RemoveAdhocStep` is re-authorised live
    /// server-side, so a client that sets this itself gains nothing.
    #[serde(default)]
    pub can_edit_sources: bool,
}

/// One objective source attached to a live op, as seen on the HUD's source editor.
///
/// Identity + size only. `id` is the machine-facing id as a string (the house wire convention for
/// ids — see [`CommsChannel::id`] and [`ClientMessage::CheckIn::event_id`]); it is `None` for the
/// ad-hoc tail, which has no single row of its own. `objective_count` is what lets the HUD warn
/// that detaching a non-last source renumbers the steps after it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SourceWire {
    /// `"contract"` | `"guide"` | `"adhoc"`.
    pub kind: String,
    /// The source's id, as a string. `None` for the ad-hoc tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Contract name, guide title, or the ad-hoc tail's display name.
    pub name: String,
    /// The source's place in the attach order — its index into the op's source list.
    pub position: usize,
    /// How many objectives this source contributes to the merged rail.
    #[serde(default)]
    pub objective_count: usize,
}

/// The member-facing clip board: the op's current clip-trigger policy plus the clips a member may
/// fire under it (already filtered by policy — the HUD doesn't need to re-derive the gate).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MemberClipBoard {
    /// `'off' | 'urgent' | 'all'`.
    pub policy: String,
    pub clips: Vec<SoundboardClip>,
}

/// A member-facing channel-comms board: a fixed quick-action catalog plus the channels the member
/// is offered in the picker. The picker is UX-only — `channel` on `SendQuickAction`/`SendText`
/// carries no authority; the server re-verifies live send permission at post time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CommsBoard {
    pub quick_actions: Vec<QuickAction>,
    pub channels: Vec<CommsChannel>,
    /// The last few messages in the member's own ready-room text channel, oldest first, so the HUD
    /// can show live room chatter without alt-tabbing out of the game. Best-effort and cache-fronted
    /// server-side — a Discord fetch failure or a not-yet-refreshed cache just yields an empty or
    /// stale-but-present ticker, never an error. `#[serde(default)]` so a pre-slice-5 peer round-trips
    /// fine (empty ticker).
    #[serde(default)]
    pub ticker: Vec<TickerMessage>,
}

/// One recent chat line from the member's ready-room channel, for the HUD's live ticker. Shows only
/// a channel the member can already see in Discord (their own op's ready room) — no new leak.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TickerMessage {
    pub author: String,
    pub content: String,
}

/// One quick-action button on the member comms board (fixed, server-authoritative catalog).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct QuickAction {
    pub key: String,
    pub label: String,
}

/// One channel offered in the comms channel picker. `id` is the Discord channel id as a decimal
/// string (not a `u64`) — Discord snowflakes exceed JS's safe-integer range, so the HUD keeps
/// channel ids as strings end-to-end rather than round-tripping them through a JS `Number`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CommsChannel {
    pub id: String,
    pub name: String,
}

/// Live music-bed play status for the op, pushed so the whole squad sees what's playing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MusicState {
    /// Current desired bed mood key (e.g. "bed/combat"); None = silence ("off").
    pub bed: Option<String>,
    /// Effective volume percent (0-100): the op override, or the global default when unset.
    pub volume: u32,
    pub paused: bool,
    /// True when a bed is set AND a pool bot is claimed for the op (actually audible).
    pub active: bool,
    /// Tags carried by the currently playing bed (e.g. "combat", "tense").
    #[serde(default)]
    pub tags: Vec<String>,
    /// Tags carried by the active objective phase, for tag-driven bed matching.
    #[serde(default)]
    pub stage_tags: Vec<String>,
    /// Metadata for the currently-playing track, when known.
    #[serde(default)]
    pub now_playing: Option<TrackMeta>,
    /// Position within the bed's playlist, when applicable.
    #[serde(default)]
    pub playlist: Option<PlaylistPos>,
    /// v14: milliseconds since the CURRENT bed track started, for a per-track progress bar. The bed
    /// loops, so the HUD wraps this at `now_playing.duration_ms`. `None` = no bed / unknown start.
    #[serde(default)]
    pub elapsed_ms: Option<i64>,
}

/// Metadata for the currently-playing track on the live music bed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackMeta {
    pub key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    /// v14: track length in milliseconds when known (probed by the voice-worker on first play);
    /// `None` = unknown, in which case the HUD shows elapsed only (no length / remaining).
    #[serde(default)]
    pub duration_ms: Option<i32>,
}

/// Position within the active bed's playlist, for a HUD now-playing widget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaylistPos {
    pub pos: usize,
    pub len: usize,
    #[serde(default)]
    pub moods: Vec<String>,
}

/// One lead-tappable clip in the HUD soundboard (a clip carrying the `hud` tag).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SoundboardClip {
    pub key: String,
    pub label: String,
}

/// One phase header on the HUD objective rail. `span` = how many consecutive objectives it covers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PhaseWire {
    pub name: String,
    pub span: u32,
}

/// One objective step on a live op, as seen on the HUD.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObjectiveStep {
    pub label: String,
    /// Free-text timing, e.g. "~10 min".
    pub timing: Option<String>,
    /// Already completed (index < current stage).
    pub done: bool,
    /// The active stage (index == current stage).
    pub active: bool,
    /// Rich guide-step detail (Field-Guide-sourced ops only). Shown on the active step's card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Where this step happens (location label), guide-sourced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// A tip for this step, guide-sourced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
    /// Display name of the attached source that produced this step (contract name / guide title /
    /// the ad-hoc tail's name), for per-step attribution on a merged rail. `None` on a peer
    /// predating this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// That source's machine-facing kind tag — `"contract"` | `"guide"` | `"adhoc"` — matching
    /// [`SourceWire::kind`], so the HUD can style attribution without string-matching the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,

    // ── v17: contract step metadata ─────────────────────────────────────────────────────────────
    //
    // All contract-sourced. Guide and ad-hoc steps leave these empty, which is why every one is
    // `Option`/`bool` with a serde default: a v16 peer omits them entirely and must still
    // deserialise, and a v17 peer must render a guide step that has none of them.
    /// The step's machine-facing kind — `travel_to_location`, `deliver_cargo`, … Open string: a
    /// new extraction model will invent new ones, so the HUD styles what it knows and falls back
    /// to plain for the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_type: Option<String>,
    /// `"low"` | `"medium"` | `"high"`, normalised upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    /// Ship this step needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_vehicle: Option<String>,
    /// Kit this step needs (tractor beam, tow cable, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_equipment: Option<String>,
    /// What ends the contract in failure at this step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_condition: Option<String>,
    /// Skippable step. `false` for every non-contract source, hence no `Option`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// One member's self-reported presence (display label only — no raw ids on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PresenceEntry {
    pub name: String,
    pub zone: Option<String>,
    pub status: Option<String>,
}

/// One squad member on a live op (display label only — no raw ids on the wire).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SquadMember {
    pub name: String,
    pub zone: Option<String>,
    pub status: Option<String>,
    pub ready: bool,
}

/// Public-safe view of the org's brand identity. Backed by `config` rows on the server;
/// changeable only via a governance vote. Contains nothing private.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrgIdentity {
    pub name: String,
    pub short_name: String,
    pub mantra: String,
    /// Hex colour, e.g. "#0b1d3a".
    pub primary_colour: String,
    /// Hex colour, e.g. "#f5a623".
    pub accent_colour: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_message_round_trips_as_tagged_json() {
        let msg = ServerMessage::IdentityChanged(OrgIdentity {
            name: "Placeholder Org".into(),
            short_name: "ORG".into(),
            mantra: "One 'verse, every voice.".into(),
            primary_colour: "#0b1d3a".into(),
            accent_colour: "#f5a623".into(),
        });
        let json = serde_json::to_string(&msg).expect("serialise");
        assert!(json.contains("\"type\":\"identity_changed\""));
        let back: ServerMessage = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(msg, back);
    }

    #[test]
    fn hello_carries_protocol_version() {
        let json = serde_json::to_string(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .expect("serialise");
        assert!(json.contains(&format!("\"protocol_version\":{PROTOCOL_VERSION}")));
    }

    #[test]
    fn protocol_version_is_17() {
        assert_eq!(PROTOCOL_VERSION, 17);
    }

    /// The HUD's JS `PROTOCOL_VERSION` is a HAND-MAINTAINED mirror of the Rust constant, and
    /// nothing else in the tree pairs them — the exact shape that rots. `include_str!` (not a
    /// runtime read) so a path typo is a compile error under `cargo check --all-targets` rather
    /// than a guard that silently never runs.
    ///
    /// Monorepo-scoped by design: it reaches the sibling `hud` crate's asset. It lives inside
    /// `#[cfg(test)]`, so it is never expanded for a plain build and cannot affect this crate's
    /// publishability as the public client's dependency.
    #[test]
    fn js_protocol_version_mirror_matches_rust() {
        const UI: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../hud/ui/index.html"));
        const DECL: &str = "const PROTOCOL_VERSION = ";

        let start = UI
            .find(DECL)
            .unwrap_or_else(|| panic!("hud/ui/index.html must declare `{DECL}<n>;`"))
            + DECL.len();
        let rest = &UI[start..];
        let end = rest
            .find(';')
            .unwrap_or_else(|| panic!("hud/ui/index.html's `{DECL}` must end in `;`"));
        let mirrored: u32 = rest[..end]
            .trim()
            .parse()
            .expect("hud/ui/index.html's PROTOCOL_VERSION must be a plain integer literal");

        assert_eq!(
            mirrored, PROTOCOL_VERSION,
            "hud/ui/index.html's `PROTOCOL_VERSION = {mirrored}` has drifted from \
             hud-protocol's `PROTOCOL_VERSION = {PROTOCOL_VERSION}` — bump BOTH in the same commit"
        );
    }

    #[test]
    fn c3_source_client_messages_round_trip() {
        for (m, tag) in [
            (
                ClientMessage::AttachSource {
                    kind: "contract".into(),
                    id: "6f1d2c3b-0000-4000-8000-000000000001".into(),
                },
                "attach_source",
            ),
            (
                ClientMessage::DetachSource {
                    kind: "guide".into(),
                    id: "6f1d2c3b-0000-4000-8000-000000000002".into(),
                },
                "detach_source",
            ),
            (
                ClientMessage::AddAdhocStep {
                    label: "Sweep the upper deck".into(),
                },
                "add_adhoc_step",
            ),
            (
                ClientMessage::RemoveAdhocStep { index: 2 },
                "remove_adhoc_step",
            ),
        ] {
            let json = serde_json::to_string(&m).expect("serialise");
            assert!(
                json.contains(&format!("\"type\":\"{tag}\"")),
                "expected tag {tag} in {json}"
            );
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
        }
    }

    /// The four new variants are what forces v16: a v15 peer's `ClientMessage` has no such tags,
    /// so its parse fails. That failure must stay *ignorable* — the member socket's receive loop
    /// matches with a catch-all arm, so an unknown tag is dropped and the connection survives.
    /// This pins the shape that makes that possible: an unknown tag is an `Err`, not a panic, and
    /// never silently decodes as some other variant.
    #[test]
    fn a_v15_peer_rejects_a_v16_client_message_without_mis_decoding_it() {
        for tag in [
            "attach_source",
            "detach_source",
            "add_adhoc_step",
            "remove_adhoc_step",
        ] {
            let unknown = format!("{{\"type\":\"{tag}_from_the_future\",\"kind\":\"contract\"}}");
            assert!(
                serde_json::from_str::<ClientMessage>(&unknown).is_err(),
                "an unknown tag must fail to parse, not decode as a neighbouring variant"
            );
        }
    }

    #[test]
    fn sources_and_can_edit_round_trip_on_op_context() {
        let op = OpContext {
            name: "Reclaim".into(),
            status: "live".into(),
            sources: vec![
                SourceWire {
                    kind: "contract".into(),
                    id: Some("6f1d2c3b-0000-4000-8000-000000000001".into()),
                    name: "Wreck Reclamation".into(),
                    position: 0,
                    objective_count: 3,
                },
                SourceWire {
                    kind: "adhoc".into(),
                    id: None,
                    name: "Ad-hoc".into(),
                    position: 1,
                    objective_count: 2,
                },
            ],
            can_edit_sources: true,
            ..OpContext::default()
        };
        let json = serde_json::to_string(&op).expect("serialise");
        assert!(json.contains("\"sources\""));
        assert!(json.contains("\"can_edit_sources\":true"));
        // The ad-hoc tail has no id; the field is skipped rather than serialised as null.
        assert!(!json.contains("\"id\":null"));
        assert_eq!(serde_json::from_str::<OpContext>(&json).unwrap(), op);
    }

    #[test]
    fn sources_omitted_when_empty() {
        let op = OpContext::default();
        let json = serde_json::to_string(&op).unwrap();
        assert!(!json.contains("\"sources\""));
    }

    #[test]
    fn objective_step_source_attribution_round_trips() {
        let step = ObjectiveStep {
            label: "Clear hostiles".into(),
            timing: Some("~10 min".into()),
            done: false,
            active: true,
            detail: Some("Sweep the wreck before scraping.".into()),
            location: None,
            tip: None,
            source: Some("Wreck Reclamation".into()),
            source_kind: Some("contract".into()),
            ..ObjectiveStep::default()
        };
        let json = serde_json::to_string(&step).expect("serialise");
        assert!(json.contains("\"source\":\"Wreck Reclamation\""));
        assert!(json.contains("\"source_kind\":\"contract\""));
        assert_eq!(serde_json::from_str::<ObjectiveStep>(&json).unwrap(), step);

        // Unattributed steps omit both keys entirely rather than emitting nulls.
        let bare = ObjectiveStep {
            label: "Clear hostiles".into(),
            ..ObjectiveStep::default()
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("source"));
    }

    /// Backward compat: a genuine v15 payload has none of the v16 keys.
    #[test]
    fn v15_payloads_without_the_v16_keys_deserialize() {
        let step_json = r#"{"label":"Clear hostiles","timing":null,"done":false,"active":true}"#;
        let step: ObjectiveStep = serde_json::from_str(step_json).unwrap();
        assert_eq!(step.source, None);
        assert_eq!(step.source_kind, None);

        let op_json = r#"{"name":"Op","status":"live","orders":[],"objectives":[],"current_stage":0,"is_lead":false,"squad":[],"my_ready":false,"ready_count":0,"phases":[]}"#;
        let op: OpContext = serde_json::from_str(op_json).unwrap();
        assert!(op.sources.is_empty());
        assert!(!op.can_edit_sources);
        // `id` was added after v16 without a bump — it must default rather than fail the parse.
        assert_eq!(op.id, None);
    }

    /// `OpContext.id` is additive within v16: a server that omits it must not break an overlay
    /// that reads it, and the omission must be *visible* (`None`) rather than an empty string the
    /// overlay would paste into a URL. This is what lets the Details button hide itself instead of
    /// opening a guessed path — the failure that shipped as `/panel/ops`.
    #[test]
    fn op_context_id_is_optional_and_omitted_when_absent() {
        let without = OpContext {
            name: "Op".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&without).unwrap();
        assert!(
            !json.contains("\"id\""),
            "an absent id must be skipped on the wire, not sent as null: {json}"
        );
        assert_eq!(
            serde_json::from_str::<OpContext>(&json).unwrap().id,
            None,
            "round-trip lost the absent-id signal"
        );

        let with = OpContext {
            id: Some("4f1e…".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&with).unwrap();
        assert_eq!(
            serde_json::from_str::<OpContext>(&json)
                .unwrap()
                .id
                .as_deref(),
            Some("4f1e…")
        );
    }

    /// Forward compat: a v16 payload must deserialize on a peer whose struct lacks the new fields.
    /// Modelled with a local stand-in for the v15 shape (serde ignores unknown fields by default —
    /// this pins that no `deny_unknown_fields` creeps onto these types later).
    #[test]
    fn a_v16_payload_deserializes_on_a_v15_shaped_struct() {
        #[derive(Deserialize)]
        struct V15ObjectiveStep {
            label: String,
            #[serde(default)]
            detail: Option<String>,
        }
        #[derive(Deserialize)]
        struct V15OpContext {
            name: String,
            #[serde(default)]
            objectives: Vec<V15ObjectiveStep>,
            #[serde(default)]
            phases: Vec<PhaseWire>,
        }

        let v16 = OpContext {
            name: "Reclaim".into(),
            status: "live".into(),
            objectives: vec![ObjectiveStep {
                label: "Clear hostiles".into(),
                detail: Some("Sweep first.".into()),
                active: true,
                source: Some("Wreck Reclamation".into()),
                source_kind: Some("contract".into()),
                ..ObjectiveStep::default()
            }],
            phases: vec![PhaseWire {
                name: "Wreck Reclamation".into(),
                span: 1,
            }],
            sources: vec![SourceWire {
                kind: "contract".into(),
                id: Some("6f1d2c3b-0000-4000-8000-000000000001".into()),
                name: "Wreck Reclamation".into(),
                position: 0,
                objective_count: 1,
            }],
            can_edit_sources: true,
            ..OpContext::default()
        };
        let json = serde_json::to_string(&v16).expect("serialise");

        let old: V15OpContext = serde_json::from_str(&json)
            .expect("a v16 payload must still deserialize on a v15-shaped peer");
        assert_eq!(old.name, "Reclaim");
        assert_eq!(old.objectives.len(), 1);
        assert_eq!(old.objectives[0].label, "Clear hostiles");
        // The v8 rich card keeps working on the old shape — v16 only added alongside it.
        assert_eq!(old.objectives[0].detail.as_deref(), Some("Sweep first."));
        // §1: the phase header IS the source name, so a v15 peer still groups a merged rail.
        assert_eq!(old.phases[0].name, "Wreck Reclamation");
    }

    #[test]
    fn trigger_clip_round_trips() {
        let m = ClientMessage::TriggerClip {
            key: "clip/rally".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"trigger_clip\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn soundboard_and_media_tags_round_trip() {
        let mut op = OpContext {
            id: None,
            name: "Reclaim".into(),
            status: "live".into(),
            orders: vec![],
            objectives: vec![],
            current_stage: 0,
            is_lead: false,
            squad: vec![],
            my_ready: false,
            ready_count: 0,
            phases: vec![],
            music: None,
            soundboard: vec![],
            comms: None,
            member_clips: None,
            sources: vec![],
            can_edit_sources: false,
        };
        op.soundboard = vec![SoundboardClip {
            key: "clip/rally".into(),
            label: "Rally".into(),
        }];
        op.music = Some(MusicState {
            bed: Some("bed/combat".into()),
            volume: 60,
            paused: false,
            active: true,
            tags: vec!["combat".into()],
            stage_tags: vec!["combat".into(), "tense".into()],
            now_playing: None,
            playlist: None,
            elapsed_ms: None,
        });
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"soundboard\""));
        assert!(s.contains("\"stage_tags\""));
        assert_eq!(serde_json::from_str::<OpContext>(&s).unwrap(), op);
    }

    #[test]
    fn now_playing_and_playlist_round_trip() {
        let music = MusicState {
            bed: Some("bed/combat".into()),
            volume: 60,
            paused: false,
            active: true,
            tags: vec!["combat".into()],
            stage_tags: vec![],
            now_playing: Some(TrackMeta {
                key: "bed/combat/track-3".into(),
                title: Some("Firefight".into()),
                artist: Some("Placeholder Artist".into()),
                genre: None,
                duration_ms: Some(204_000),
            }),
            playlist: Some(PlaylistPos {
                pos: 2,
                len: 8,
                moods: vec!["combat".into(), "tense".into()],
            }),
            elapsed_ms: Some(45_000),
        };
        let json = serde_json::to_string(&music).expect("serialise");
        assert!(json.contains("\"now_playing\""));
        assert!(json.contains("\"playlist\""));
        assert_eq!(serde_json::from_str::<MusicState>(&json).unwrap(), music);
    }

    #[test]
    fn transport_next_and_prev_round_trip() {
        for (m, tag) in [
            (ClientMessage::TransportNext, "transport_next"),
            (ClientMessage::TransportPrev, "transport_prev"),
        ] {
            let json = serde_json::to_string(&m).expect("serialise");
            assert!(json.contains(&format!("\"type\":\"{tag}\"")));
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
        }
    }

    #[test]
    fn v12_music_state_without_now_playing_or_playlist_deserializes() {
        // v12: no now_playing/playlist keys at all.
        let json = r#"{"bed":"bed/combat","volume":60,"paused":false,"active":true,"tags":[],"stage_tags":[]}"#;
        let music: MusicState = serde_json::from_str(json).unwrap();
        assert_eq!(music.now_playing, None);
        assert_eq!(music.playlist, None);
    }

    #[test]
    fn v11_op_context_without_soundboard_or_media_tags_deserializes() {
        // v11: no soundboard; music without tags/stage_tags.
        let json = r#"{"name":"Op","status":"live","orders":[],"objectives":[],"current_stage":0,"is_lead":false,"squad":[],"my_ready":false,"ready_count":0,"phases":[],"music":{"bed":"bed/combat","volume":60,"paused":false,"active":true}}"#;
        let op: OpContext = serde_json::from_str(json).unwrap();
        assert!(op.soundboard.is_empty());
        let m = op.music.unwrap();
        assert!(m.tags.is_empty() && m.stage_tags.is_empty());
    }

    #[test]
    fn music_state_round_trips_on_op_context() {
        let mut op = OpContext {
            id: None,
            name: "Reclaim".into(),
            status: "live".into(),
            orders: vec![],
            objectives: vec![],
            current_stage: 0,
            is_lead: false,
            squad: vec![],
            my_ready: false,
            ready_count: 0,
            phases: vec![],
            music: None,
            soundboard: vec![],
            comms: None,
            member_clips: None,
            sources: vec![],
            can_edit_sources: false,
        };
        op.music = Some(MusicState {
            bed: Some("bed/combat".into()),
            volume: 60,
            paused: false,
            active: true,
            tags: vec![],
            stage_tags: vec![],
            now_playing: None,
            playlist: None,
            elapsed_ms: None,
        });
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"music\""));
        assert_eq!(serde_json::from_str::<OpContext>(&s).unwrap(), op);
    }

    #[test]
    fn v10_op_context_without_music_still_deserializes() {
        // A v10 payload has no "music" key; it must default to None.
        let json = r#"{"name":"Op","status":"live","orders":[],"objectives":[],"current_stage":0,"is_lead":false,"squad":[],"my_ready":false,"ready_count":0,"phases":[]}"#;
        let op: OpContext = serde_json::from_str(json).unwrap();
        assert_eq!(op.music, None);
    }

    #[test]
    fn phases_omitted_when_empty_and_present_when_set() {
        let mut ctx = OpContext::default();
        assert!(!serde_json::to_string(&ctx).unwrap().contains("phases"));
        ctx.phases = vec![PhaseWire {
            name: "Ingress".into(),
            span: 2,
        }];
        ctx.name = "Reclaim".into();
        ctx.is_lead = true;
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("\"phases\""));
        assert!(json.contains("Ingress"));

        // An older peer's payload genuinely lacks the `phases` key (it predates this
        // field), not merely an empty array — simulate that by stripping the key
        // entirely, then confirm we still deserialize, with `phases` defaulting empty
        // and the other populated fields intact.
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("phases");
        let skewed = serde_json::to_string(&value).unwrap();
        assert!(!skewed.contains("phases"));
        let round_tripped: OpContext = serde_json::from_str(&skewed)
            .expect("an older peer's payload without a phases key should still deserialize");
        assert_eq!(round_tripped.name, "Reclaim");
        assert!(round_tripped.is_lead);
        assert!(round_tripped.phases.is_empty());
    }

    #[test]
    fn complete_op_round_trips() {
        let m = ClientMessage::CompleteOp;
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(json.contains("\"type\":\"complete_op\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn set_bed_round_trips() {
        let m = ClientMessage::SetBed {
            key: "bed/combat".into(),
        };
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(json.contains("\"type\":\"set_bed\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn set_bed_volume_and_paused_round_trip() {
        for m in [
            ClientMessage::SetBedVolume { percent: 60 },
            ClientMessage::SetBedPaused { paused: true },
        ] {
            let json = serde_json::to_string(&m).expect("serialise");
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
        }
        let vol_json =
            serde_json::to_string(&ClientMessage::SetBedVolume { percent: 60 }).expect("serialise");
        assert!(vol_json.contains("\"type\":\"set_bed_volume\""));
        let paused_json = serde_json::to_string(&ClientMessage::SetBedPaused { paused: true })
            .expect("serialise");
        assert!(paused_json.contains("\"type\":\"set_bed_paused\""));
    }

    #[test]
    fn check_in_round_trips() {
        let m = ClientMessage::CheckIn {
            event_id: "abc-123".into(),
        };
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(json.contains("\"type\":\"check_in\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn squad_and_set_ready_round_trip() {
        let set_ready = ClientMessage::SetReady { ready: true };
        let json = serde_json::to_string(&set_ready).expect("serialise");
        assert!(json.contains("\"type\":\"set_ready\""));
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).unwrap(),
            set_ready
        );

        let op = OpContext {
            id: None,
            name: "Reclaim".into(),
            status: "live".into(),
            orders: vec![],
            objectives: vec![],
            current_stage: 0,
            is_lead: false,
            squad: vec![SquadMember {
                name: "Pilot".into(),
                zone: Some("Daymar".into()),
                status: Some("staging".into()),
                ready: true,
            }],
            my_ready: true,
            ready_count: 1,
            phases: vec![],
            music: None,
            soundboard: vec![],
            comms: None,
            member_clips: None,
            sources: vec![],
            can_edit_sources: false,
        };
        let json = serde_json::to_string(&op).expect("serialise");
        assert_eq!(serde_json::from_str::<OpContext>(&json).unwrap(), op);
    }

    #[test]
    fn telemetry_and_org_context_round_trip() {
        let t = ClientMessage::Telemetry(Telemetry {
            zone: Some("Daymar".into()),
            quantum_state: None,
            event: Some("kill".into()),
        });
        let json = serde_json::to_string(&t).expect("serialise");
        assert!(json.contains("\"type\":\"telemetry\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), t);

        let c = ServerMessage::OrgContext(OrgContext {
            on_op: true,
            op: Some(OpContext {
                id: None,
                name: "Reclaim".into(),
                status: "live".into(),
                orders: vec!["hold the line".into()],
                objectives: vec![ObjectiveStep {
                    label: "Clear hostiles".into(),
                    timing: Some("~10 min".into()),
                    done: false,
                    active: true,
                    detail: Some("Sweep the wreck for hostiles before scraping.".into()),
                    location: Some("Daymar".into()),
                    tip: None,
                    source: Some("Wreck Reclamation".into()),
                    source_kind: Some("contract".into()),
                    ..ObjectiveStep::default()
                }],
                current_stage: 0,
                is_lead: true,
                squad: vec![],
                my_ready: false,
                ready_count: 0,
                phases: vec![],
                music: None,
                soundboard: vec![],
                comms: None,
                member_clips: None,
                sources: vec![SourceWire {
                    kind: "contract".into(),
                    id: Some("6f1d2c3b-0000-4000-8000-000000000001".into()),
                    name: "Wreck Reclamation".into(),
                    position: 0,
                    objective_count: 1,
                }],
                can_edit_sources: true,
            }),
            roster: vec![PresenceEntry {
                name: "Pilot".into(),
                zone: Some("Crusader".into()),
                status: None,
            }],
            fleet: vec![FleetEntry {
                name: "Reclaimer-01".into(),
                status: "checked_out".into(),
                location: "Daymar".into(),
            }],
            alert_posture: Some("elevated".into()),
            discord_presence: None,
        });
        let json = serde_json::to_string(&c).expect("serialise");
        assert!(json.contains("\"type\":\"org_context\""));
        assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), c);
    }

    #[test]
    fn v13_org_context_without_discord_presence_key_deserializes() {
        // A v13 payload genuinely lacks the `discord_presence` key (it predates this field).
        let json = r#"{"on_op":false,"op":null,"roster":[],"fleet":[],"alert_posture":null}"#;
        let ctx: OrgContext = serde_json::from_str(json).unwrap();
        assert_eq!(ctx.discord_presence, None);
    }

    #[test]
    fn discord_presence_round_trips_on_org_context() {
        let ctx = OrgContext {
            discord_presence: Some(DiscordPresence {
                allowed: true,
                client_id: "123456789".into(),
                details: "Playing StarPlatform".into(),
                state: "Reclaim".into(),
                button_label: String::new(),
                button_url: String::new(),
            }),
            ..OrgContext::default()
        };
        let json = serde_json::to_string(&ctx).expect("serialise");
        assert!(json.contains("\"discord_presence\""));
        assert_eq!(serde_json::from_str::<OrgContext>(&json).unwrap(), ctx);
    }

    #[test]
    fn send_quick_action_round_trips_with_and_without_channel() {
        for m in [
            ClientMessage::SendQuickAction {
                channel: None,
                action: "en_route".into(),
            },
            ClientMessage::SendQuickAction {
                channel: Some(123456789012345678),
                action: "en_route".into(),
            },
        ] {
            let json = serde_json::to_string(&m).expect("serialise");
            assert!(json.contains("\"type\":\"send_quick_action\""));
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
        }
    }

    #[test]
    fn send_text_round_trips() {
        let m = ClientMessage::SendText {
            channel: Some(42),
            text: "on my way".into(),
        };
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(json.contains("\"type\":\"send_text\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn comms_board_round_trips_on_op_context() {
        let mut op = OpContext {
            name: "Reclaim".into(),
            status: "live".into(),
            ..OpContext::default()
        };
        op.comms = Some(CommsBoard {
            quick_actions: vec![QuickAction {
                key: "en_route".into(),
                label: "En route".into(),
            }],
            channels: vec![CommsChannel {
                id: "123456789012345678".into(),
                name: "ready-room".into(),
            }],
            ..CommsBoard::default()
        });
        let json = serde_json::to_string(&op).expect("serialise");
        assert!(json.contains("\"comms\""));
        assert!(json.contains("123456789012345678"));
        assert_eq!(serde_json::from_str::<OpContext>(&json).unwrap(), op);
    }

    #[test]
    fn comms_omitted_when_none() {
        let op = OpContext::default();
        assert!(!serde_json::to_string(&op).unwrap().contains("comms"));
    }

    #[test]
    fn v14_op_context_without_comms_key_deserializes() {
        // A v14 payload genuinely lacks the `comms` key (it predates this field).
        let json = r#"{"name":"Op","status":"live","orders":[],"objectives":[],"current_stage":0,"is_lead":false,"squad":[],"my_ready":false,"ready_count":0,"phases":[]}"#;
        let op: OpContext = serde_json::from_str(json).unwrap();
        assert_eq!(op.comms, None);
    }

    #[test]
    fn trigger_member_clip_round_trips() {
        let m = ClientMessage::TriggerMemberClip {
            key: "clip/rally".into(),
        };
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(json.contains("\"type\":\"trigger_member_clip\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn set_clip_policy_round_trips() {
        let m = ClientMessage::SetClipPolicy {
            policy: "urgent".into(),
        };
        let json = serde_json::to_string(&m).expect("serialise");
        assert!(json.contains("\"type\":\"set_clip_policy\""));
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }

    #[test]
    fn member_clip_board_round_trips_on_op_context() {
        let mut op = OpContext {
            name: "Reclaim".into(),
            status: "live".into(),
            ..OpContext::default()
        };
        op.member_clips = Some(MemberClipBoard {
            policy: "all".into(),
            clips: vec![SoundboardClip {
                key: "clip/rally".into(),
                label: "Rally".into(),
            }],
        });
        let json = serde_json::to_string(&op).expect("serialise");
        assert!(json.contains("\"member_clips\""));
        assert_eq!(serde_json::from_str::<OpContext>(&json).unwrap(), op);
    }

    #[test]
    fn member_clips_omitted_when_none() {
        let op = OpContext::default();
        assert!(!serde_json::to_string(&op).unwrap().contains("member_clips"));
    }

    #[test]
    fn v15_op_context_without_member_clips_key_deserializes() {
        // A payload predating this field genuinely lacks the `member_clips` key.
        let json = r#"{"name":"Op","status":"live","orders":[],"objectives":[],"current_stage":0,"is_lead":false,"squad":[],"my_ready":false,"ready_count":0,"phases":[]}"#;
        let op: OpContext = serde_json::from_str(json).unwrap();
        assert_eq!(op.member_clips, None);
    }

    #[test]
    fn comms_board_ticker_round_trips() {
        let board = CommsBoard {
            quick_actions: vec![],
            channels: vec![],
            ticker: vec![
                TickerMessage {
                    author: "Nigel".into(),
                    content: "forming up on the pad".into(),
                },
                TickerMessage {
                    author: "Ada".into(),
                    content: "otw".into(),
                },
            ],
        };
        let json = serde_json::to_string(&board).expect("serialise");
        assert!(json.contains("\"ticker\""));
        assert_eq!(serde_json::from_str::<CommsBoard>(&json).unwrap(), board);
    }

    #[test]
    fn comms_board_without_ticker_key_deserializes_to_empty() {
        // A comms board predating slice 5 genuinely lacks the `ticker` key.
        let json = r#"{"quick_actions":[],"channels":[]}"#;
        let board: CommsBoard = serde_json::from_str(json).unwrap();
        assert!(board.ticker.is_empty());
    }
}
