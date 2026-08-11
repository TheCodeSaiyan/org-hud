//! The HUD's **local control API** (roadmap **C10**, slice 1) — a loopback endpoint external local
//! tools can drive the live op through. Built for the Stream Deck plugin, but deliberately not
//! specific to it.
//!
//! # Why this exists at all
//!
//! There is no "connect the Stream Deck to my app" API. A Stream Deck plugin is a **separate
//! Node.js process** that the Stream Deck app launches and supervises, and the WebSocket it speaks
//! is a private plugin↔Stream Deck channel, not a bus anything else can join. So the hardware
//! cannot reach the HUD; the HUD has to offer somewhere to be reached. Before this module the crate
//! had no server of any kind — `reqwest` and `tokio-tungstenite` were both used purely as clients.
//!
//! # 🔴 Authority: this is transport, not permission
//!
//! The token authorises **reaching the HUD**, nothing more. Every action is relayed over the member
//! socket the HUD already holds, so it lands on exactly the same server-side gate it does today
//! (`live_op_led_by` and friends in `ws_member/handlers`). Three consequences, all deliberate:
//!
//! - **The plugin gets no member token and no server socket of its own.** A second credential living
//!   in a user-readable Node process is the wrong default; leaking the loopback token must not leak
//!   org authority.
//! - **[`resolve`]'s refusals are UX, never enforcement.** They exist so a hardware key can say *why*
//!   it did nothing (see below) — the server re-checks everything regardless, and a caller that
//!   bypassed this module entirely would gain nothing.
//! - **Curated actions, not a generic "send this `ClientMessage`".** An opaque relay would make every
//!   future protocol addition reachable from a local process the moment it shipped. Unknown → deny.
//!
//! # The silent-denial problem this is shaped around
//!
//! A failed gate server-side is a bare `return false` — nothing comes back. On screen that is
//! survivable, because the lead-only controls simply are not rendered. **On a hardware key it is
//! not**: the key lights, nothing happens, and no reason is given. So this API pushes [`KeyState`]
//! and refuses locally with a *reason*, letting the plugin grey a key it knows will fail rather than
//! offering a dead one. That mirrors the rule already learned on the HUD's own board — gate a
//! control's **exposure**, don't ship a dead button.

use hud_protocol::{ClientMessage, OpContext};
use tauri::Manager;

/// Version of the **local** API, negotiated independently of `hud_protocol::PROTOCOL_VERSION`.
///
/// They version different contracts and move for different reasons: `PROTOCOL_VERSION` tracks
/// HUD↔server, this tracks HUD↔local-tool. Note `PROTOCOL_VERSION` is currently advisory — it is
/// sent but never compared — so an external consumer is the first thing that genuinely needs a
/// version it can act on. Bump on any breaking change to the action set or [`KeyState`].
pub const LOCAL_API_VERSION: u32 = 1;

/// Why a connection was refused. Kept separate from a bare bool so the listener can log which rule
/// fired without leaking the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// No token, or the wrong one.
    BadToken,
    /// The handshake carried an `Origin` header — see [`authorize`].
    BrowserOrigin,
    /// No token has been generated yet, so the API is closed.
    NotPaired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Allow,
    Deny(Denial),
}

/// Decide whether a handshake may proceed.
///
/// 🔴 **Binding to `127.0.0.1` is not access control.** A loopback port is reachable by every
/// process on the machine *and by any web page the user visits* — a page can open
/// `ws://127.0.0.1:<port>` and, unlike `fetch`, the same-origin policy does not block the
/// connection itself. So two rules, not one:
///
/// 1. **A token is always required**, even on loopback.
/// 2. **Any handshake carrying an `Origin` header is refused outright.** Browsers are required to
///    send `Origin` on a WebSocket handshake; a native client (the Node plugin) does not send one.
///    That single rule takes every browser-borne attacker off the table without depending on the
///    token staying secret, which matters because Stream Deck's own settings documentation warns
///    that users can read the values stored there.
///
/// Empty `expected` means no token has been generated, and the API stays closed rather than open.
pub fn authorize(token: Option<&str>, origin: Option<&str>, expected: &str) -> Auth {
    if expected.is_empty() {
        return Auth::Deny(Denial::NotPaired);
    }
    if origin.is_some() {
        return Auth::Deny(Denial::BrowserOrigin);
    }
    match token {
        Some(t) if constant_time_eq(t, expected) => Auth::Allow,
        _ => Auth::Deny(Denial::BadToken),
    }
}

/// Compare without an early return on the first differing byte.
///
/// The threat is modest — an attacker who can time this can already run code on the machine — but a
/// token comparison that leaks its prefix is the kind of thing that gets copied into somewhere it
/// matters, so it is written correctly once here.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The subprotocol a local tool offers: `["orghud.v1", "<token>"]`.
pub const SUBPROTOCOL: &str = "orghud.v1";

/// Pull the token out of a `Sec-WebSocket-Protocol` header.
///
/// 🔴 Why this exists at all: the **WHATWG `WebSocket` API cannot set request headers**, and the
/// Stream Deck plugin runs on Node's global `WebSocket`, which is that API — so `Authorization` is
/// simply unavailable to it. Offering the token as a second subprotocol is the standard workaround,
/// and it keeps the secret out of the URL, where a query parameter would end up in any logging that
/// records request lines.
///
/// First entry is the protocol id (which the server must echo back); second is the token. A
/// single-entry header is a client that spoke the protocol but sent no token — a refusal, not a
/// token of `""`.
pub fn token_from_subprotocol(header: Option<&str>) -> Option<&str> {
    let raw = header?;
    let mut parts = raw.split(',').map(str::trim);
    if parts.next()? != SUBPROTOCOL {
        return None;
    }
    parts.next().filter(|t| !t.is_empty())
}

/// What a local tool may ask for. **Curated on purpose** — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    CheckIn,
    ToggleReady,
    ObjectiveNext,
    ObjectivePrev,
    BedPause,
    BedResume,
    TransportNext,
    TransportPrev,
    BedVolume {
        percent: i64,
    },
    TriggerClip {
        key: String,
    },
    /// A member-level quick action ("Downed", "En route", …) into the op's ready room. **Not
    /// lead-gated** — the server requires only that the sender is checked into a live op.
    ///
    /// Named `Quick` rather than `QuickAction` because clippy objects to a variant repeating its
    /// enum's name; the WIRE name stays `quick_action`, which is what the plugin sends.
    #[serde(rename = "quick_action")]
    Quick {
        key: String,
    },
    /// A clip from the member clip board. Not lead-gated; the server additionally enforces the op's
    /// `clip_policy`, a 1 s identical-repeat roll-up and a per-room audio budget.
    MemberClip {
        key: String,
    },
    /// Irreversible. `confirm` must be `true` or this is refused — see [`Action::is_destructive`].
    CompleteOp {
        confirm: bool,
    },
}

/// Why an action was not relayed. Every variant is a *reason a key can display*, which is the whole
/// point: the alternative is a key that lights up and does nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The HUD's own socket to the server is down.
    NotConnected,
    /// No live op in context.
    NoLiveOp,
    /// The action is lead-only and this member does not lead the op.
    NotLead,
    /// The op has no objective rail to move along.
    NoObjectives,
    /// Already at the end (or start) of the rail.
    AtEnd,
    /// No bed is audible, so bed/transport controls would do nothing.
    NoBed,
    /// The clip is not on this member's soundboard.
    UnknownClip,
    /// Volume outside 0..=100.
    OutOfRange,
    /// A destructive action arrived without `confirm: true`.
    NeedsConfirm,
    /// Check-in was asked for on an op this member is already on.
    AlreadyCheckedIn,
    /// The action is not on the board the server pushed for this member.
    UnknownAction,
    /// The op lead has clip triggering switched off.
    ClipsOff,
}

impl Action {
    /// True for actions that cannot be undone. The SDK has no built-in confirmation, so a single
    /// press must not end an op — the plugin is required to make these two-state or long-press.
    pub fn is_destructive(&self) -> bool {
        matches!(self, Action::CompleteOp { .. })
    }

    /// Whether the caller supplied the confirmation a destructive action requires. Non-destructive
    /// actions are trivially confirmed, so the rule in [`resolve`] reads as one sentence.
    fn confirmed(&self) -> bool {
        match self {
            Action::CompleteOp { confirm } => *confirm,
            _ => true,
        }
    }
}

/// Translate an action into the intent the HUD would send, refusing locally when the server-side
/// gate is already known to fail.
///
/// 🔴 This is **not** the authority. It exists so a key can explain itself; the server re-checks
/// every one of these regardless, and skipping this function would gain a caller nothing.
pub fn resolve(
    action: &Action,
    connected: bool,
    ctx: Option<&OpContext>,
    checkin_candidate: Option<&str>,
) -> Result<ClientMessage, Refusal> {
    if !connected {
        return Err(Refusal::NotConnected);
    }

    // 🔴 Check-in is answered BEFORE the context requirement, and that is not a special case for
    // its own sake — it is forced by how context is built. `OpContext` comes from
    // `events::store::current_live_op`, which JOINs `event_checkin`, so **the context exists if and
    // only if you are already checked in**. Requiring a context first therefore made the Check In
    // key refuse in exactly the situation it exists for, and be a no-op in the only situation it
    // could run. The joinable op is supplied separately by the caller.
    if matches!(action, Action::CheckIn) {
        if ctx.is_some() {
            return Err(Refusal::AlreadyCheckedIn);
        }
        return checkin_candidate
            .map(|id| ClientMessage::CheckIn {
                event_id: id.to_string(),
            })
            .ok_or(Refusal::NoLiveOp);
    }

    let ctx = ctx.ok_or(Refusal::NoLiveOp)?;

    // 🔴 Enforced HERE, not left to the plugin. The SDK has no confirmation primitive, and the
    // plugin is a separate process we do not control — so a single stray press must not end an op
    // even if the plugin is buggy, outdated, or someone else's entirely.
    if action.is_destructive() && !action.confirmed() {
        return Err(Refusal::NeedsConfirm);
    }

    // Lead-gated server-side: SetObjective, CompleteOp, SetBed*, Transport*, TriggerClip.
    // CheckIn is NOT lead-gated (its only server guard is `is_live`), and SetReady gates on being
    // checked in rather than leading — so neither is refused here.
    // Mirrors the server: CheckIn's only guard is `is_live`; SetReady, quick actions and member
    // clips gate on `current_live_op` (checked in) rather than on leading. Refusing these for a
    // non-lead would make the deck weaker than the screen for the majority of members.
    let lead_only = !matches!(
        action,
        Action::CheckIn | Action::ToggleReady | Action::Quick { .. } | Action::MemberClip { .. }
    );
    if lead_only && !ctx.is_lead {
        return Err(Refusal::NotLead);
    }

    match action {
        // Handled above, before the context requirement.
        Action::CheckIn => Err(Refusal::AlreadyCheckedIn),
        Action::ToggleReady => Ok(ClientMessage::SetReady {
            ready: !ctx.my_ready,
        }),
        Action::ObjectiveNext | Action::ObjectivePrev => {
            let count = ctx.objectives.len();
            if count == 0 {
                return Err(Refusal::NoObjectives);
            }
            // `count` itself is the valid all-complete sentinel, so the rail is 0..=count.
            let next = if matches!(action, Action::ObjectiveNext) {
                if ctx.current_stage >= count {
                    return Err(Refusal::AtEnd);
                }
                ctx.current_stage + 1
            } else {
                if ctx.current_stage == 0 {
                    return Err(Refusal::AtEnd);
                }
                ctx.current_stage - 1
            };
            Ok(ClientMessage::SetObjective { index: next })
        }
        Action::BedPause | Action::BedResume | Action::TransportNext | Action::TransportPrev => {
            if !bed_is_audible(ctx) {
                return Err(Refusal::NoBed);
            }
            Ok(match action {
                Action::BedPause => ClientMessage::SetBedPaused { paused: true },
                Action::BedResume => ClientMessage::SetBedPaused { paused: false },
                Action::TransportNext => ClientMessage::TransportNext,
                _ => ClientMessage::TransportPrev,
            })
        }
        Action::BedVolume { percent } => {
            if !(0..=100).contains(percent) {
                return Err(Refusal::OutOfRange);
            }
            if !bed_is_audible(ctx) {
                return Err(Refusal::NoBed);
            }
            Ok(ClientMessage::SetBedVolume { percent: *percent })
        }
        Action::TriggerClip { key } => {
            // The soundboard is already filtered server-side to `hud`-tagged clips this member may
            // fire, so membership of that list is the right local check — and it means a stale
            // plugin config cannot name an arbitrary asset.
            if !ctx.soundboard.iter().any(|c| &c.key == key) {
                return Err(Refusal::UnknownClip);
            }
            Ok(ClientMessage::TriggerClip { key: key.clone() })
        }
        Action::Quick { key } => {
            // Validated against the board the SERVER pushed, exactly as `TriggerClip` is: a stale
            // plugin config must not be able to name an action this member was never offered.
            if !ctx
                .comms
                .as_ref()
                .is_some_and(|c| c.quick_actions.iter().any(|q| &q.key == key))
            {
                return Err(Refusal::UnknownAction);
            }
            // `channel: None` = the op's ready room. The deck deliberately offers no channel
            // picker: an unlabelled key that posts somewhere else is a good way to say the wrong
            // thing in the wrong place.
            Ok(ClientMessage::SendQuickAction {
                channel: None,
                action: key.clone(),
            })
        }
        Action::MemberClip { key } => {
            let board = ctx.member_clips.as_ref().ok_or(Refusal::ClipsOff)?;
            if board.policy == "off" {
                return Err(Refusal::ClipsOff);
            }
            if !board.clips.iter().any(|c| &c.key == key) {
                return Err(Refusal::UnknownClip);
            }
            Ok(ClientMessage::TriggerMemberClip { key: key.clone() })
        }
        Action::CompleteOp { .. } => Ok(ClientMessage::CompleteOp),
    }
}

fn bed_is_audible(ctx: &OpContext) -> bool {
    ctx.music.as_ref().is_some_and(|m| m.active)
}

/// Pushed to the local tool so it can render honest keys instead of dead ones.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KeyState {
    pub api_version: u32,
    /// The HUD's own socket to the server. False ⇒ every key should read as disconnected.
    pub connected: bool,
    pub live_op: bool,
    pub op_name: Option<String>,
    /// A live op this member could check into, when they are not already on one. Without this a
    /// Check In key has nothing to act on — see the note in [`resolve`].
    pub checkin: Option<String>,
    pub is_lead: bool,
    pub my_ready: bool,
    pub bed_active: bool,
    pub bed_paused: bool,
    /// Effective bed volume 0..=100. Without this a rotary control has to invent a starting point,
    /// so its first turn jumps the volume to wherever that guess was rather than nudging from where
    /// the bed actually is.
    pub bed_volume: u32,
    /// `(current_stage, objective_count)`; the rail is `0..=count`.
    pub objective: Option<(usize, usize)>,
    pub now_playing: Option<String>,
    /// Keys of the `hud`-tagged clips this member may fire — the plugin's property inspector
    /// populates its picker from this rather than guessing.
    pub clips: Vec<String>,
    /// Member-level quick actions, as `(key, label)`. Available to every member on the op, not just
    /// the lead — this is what makes a deck worth owning if you are not leading.
    pub quick_actions: Vec<(String, String)>,
    /// Member clip board, as `(key, label)`. Empty when the lead has clips off.
    pub member_clips: Vec<(String, String)>,
}

/// Derive the pushed state. Total: a `None` context yields a fully-disabled board rather than an
/// absent one, so the plugin always has something honest to draw.
pub fn key_state(connected: bool, ctx: Option<&OpContext>, checkin: Option<&str>) -> KeyState {
    let base = KeyState {
        api_version: LOCAL_API_VERSION,
        connected,
        live_op: false,
        op_name: None,
        checkin: None,
        is_lead: false,
        my_ready: false,
        bed_active: false,
        bed_paused: false,
        bed_volume: 0,
        objective: None,
        now_playing: None,
        clips: Vec::new(),
        quick_actions: Vec::new(),
        member_clips: Vec::new(),
    };
    let Some(ctx) = ctx else {
        // Not on an op: the only thing a board can offer is a way to join one.
        return KeyState {
            checkin: checkin.map(str::to_string),
            ..base
        };
    };
    KeyState {
        live_op: true,
        op_name: Some(ctx.name.clone()),
        is_lead: ctx.is_lead,
        my_ready: ctx.my_ready,
        bed_active: bed_is_audible(ctx),
        bed_paused: ctx.music.as_ref().is_some_and(|m| m.paused),
        bed_volume: ctx.music.as_ref().map(|m| m.volume).unwrap_or(0),
        objective: Some((ctx.current_stage, ctx.objectives.len())),
        now_playing: ctx
            .music
            .as_ref()
            .and_then(|m| m.now_playing.as_ref())
            .and_then(|t| t.title.clone()),
        clips: ctx.soundboard.iter().map(|c| c.key.clone()).collect(),
        quick_actions: ctx
            .comms
            .as_ref()
            .map(|c| {
                c.quick_actions
                    .iter()
                    .map(|q| (q.key.clone(), q.label.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        member_clips: ctx
            .member_clips
            .as_ref()
            .filter(|b| b.policy != "off")
            .map(|b| {
                b.clips
                    .iter()
                    .map(|c| (c.key.clone(), c.label.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        ..base
    }
}

/// Default loopback port. Fixed rather than ephemeral so the plugin has something to default to;
/// overridable from HUD settings for the rare clash.
pub const DEFAULT_PORT: u16 = 48291;

/// Keyring entry for the pairing token — **separate from the member token** so that leaking the one
/// a user pastes into a third-party plugin's settings cannot leak org authority.
const TOKEN_KEY: &str = "streamdeck-token";

/// Read the pairing token, generating one on first use. Empty string on any keyring failure, which
/// [`authorize`] treats as "not paired" — closed, never open.
pub fn pairing_token(service: &str) -> String {
    let Ok(entry) = keyring::Entry::new(service, TOKEN_KEY) else {
        return String::new();
    };
    if let Ok(existing) = entry.get_password() {
        if !existing.is_empty() {
            return existing;
        }
    }
    let fresh = uuid::Uuid::new_v4().simple().to_string();
    match entry.set_password(&fresh) {
        Ok(()) => fresh,
        // Could not persist it — returning it anyway would authorise a token that vanishes on
        // restart, which reads as "pairing randomly stops working". Stay closed instead.
        Err(_) => String::new(),
    }
}

/// Forget the pairing token, revoking every plugin paired with this HUD.
pub fn revoke_token(service: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(service, TOKEN_KEY).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// One request from a local tool. The optional `id` is echoed back on the reply.
///
/// 🔴 Correlation is not a nicety. Without it a caller can only assume the next reply belongs to
/// its last request, so two keys pressed in quick succession can land each other's outcome — a key
/// flashing OK for a refusal that was really about a different key.
#[derive(Debug, Clone, serde::Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<u64>,
    #[serde(flatten)]
    action: Action,
}

/// What the HUD sends back over the local socket.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    /// Pushed on connect, on every context change, and whenever the member socket goes up or down.
    State(KeyState),
    Ack {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
    },
    /// A refusal WITH a reason, so a key can explain itself rather than going dead.
    Nack {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        reason: String,
    },
}

impl Refusal {
    /// Stable wire names — the plugin switches on these to pick a key face.
    fn wire(&self) -> &'static str {
        match self {
            Refusal::NotConnected => "not_connected",
            Refusal::NoLiveOp => "no_live_op",
            Refusal::NotLead => "not_lead",
            Refusal::NoObjectives => "no_objectives",
            Refusal::AtEnd => "at_end",
            Refusal::NoBed => "no_bed",
            Refusal::UnknownClip => "unknown_clip",
            Refusal::OutOfRange => "out_of_range",
            Refusal::NeedsConfirm => "needs_confirm",
            Refusal::AlreadyCheckedIn => "already_checked_in",
            Refusal::UnknownAction => "unknown_action",
            Refusal::ClipsOff => "clips_off",
        }
    }
}

/// Run the loopback listener for the lifetime of the app.
///
/// Best-effort throughout: a bind failure means the feature is simply absent, never a dead overlay.
/// Bound to `127.0.0.1` so it is not reachable off-machine — but see [`authorize`] for why that is
/// necessary and not sufficient.
/// Whether the listener is actually bound right now.
///
/// The toggle is read once at startup, so between ticking it and restarting there is a window where
/// the setting says yes and nothing is listening. Surfacing the real state stops the pairing key
/// looking live when it is not — exactly the trap the first user hit.
static LISTENING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True when the loopback listener is bound and accepting.
pub fn is_listening() -> bool {
    LISTENING.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn spawn(app: tauri::AppHandle, port: u16, service: &'static str) {
    tauri::async_runtime::spawn(async move {
        let listener =
            match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("local api: cannot bind 127.0.0.1:{port}: {e}");
                    return;
                }
            };
        LISTENING.store(true, std::sync::atomic::Ordering::Relaxed);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                handle_conn(app, stream, service).await;
            });
        }
    });
}

/// The large `Err` is `tungstenite`'s own `ErrorResponse`, which its handshake callback requires —
/// boxing it is not an option the API allows.
#[allow(clippy::result_large_err)]
async fn handle_conn(app: tauri::AppHandle, stream: tokio::net::TcpStream, service: &'static str) {
    let expected = pairing_token(service);
    let mut verdict = Auth::Deny(Denial::BadToken);

    // Inspect the handshake rather than trusting a post-connect hello: a client that never
    // authenticates should not get as far as an open socket.
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         res: tokio_tungstenite::tungstenite::handshake::server::Response| {
            let header = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok());
            // Either transport: a native client can set `Authorization`; a WHATWG `WebSocket`
            // client (Node's global, which the Stream Deck plugin uses) cannot, and offers the
            // token as a subprotocol instead.
            let offered = header("sec-websocket-protocol");
            let token = header("authorization")
                .and_then(|v| v.strip_prefix("Bearer ").map(str::trim))
                .or_else(|| token_from_subprotocol(offered));
            verdict = authorize(token, header("origin"), &expected);
            match verdict {
                Auth::Allow => {
                    let mut res = res;
                    // The handshake REQUIRES echoing exactly one offered subprotocol; omit it and a
                    // conforming client drops the connection right after a "successful" upgrade.
                    // RFC 6455 §4.1: the server may only select a subprotocol the client
                    // OFFERED. A native client that authenticates via `Authorization` but happens
                    // to offer some unrelated subprotocol would otherwise be told it selected
                    // `orghud.v1` — which it never offered — and must then fail the connection,
                    // with no diagnostic. Condition on OURS being present, not on any being.
                    if token_from_subprotocol(offered).is_some() {
                        if let Ok(v) = SUBPROTOCOL.parse() {
                            res.headers_mut().insert("sec-websocket-protocol", v);
                        }
                    }
                    Ok(res)
                }
                Auth::Deny(_) => Err(tokio_tungstenite::tungstenite::http::Response::builder()
                    .status(403)
                    .body(Some("forbidden".to_string()))
                    .expect("a static 403 response always builds")),
            }
        },
    )
    .await;

    let mut ws = match ws {
        Ok(ws) => ws,
        Err(_) => {
            // Log WHICH rule fired — never the token itself.
            if let Auth::Deny(d) = verdict {
                eprintln!("local api: refused a connection ({d:?})");
            }
            return;
        }
    };

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let state = app.state::<crate::AppState>();
    let mut rx = state.local_state_tx.subscribe();

    // Send the board immediately, so a freshly-launched plugin draws honest keys rather than
    // waiting up to a whole context tick with dead ones.
    if let Ok(txt) = serde_json::to_string(&Response::State(current_key_state(&app))) {
        let _ = ws.send(Message::Text(txt)).await;
    }

    loop {
        tokio::select! {
            pushed = rx.recv() => match pushed {
                Ok(txt) => { if ws.send(Message::Text(txt)).await.is_err() { break; } }
                // Lagged: the plugin missed some states. The next push is a full snapshot, so
                // dropping the gap is correct — this is state, not a command stream.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            inbound = ws.next() => match inbound {
                Some(Ok(Message::Text(txt))) => {
                    let reply = handle_request(&app, &txt);
                    let Ok(json) = serde_json::to_string(&reply) else { continue };
                    if ws.send(Message::Text(json)).await.is_err() { break; }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
        }
    }
}

/// Parse, resolve and relay one request. Split out so the whole decision path is one expression a
/// reader can follow: unknown text → nack, refused → nack WITH a reason, allowed → relay.
fn handle_request(app: &tauri::AppHandle, text: &str) -> Response {
    // An unrecognised tag lands here, which is the load-bearing case: unknown action → deny. The
    // reply carries no id because we could not parse one.
    let Ok(req) = serde_json::from_str::<Request>(text) else {
        return Response::Nack {
            id: None,
            reason: "unknown_action".into(),
        };
    };
    let id = req.id;
    let state = app.state::<crate::AppState>();
    let connected = state.member_tx.lock().map(|t| t.is_some()).unwrap_or(false);
    let ctx = state.local_last_ctx.lock().ok().and_then(|c| c.clone());

    let checkin = state.local_checkin.lock().ok().and_then(|c| c.clone());
    match resolve(&req.action, connected, ctx.as_ref(), checkin.as_deref()) {
        Err(r) => Response::Nack {
            id,
            reason: r.wire().into(),
        },
        Ok(msg) => match crate::send_member(app, msg) {
            Ok(()) => Response::Ack { id },
            // The socket dropped between the check and the send.
            Err(_) => Response::Nack {
                id,
                reason: Refusal::NotConnected.wire().into(),
            },
        },
    }
}

fn current_key_state(app: &tauri::AppHandle) -> KeyState {
    let state = app.state::<crate::AppState>();
    let connected = state.member_tx.lock().map(|t| t.is_some()).unwrap_or(false);
    let ctx = state.local_last_ctx.lock().ok().and_then(|c| c.clone());
    let checkin = state.local_checkin.lock().ok().and_then(|c| c.clone());
    key_state(connected, ctx.as_ref(), checkin.as_deref())
}

/// A live op this member could check into, refreshed only while they are NOT on one — which is the
/// only time it is needed, and keeps this off the hot path for the common case.
///
/// Cheap-but-not-free (one authed REST call), so it is TTL'd rather than run on every 15 s push.
async fn refresh_checkin_candidate(app: &tauri::AppHandle) {
    const TTL: std::time::Duration = std::time::Duration::from_secs(30);
    {
        let state = app.state::<crate::AppState>();
        // Already on an op ⇒ nothing to join, and `resolve` refuses with `already_checked_in`.
        if state
            .local_last_ctx
            .lock()
            .map(|c| c.is_some())
            .unwrap_or(false)
        {
            if let Ok(mut slot) = state.local_checkin.lock() {
                *slot = None;
            }
            return;
        }
        let fresh = state
            .local_checkin_at
            .lock()
            .ok()
            .and_then(|at| *at)
            .map(|t| t.elapsed() < TTL)
            .unwrap_or(false);
        if fresh {
            return;
        }
    }
    let found = crate::api_get_json(app, "/api/events").await.and_then(|v| {
        v.as_array()?
            .iter()
            .find(|e| e.get("status").and_then(|s| s.as_str()) == Some("live"))
            .and_then(|e| e.get("id").and_then(|i| i.as_str()).map(str::to_string))
    });
    {
        let state = app.state::<crate::AppState>();
        // Bind the guards BEFORE using them: an `if let` directly on `state.x.lock()` keeps the
        // temporary alive past `state`, which does not borrow-check. Locals drop in reverse
        // declaration order, so binding first puts the guards away before `state` goes.
        let slot = state.local_checkin.lock();
        let at = state.local_checkin_at.lock();
        if let Ok(mut slot) = slot {
            *slot = found;
        }
        if let Ok(mut at) = at {
            *at = Some(std::time::Instant::now());
        }
    }
}

/// Called from the context handler on every push: remember the context, then fan out.
pub async fn on_context(app: &tauri::AppHandle, op: Option<&OpContext>) {
    if let Ok(mut slot) = app.state::<crate::AppState>().local_last_ctx.lock() {
        *slot = op.cloned();
    }
    push_board(app).await;
}

/// Re-derive the board and fan it out.
///
/// 🔴 Must be called on **connection changes too**, not just context messages. `connected` is
/// derived from the member socket, and the socket can drop without any further context arriving —
/// so a board pushed only on context would keep asserting `connected: true` for as long as the
/// outage lasted, leaving every key lit and lying. The press path re-checks liveness anyway, so the
/// user would get a refusal rather than silence, but a key that looks available and is not is
/// exactly the confusion this API exists to remove.
pub async fn push_board(app: &tauri::AppHandle) {
    refresh_checkin_candidate(app).await;
    push_board_now(app);
}

/// Fan out without refreshing the candidate — for callers already on a hot path.
pub fn push_board_now(app: &tauri::AppHandle) {
    let state = app.state::<crate::AppState>();
    if let Ok(txt) = serde_json::to_string(&Response::State(current_key_state(app))) {
        // Fails only when nothing is connected, which is the normal case.
        let _ = state.local_state_tx.send(txt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hud_protocol::{MusicState, SoundboardClip, TrackMeta};

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

    fn ctx(is_lead: bool) -> OpContext {
        OpContext {
            is_lead,
            music: Some(music(true)),
            ..Default::default()
        }
    }

    // ---- authorize -------------------------------------------------------

    #[test]
    fn a_browser_handshake_is_refused_even_with_the_right_token() {
        // The rule that makes loopback safe: a web page the user visits can open a ws:// to
        // 127.0.0.1, and the same-origin policy does not stop the connection. Browsers must send
        // `Origin`; the Node plugin does not.
        assert_eq!(
            authorize(Some("good"), Some("https://evil.example"), "good"),
            Auth::Deny(Denial::BrowserOrigin)
        );
        // Even a same-origin-looking page is refused — any Origin at all means a browser.
        assert_eq!(
            authorize(Some("good"), Some("http://127.0.0.1:1234"), "good"),
            Auth::Deny(Denial::BrowserOrigin)
        );
    }

    #[test]
    fn a_native_client_with_the_right_token_is_allowed() {
        assert_eq!(authorize(Some("good"), None, "good"), Auth::Allow);
    }

    #[test]
    fn a_wrong_or_missing_token_is_refused() {
        assert_eq!(
            authorize(Some("bad"), None, "good"),
            Auth::Deny(Denial::BadToken)
        );
        assert_eq!(authorize(None, None, "good"), Auth::Deny(Denial::BadToken));
        // A prefix must not pass — the comparison is length-checked first.
        assert_eq!(
            authorize(Some("goo"), None, "good"),
            Auth::Deny(Denial::BadToken)
        );
        // 🔴 SAME LENGTH, different bytes. Without this case every wrong-token assertion above is
        // satisfied by the length check alone, so the byte comparison itself goes untested — a
        // mutation that made `constant_time_eq` return `true` unconditionally survived until this
        // line existed.
        assert_eq!(
            authorize(Some("gooX"), None, "good"),
            Auth::Deny(Denial::BadToken)
        );
        assert_eq!(
            authorize(Some("doog"), None, "good"),
            Auth::Deny(Denial::BadToken)
        );
    }

    #[test]
    fn an_unpaired_hud_stays_closed_rather_than_open() {
        // An empty expected token must not mean "accept anything".
        assert_eq!(authorize(Some(""), None, ""), Auth::Deny(Denial::NotPaired));
        assert_eq!(authorize(None, None, ""), Auth::Deny(Denial::NotPaired));
    }

    #[test]
    fn a_token_can_arrive_as_a_subprotocol_because_the_browser_api_cannot_set_headers() {
        assert_eq!(
            token_from_subprotocol(Some("orghud.v1, abc123")),
            Some("abc123")
        );
        assert_eq!(
            token_from_subprotocol(Some("orghud.v1,abc123")),
            Some("abc123")
        );
    }

    #[test]
    fn a_subprotocol_without_our_id_or_without_a_token_yields_nothing() {
        assert_eq!(token_from_subprotocol(None), None);
        assert_eq!(token_from_subprotocol(Some("someone.else, abc")), None);
        // Spoke the protocol but sent no token: a refusal, NOT a token of "".
        assert_eq!(token_from_subprotocol(Some("orghud.v1")), None);
        assert_eq!(token_from_subprotocol(Some("orghud.v1, ")), None);
    }

    // ---- resolve ---------------------------------------------------------

    #[test]
    fn a_down_socket_refuses_before_anything_else() {
        assert_eq!(
            resolve(&Action::CheckIn, false, Some(&ctx(true)), None),
            Err(Refusal::NotConnected)
        );
    }

    #[test]
    fn lead_only_actions_refuse_for_a_non_lead_with_a_reason() {
        // The whole point: a REASON, not silence, so the key can grey rather than lie.
        for a in [
            Action::ObjectiveNext,
            Action::BedPause,
            Action::TransportNext,
            Action::CompleteOp { confirm: true },
        ] {
            assert_eq!(
                resolve(&a, true, Some(&ctx(false)), None),
                Err(Refusal::NotLead),
                "{a:?}"
            );
        }
    }

    #[test]
    fn check_in_works_when_there_is_no_context_which_is_the_only_time_it_matters() {
        // 🔴 The bug this encodes: `OpContext` comes from `current_live_op`, which JOINs
        // `event_checkin` — so the context exists IF AND ONLY IF you are already checked in.
        // Requiring a context first made this key refuse in exactly the situation it exists for.
        assert_eq!(
            resolve(&Action::CheckIn, true, None, Some("ev-1")),
            Ok(ClientMessage::CheckIn {
                event_id: "ev-1".into()
            })
        );
    }

    #[test]
    fn check_in_without_a_joinable_op_says_so() {
        assert_eq!(
            resolve(&Action::CheckIn, true, None, None),
            Err(Refusal::NoLiveOp)
        );
    }

    #[test]
    fn check_in_while_already_on_the_op_is_refused_rather_than_being_a_silent_no_op() {
        // Previously this reached the server and returned `newly = false` — nothing happened, and
        // the key flashed OK for it.
        assert_eq!(
            resolve(&Action::CheckIn, true, Some(&ctx(false)), Some("ev-1")),
            Err(Refusal::AlreadyCheckedIn)
        );
    }

    #[test]
    fn a_board_with_no_op_still_offers_something_to_join() {
        let s = key_state(true, None, Some("ev-9"));
        assert!(!s.live_op);
        assert_eq!(s.checkin.as_deref(), Some("ev-9"));
        // …and once you are on an op there is nothing left to join.
        assert_eq!(
            key_state(true, Some(&ctx(true)), Some("ev-9")).checkin,
            None
        );
    }

    #[test]
    fn check_in_and_ready_are_not_lead_gated() {
        // Mirrors the server: CheckIn's only guard is `is_live`, and SetReady gates on being
        // checked in. Refusing them for a non-lead would make the deck weaker than the screen.
        let c = ctx(false);
        assert_eq!(
            resolve(&Action::ToggleReady, true, Some(&c), None),
            Ok(ClientMessage::SetReady { ready: true })
        );
    }

    #[test]
    fn ready_toggles_against_the_current_state() {
        let mut c = ctx(false);
        c.my_ready = true;
        assert_eq!(
            resolve(&Action::ToggleReady, true, Some(&c), None),
            Ok(ClientMessage::SetReady { ready: false })
        );
    }

    #[test]
    fn the_objective_rail_is_inclusive_of_the_all_complete_sentinel() {
        let mut c = ctx(true);
        c.objectives = vec![Default::default(), Default::default()];
        c.current_stage = 1;
        assert_eq!(
            resolve(&Action::ObjectiveNext, true, Some(&c), None),
            Ok(ClientMessage::SetObjective { index: 2 })
        );
        // 2 == count is the all-complete sentinel and IS reachable; 3 is not.
        c.current_stage = 2;
        assert_eq!(
            resolve(&Action::ObjectiveNext, true, Some(&c), None),
            Err(Refusal::AtEnd)
        );
    }

    #[test]
    fn objective_prev_stops_at_zero_rather_than_underflowing() {
        let mut c = ctx(true);
        c.objectives = vec![Default::default()];
        c.current_stage = 0;
        // usize underflow here would panic in release-with-overflow-checks and wrap otherwise.
        assert_eq!(
            resolve(&Action::ObjectivePrev, true, Some(&c), None),
            Err(Refusal::AtEnd)
        );
    }

    #[test]
    fn an_op_with_no_objectives_refuses_rather_than_sending_index_zero() {
        let c = ctx(true);
        assert_eq!(
            resolve(&Action::ObjectiveNext, true, Some(&c), None),
            Err(Refusal::NoObjectives)
        );
    }

    #[test]
    fn bed_controls_refuse_when_no_bed_is_audible() {
        let mut c = ctx(true);
        c.music = Some(music(false));
        for a in [
            Action::BedPause,
            Action::TransportNext,
            Action::BedVolume { percent: 50 },
        ] {
            assert_eq!(
                resolve(&a, true, Some(&c), None),
                Err(Refusal::NoBed),
                "{a:?}"
            );
        }
    }

    #[test]
    fn volume_is_range_checked_before_anything_is_sent() {
        let c = ctx(true);
        assert_eq!(
            resolve(&Action::BedVolume { percent: 101 }, true, Some(&c), None),
            Err(Refusal::OutOfRange)
        );
        assert_eq!(
            resolve(&Action::BedVolume { percent: -1 }, true, Some(&c), None),
            Err(Refusal::OutOfRange)
        );
        assert_eq!(
            resolve(&Action::BedVolume { percent: 0 }, true, Some(&c), None),
            Ok(ClientMessage::SetBedVolume { percent: 0 })
        );
    }

    #[test]
    fn a_clip_not_on_the_soundboard_is_refused() {
        // A stale plugin config must not be able to name an arbitrary asset. The soundboard is
        // already server-filtered to `hud`-tagged clips this member may fire.
        let mut c = ctx(true);
        c.soundboard = vec![SoundboardClip {
            key: "clip/rally".into(),
            ..Default::default()
        }];
        assert_eq!(
            resolve(
                &Action::TriggerClip {
                    key: "clip/nope".into()
                },
                true,
                Some(&c),
                None
            ),
            Err(Refusal::UnknownClip)
        );
        assert_eq!(
            resolve(
                &Action::TriggerClip {
                    key: "clip/rally".into()
                },
                true,
                Some(&c),
                None
            ),
            Ok(ClientMessage::TriggerClip {
                key: "clip/rally".into()
            })
        );
    }

    #[test]
    fn completing_an_op_is_marked_destructive_so_the_plugin_must_confirm() {
        // The SDK has no built-in confirmation; a single press must not be able to end an op.
        assert!(Action::CompleteOp { confirm: true }.is_destructive());
        // And the HUD refuses it outright without the flag, whatever the plugin believes.
        assert_eq!(
            resolve(
                &Action::CompleteOp { confirm: false },
                true,
                Some(&ctx(true)),
                None
            ),
            Err(Refusal::NeedsConfirm)
        );
        assert_eq!(
            resolve(
                &Action::CompleteOp { confirm: true },
                true,
                Some(&ctx(true)),
                None
            ),
            Ok(ClientMessage::CompleteOp)
        );
        for a in [Action::CheckIn, Action::ObjectiveNext, Action::BedPause] {
            assert!(!a.is_destructive(), "{a:?}");
        }
    }

    #[test]
    fn the_board_carries_the_current_volume_so_a_dial_starts_from_the_truth() {
        let mut c = ctx(true);
        let mut m = music(true);
        m.volume = 35;
        c.music = Some(m);
        assert_eq!(key_state(true, Some(&c), None).bed_volume, 35);
        // No op at all: 0 rather than a made-up default, so a dial has nothing to jump to.
        assert_eq!(key_state(false, None, None).bed_volume, 0);
    }

    #[test]
    fn a_dropped_member_socket_pushes_a_fresh_board() {
        // `connected` is derived from the member socket, and the socket can drop with no further
        // context arriving — so a board pushed ONLY on context would assert `connected: true` for
        // the whole outage, leaving every key lit and lying. Pin the call on the disconnect path.
        const MAIN: &str = include_str!("main.rs");
        // Positional: `nth(2)` is the text AFTER the second emit. Assert the count too, or adding
        // a third emit anywhere silently moves this window somewhere meaningless.
        let parts: Vec<_> = MAIN
            .split("let _ = handle.emit(\"hud:context-stale\", ());")
            .collect();
        assert_eq!(
            parts.len(),
            3,
            "expected exactly 2 context-stale emits; this guard indexes them positionally"
        );
        let down = parts[2];
        let window = &down[..down.len().min(400)];
        assert!(
            window.contains("local_api::push_board"),
            "the socket-down path must push a board to local tools, or their view of `connected`              stays true until a context message that cannot arrive while the socket is down"
        );
    }

    #[test]
    fn a_reply_echoes_the_request_id_so_outcomes_land_on_the_right_key() {
        let req: Request = serde_json::from_str(r#"{"action":"check_in","id":7}"#).expect("parse");
        assert_eq!(req.id, Some(7));
        assert_eq!(req.action, Action::CheckIn);
        // Optional, so a caller that does not correlate still works.
        let bare: Request = serde_json::from_str(r#"{"action":"check_in"}"#).expect("parse");
        assert_eq!(bare.id, None);
        let json = serde_json::to_string(&Response::Nack {
            id: Some(7),
            reason: "not_lead".into(),
        })
        .expect("serialize");
        assert!(
            json.contains(r#""id":7"#) && json.contains(r#""reason":"not_lead""#),
            "{json}"
        );
    }

    #[test]
    fn an_unknown_action_is_still_refused_after_dropping_deny_unknown_fields() {
        // `flatten` is incompatible with `deny_unknown_fields`, so that attribute had to go. The
        // half that matters — an unrecognised action is rejected — must still hold.
        assert!(serde_json::from_str::<Request>(r#"{"action":"rm_rf"}"#).is_err());
    }

    #[test]
    fn nothing_in_this_module_can_send_without_going_through_resolve() {
        // 🔴 THE security claim of the whole feature: the token is transport, not authority, and a
        // local tool cannot mutate anything without passing the same checks the on-screen controls
        // do. That rests on there being exactly ONE send path here, guarded by `resolve`. A second
        // `send_member` call added later — a convenience shortcut, a "just relay this" escape
        // hatch — would quietly undo it, and nothing else in the suite would notice.
        const SRC: &str = include_str!("local_api.rs");
        let prod = SRC.split("#[cfg(test)]").next().expect("the non-test half");
        let sends: Vec<_> = prod.match_indices("send_member(").collect();
        assert_eq!(
            sends.len(),
            1,
            "expected exactly one send path in local_api.rs, found {}. Every send MUST sit inside              the `resolve` match, or a local tool gains a route that skips the local checks.",
            sends.len()
        );
        // …and that one call must be downstream of a `resolve`.
        let resolve_at = prod.find("match resolve(").expect("the resolve match");
        assert!(
            sends[0].0 > resolve_at,
            "the send must be inside the `resolve` match, not before it"
        );
    }

    #[test]
    fn we_only_echo_the_subprotocol_when_ours_was_offered() {
        // A client offering something else entirely must not be told it selected ours — RFC 6455
        // requires it to fail the connection, which would look like an unexplained instant drop.
        assert!(token_from_subprotocol(Some("chat")).is_none());
        assert!(token_from_subprotocol(Some("chat, orghud.v1")).is_none());
        assert!(token_from_subprotocol(Some("orghud.v1, tok")).is_some());
    }

    // ---- the Stream Deck plugin (C10 slice 2) ----------------------------
    //
    // These live in Rust, not Node, for one reason: CI runs `cargo`, not `node`. A drift guard the
    // build never executes is decoration.

    const PLUGIN_JS: &str =
        include_str!("../../streamdeck/com.thecodesaiyan.orghud.sdPlugin/bin/plugin.js");
    const MANIFEST: &str =
        include_str!("../../streamdeck/com.thecodesaiyan.orghud.sdPlugin/manifest.json");

    /// Every `action: 'x'` literal the plugin sends.
    fn actions_sent_by_plugin() -> Vec<String> {
        let mut out = Vec::new();
        for (i, _) in PLUGIN_JS.match_indices("action: '") {
            let rest = &PLUGIN_JS[i + "action: '".len()..];
            if let Some(end) = rest.find('\'') {
                out.push(rest[..end].to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn every_action_the_plugin_sends_exists_in_this_enum() {
        let sent = actions_sent_by_plugin();
        assert!(
            !sent.is_empty(),
            "extraction found nothing — the guard would pass vacuously"
        );
        for name in &sent {
            let probe = format!(r#"{{"action":"{name}"}}"#);
            if let Err(e) = serde_json::from_str::<Action>(&probe) {
                // A MISSING FIELD is fine — it proves the tag was recognised and the variant simply
                // carries data. An UNKNOWN VARIANT means the plugin sends something the HUD would
                // reject at runtime as `unknown_action`, i.e. a dead key with no way to find out why.
                assert!(
                    !e.to_string().contains("unknown variant"),
                    "the Stream Deck plugin sends `{name}`, which is not an Action variant — that                      key would be silently refused as `unknown_action`. Either add the variant or                      fix the plugin."
                );
            }
        }
    }

    #[test]
    fn the_plugin_and_the_hud_agree_on_the_subprotocol() {
        // A mismatch here means the token is never found, every connection is refused as
        // `BadToken`, and the plugin looks simply broken with no clue as to why.
        assert!(
            PLUGIN_JS.contains(&format!("const SUBPROTOCOL = '{SUBPROTOCOL}'")),
            "plugin.js must offer the same subprotocol id the HUD echoes ({SUBPROTOCOL})"
        );
    }

    #[test]
    fn the_plugin_defaults_to_the_same_port_the_hud_binds() {
        assert!(
            PLUGIN_JS.contains(&format!("const DEFAULT_HUD_PORT = {DEFAULT_PORT}")),
            "plugin.js's default port must match the HUD's DEFAULT_PORT ({DEFAULT_PORT})"
        );
    }

    #[test]
    fn every_manifest_action_is_handled_by_the_plugin() {
        // A manifest action with no handler is a key the user can place on their deck that does
        // nothing at all — the exact dead-control failure this whole feature is shaped to avoid.
        let mut uuids = Vec::new();
        for (i, _) in MANIFEST.match_indices(r#""UUID": ""#) {
            let rest = &MANIFEST[i + r#""UUID": ""#.len()..];
            if let Some(end) = rest.find('"') {
                uuids.push(rest[..end].to_string());
            }
        }
        assert!(!uuids.is_empty(), "no action UUIDs found — vacuous guard");
        for u in &uuids {
            // 🔴 A UUID must appear in a *press* HANDLER, not merely somewhere. Earlier versions
            // accepted any mention, then any mention plus `'{u}',` — and `'{u}',` matches mere
            // membership of a Set (e.g. UNCONFIRMED), which is classification, not handling. A
            // deleted handler therefore still passed. Accept only the two real dispatch forms.
            //
            // The encoder is the one legitimate exception: dial events are routed by EVENT TYPE
            // (`dialRotate`/`dialDown`), so its uuid never appears in a comparison at all.
            const EVENT_ROUTED: &[&str] = &["com.thecodesaiyan.orghud.volume"];
            if EVENT_ROUTED.contains(&u.as_str()) {
                continue;
            }
            let handled = PLUGIN_JS.contains(&format!("'{u}':"))
                || PLUGIN_JS.contains(&format!("uuid === '{u}'"));
            assert!(
                handled,
                "manifest declares action `{u}` but plugin.js has no press handler for it — a                  mention in a paint rule or a Set is classification, not handling, and that key                  would sit on a deck doing nothing"
            );
        }
    }

    #[test]
    fn completing_an_op_from_a_deck_requires_confirmation_on_both_sides() {
        // Belt and braces, deliberately: the plugin arms-then-fires, AND the HUD refuses without
        // `confirm: true`. Either alone is a single stray press away from ending someone's op.
        assert!(
            PLUGIN_JS.contains("complete_op', confirm: true")
                || PLUGIN_JS.contains(r#"complete_op", confirm: true"#),
            "the plugin must send confirm:true only on the second press"
        );
        assert_eq!(
            resolve(
                &Action::CompleteOp { confirm: false },
                true,
                Some(&ctx(true)),
                None
            ),
            Err(Refusal::NeedsConfirm)
        );
    }

    // ---- member-level actions (the reason a non-lead wants a deck) --------

    fn with_boards(is_lead: bool, policy: &str) -> OpContext {
        let mut c = ctx(is_lead);
        c.comms = Some(hud_protocol::CommsBoard {
            quick_actions: vec![hud_protocol::QuickAction {
                key: "downed".into(),
                label: "Downed".into(),
            }],
            ..Default::default()
        });
        c.member_clips = Some(hud_protocol::MemberClipBoard {
            policy: policy.into(),
            clips: vec![SoundboardClip {
                key: "clip/rally".into(),
                label: "Rally".into(),
            }],
        });
        c
    }

    #[test]
    fn quick_actions_and_member_clips_work_for_someone_who_is_not_the_lead() {
        // The whole point. Eight of the other actions are lead-only, so without these the deck is
        // a tool for op leads and nobody else.
        let c = with_boards(false, "all");
        assert_eq!(
            resolve(
                &Action::Quick {
                    key: "downed".into()
                },
                true,
                Some(&c),
                None
            ),
            Ok(ClientMessage::SendQuickAction {
                channel: None,
                action: "downed".into()
            })
        );
        assert_eq!(
            resolve(
                &Action::MemberClip {
                    key: "clip/rally".into()
                },
                true,
                Some(&c),
                None
            ),
            Ok(ClientMessage::TriggerMemberClip {
                key: "clip/rally".into()
            })
        );
    }

    #[test]
    fn a_quick_action_not_on_the_pushed_board_is_refused() {
        // A stale plugin config must not be able to post something this member was never offered.
        let c = with_boards(false, "all");
        assert_eq!(
            resolve(&Action::Quick { key: "nuke".into() }, true, Some(&c), None),
            Err(Refusal::UnknownAction)
        );
    }

    #[test]
    fn member_clips_respect_the_leads_policy() {
        let off = with_boards(false, "off");
        assert_eq!(
            resolve(
                &Action::MemberClip {
                    key: "clip/rally".into()
                },
                true,
                Some(&off),
                None
            ),
            Err(Refusal::ClipsOff)
        );
        // …and a board the server never sent at all is the same answer, not a panic.
        let mut none = ctx(false);
        none.member_clips = None;
        assert_eq!(
            resolve(
                &Action::MemberClip {
                    key: "clip/rally".into()
                },
                true,
                Some(&none),
                None
            ),
            Err(Refusal::ClipsOff)
        );
    }

    #[test]
    fn the_board_offers_member_options_to_a_non_lead() {
        let c = with_boards(false, "all");
        let s = key_state(true, Some(&c), None);
        assert_eq!(
            s.quick_actions,
            vec![("downed".to_string(), "Downed".to_string())]
        );
        assert_eq!(
            s.member_clips,
            vec![("clip/rally".to_string(), "Rally".to_string())]
        );
        // Clips off ⇒ nothing offered, so the picker cannot list something that would be refused.
        let s_off = key_state(true, Some(&with_boards(false, "off")), None);
        assert!(s_off.member_clips.is_empty());
        assert!(
            !s_off.quick_actions.is_empty(),
            "quick actions are unaffected by clip policy"
        );
    }

    // ---- key_state -------------------------------------------------------

    #[test]
    fn no_op_yields_a_disabled_board_not_an_absent_one() {
        let s = key_state(false, None, None);
        assert!(!s.connected && !s.live_op && !s.is_lead);
        assert_eq!(s.objective, None);
        assert!(s.clips.is_empty());
        assert_eq!(s.api_version, LOCAL_API_VERSION);
    }

    #[test]
    fn key_state_carries_what_a_key_needs_to_grey_itself() {
        let mut c = ctx(true);
        c.name = "Ghost Ops".into();
        c.objectives = vec![Default::default(), Default::default()];
        c.current_stage = 1;
        c.soundboard = vec![SoundboardClip {
            key: "clip/rally".into(),
            ..Default::default()
        }];
        let mut m = music(true);
        m.paused = true;
        m.now_playing = Some(TrackMeta {
            key: "k".into(),
            title: Some("Hostile Approach".into()),
            artist: None,
            genre: None,
            duration_ms: None,
        });
        c.music = Some(m);

        let s = key_state(true, Some(&c), None);
        assert!(s.connected && s.live_op && s.is_lead && s.bed_active && s.bed_paused);
        assert_eq!(s.op_name.as_deref(), Some("Ghost Ops"));
        assert_eq!(s.objective, Some((1, 2)));
        assert_eq!(s.now_playing.as_deref(), Some("Hostile Approach"));
        assert_eq!(s.clips, vec!["clip/rally".to_string()]);
    }
}
