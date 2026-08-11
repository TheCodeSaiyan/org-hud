//! starplatform desktop HUD — a transparent, always-on-top tray overlay with two layers:
//!
//! * PUBLIC (always on): the org's live identity card, fed by the unauthenticated `/ws`.
//! * MEMBER (opt-in): after logging in, an interactive panel of internal live data, pulled from the
//!   server's bearer-token REST API.
//!
//! Runs from the system tray (no taskbar entry); a JSON config file (server URL, theme, screen
//! corner, scale, which components show, start-on-login) is the single source of truth, edited from
//! the in-panel Settings view (gear button / tray "Settings…" / hotkey). Every privileged action
//! stays in Rust commands so the bundled frontend needs no Tauri ACL, and this binary depends ONLY
//! on `hud-protocol` from the workspace.

// Hide the console window in release builds — this is a GUI overlay, not a terminal app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, PhysicalPosition, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

mod local_api;
mod rich_presence;
use rich_presence::RichPresence;
mod smtc;

const KEYRING_SERVICE: &str = "com.thecodesaiyan.starplatformhud";
const KEYRING_USER: &str = "desktop-token";
const WIN_W: f64 = 520.0;
const WIN_H: f64 = 680.0;

// ---- config ----

/// One HUD component's display state: whether it shows and its own zoom factor. Stored opaquely —
/// the frontend owns the meaning; Rust just persists and round-trips it.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Comp {
    show: bool,
    scale: f64,
}

impl Default for Comp {
    fn default() -> Self {
        Self {
            show: true,
            scale: 1.0,
        }
    }
}

/// Persisted HUD preferences — the single source of truth, written to the app config dir.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct HudConfig {
    server_url: String,
    start_on_login: bool,
    theme: String,
    /// Screen corner the overlay docks to: bottom-left|bottom-right|top-left|top-right.
    corner: String,
    /// Overall overlay zoom factor (0.8–1.4); per-component scale stacks on top.
    scale: f64,
    /// Per-component show/scale, keyed by the frontend's component name (card/op/…/soundboard). A map,
    /// not a fixed struct, so a new overlay widget round-trips through Rust without a struct change
    /// (the old fixed struct silently dropped unknown keys like nowplaying/music/soundboard/fieldguide).
    #[serde(default)]
    components: HashMap<String, Comp>,
    /// Hide the whole overlay unless Star Citizen is the focused window (Windows). Default on.
    #[serde(default = "default_true")]
    auto_hide: bool,
    /// Let physical media keys drive the op's music bed via the Windows System Media Transport
    /// Controls (C11). **Default OFF**, mirroring the `voice.enabled` precedent: it makes the
    /// machine advertise a media session for audio it is not producing, and it competes with other
    /// players for whichever session the OS considers current. Only ever active while the member
    /// leads a live op with an audible bed — see the `smtc` module doc.
    #[serde(default)]
    media_keys: bool,
    /// Expose the C10 local control API on loopback, so tools like a Stream Deck plugin can drive
    /// the live op. **Default OFF** — it opens a listening socket, and a feature nobody asked for
    /// should not start one. See the `local_api` module doc for why loopback alone is not enough.
    #[serde(default)]
    local_api: bool,
    /// Port for the above. Fixed by default so a plugin has something to default to.
    #[serde(default = "default_local_port")]
    local_api_port: u16,
    /// Per-widget screen positions (widget id → [x, y] in logical px), set by dragging.
    #[serde(default)]
    positions: HashMap<String, [f64; 2]>,
    /// Per-widget explicit sizes (widget id → [w, h] as a border box, in the widget's own CSS px —
    /// i.e. before its per-component `zoom`), set by dragging an edge or corner. `h <= 0` means AUTO:
    /// the frontend only writes a height when a vertical edge is dragged, so a widget the user has
    /// merely widened still sizes itself to its content. Open map for the same reason `components`
    /// is one (PR #29) — a new widget id round-trips with no Rust change — and `#[serde(default)]`
    /// so a config written before this field existed still loads instead of resetting the user.
    #[serde(default)]
    sizes: HashMap<String, [f64; 2]>,
    /// Opacity of the **interact**-mode scrim, in PERCENT (`TINT_MIN`..=`TINT_MAX`). Rust only
    /// persists the number: the frontend feeds it to the `--arrange-scrim` custom property and CSS
    /// mixes it against `--arrange-wash` (which is derived from `--accent`), so all four themes and
    /// a runtime org accent keep working with no colour table on this side.
    ///
    /// `#[serde(default = "default_tint")]` and NOT a bare `#[serde(default)]`: a field-level
    /// `default` overrides the container one and would hand back `f64::default()` — 0.0, i.e. every
    /// existing user silently upgraded to *no* tint at all.
    #[serde(default = "default_tint")]
    tint_opacity: f64,
    /// The three rebindable hotkeys. `#[serde(default = "default_keys")]` and NOT a bare
    /// `#[serde(default)]`: a field-level bare default builds `KeyBindings` from *its* container
    /// default, which is what we want here only by luck — spelling the fn out keeps it correct even
    /// if `Binding` ever gains a `#[derive(Default)]`-shaped zero that means "unbound".
    ///
    /// Whatever lands here is run through `KeyBindings::sanitized` on load AND on save, so a
    /// hand-edited or truncated config can never install a binding that fires while typing.
    #[serde(default = "default_keys")]
    keys: KeyBindings,
}

fn default_true() -> bool {
    true
}

// ---- hotkey bindings ----

// Windows virtual-key codes, spelled out here rather than pulled from `windows-sys`: the whole
// binding model (config shape, validation, `classify_hotkey`) is cross-platform on purpose so
// ubuntu CI exercises it, and `windows-sys` is a `cfg(windows)`-only dependency.
const VK_X: u32 = 0x58;
const VK_C: u32 = 0x43;
const VK_Z: u32 = 0x5A;

/// Virtual keys that are modifiers. None of them may be a binding's MAIN key — a binding whose key
/// *is* Ctrl fires the instant the user reaches for any Ctrl chord.
const MODIFIER_VKS: [u32; 9] = [
    0x10, // VK_SHIFT
    0x11, // VK_CONTROL
    0x12, // VK_MENU (Alt)
    0xA0, 0xA1, // VK_LSHIFT / VK_RSHIFT
    0xA2, 0xA3, // VK_LCONTROL / VK_RCONTROL
    0xA4, 0xA5, // VK_LMENU / VK_RMENU
];

/// One rebindable hotkey: the main key's Windows virtual-key code plus the modifiers that must be
/// held. `alt` means the **LEFT** Alt specifically — see `binding_matches` for the AltGr measurement
/// that forces it.
///
/// `Default` is derived (vk `0`, no modifiers) purely so a truncated `{"vk":88}` object still
/// deserialises instead of failing the whole config. That value is *invalid*, `check_binding`
/// rejects it, and `KeyBindings::sanitized` swaps the set back to the defaults — a vk of 0 can never
/// match a real key anyway (`binding_matches` guards it), so the failure mode is inert, not wild.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
struct Binding {
    vk: u32,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

/// The three bindings, one per hotkey action. A fixed struct rather than the open map `components`
/// uses: the action set is closed by `HudMode` + the visibility axis, so an unknown key here would be
/// a typo, not a forward-compatible extension.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(default)]
struct KeyBindings {
    interact: Binding,
    settings: Binding,
    visibility: Binding,
}

/// The shipped defaults: a one-handed bottom-left cluster on ordinary letter keys.
///
/// Chosen over the old Pause family because Pause/Break is a scan-code special case (Ctrl+Pause is a
/// *different* key, `VK_CANCEL`), is missing or Fn-shifted on many compact keyboards, and is a single
/// physical key that some remappers eat. Ctrl+Alt+letter has none of that, and Star Citizen binds
/// almost nothing on Ctrl+Alt.
const DEFAULT_KEYS: KeyBindings = KeyBindings {
    // Ctrl+Alt+X — widgets forward and interactable, over the tinted game.
    interact: Binding {
        vk: VK_X,
        ctrl: true,
        alt: true,
        shift: false,
    },
    // Ctrl+Alt+C — the settings surface, with no backdrop at all.
    settings: Binding {
        vk: VK_C,
        ctrl: true,
        alt: true,
        shift: false,
    },
    // Ctrl+Alt+Z — show / hide the whole overlay.
    visibility: Binding {
        vk: VK_Z,
        ctrl: true,
        alt: true,
        shift: false,
    },
};

impl Default for KeyBindings {
    fn default() -> Self {
        DEFAULT_KEYS
    }
}

fn default_keys() -> KeyBindings {
    DEFAULT_KEYS
}

/// Why a single binding is unusable, or `Ok` if it is fine. The strings are user-facing: the settings
/// UI mirrors these same three rules, and a rejected capture shows the reason.
fn check_binding(b: &Binding) -> Result<(), &'static str> {
    if b.vk == 0 || b.vk > 0xFF {
        return Err("no key");
    }
    if MODIFIER_VKS.contains(&b.vk) {
        return Err("a modifier key cannot be the main key");
    }
    // A bare key — or a Shift-only combo, which is the same thing while typing — would fire during
    // normal gameplay and chat. At least one of Ctrl / Alt is mandatory; Shift may ride on top.
    if !(b.ctrl || b.alt) {
        return Err("needs Ctrl or Alt");
    }
    Ok(())
}

/// Validate the whole set: each binding legal, and no two actions sharing one combo (which would make
/// one of them permanently unreachable — `classify_hotkey` resolves ties in a fixed order).
fn check_bindings(k: &KeyBindings) -> Result<(), &'static str> {
    let all = [k.interact, k.settings, k.visibility];
    for b in &all {
        check_binding(b)?;
    }
    for (i, a) in all.iter().enumerate() {
        if all[i + 1..].contains(a) {
            return Err("two actions share one combo");
        }
    }
    Ok(())
}

impl KeyBindings {
    /// The set to actually install. An invalid set is replaced **wholesale** by the defaults rather
    /// than repaired binding-by-binding: a per-binding repair can reintroduce a collision with the
    /// two that were kept, and "one bad edit resets the keys" is a rule a user can predict. Only a
    /// hand-edited or corrupt `config.json` ever reaches this — the settings UI validates on capture.
    fn sanitized(self) -> Self {
        if check_bindings(&self).is_ok() {
            self
        } else {
            DEFAULT_KEYS
        }
    }
}

/// Live physical modifier state, sampled at the instant a key fires. Split L/R for Alt because that
/// split is the ONLY thing separating a real Ctrl+Alt from AltGr — see `binding_matches`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mods {
    /// Either Ctrl. Under AltGr this is DOWN (a phantom LEFT Ctrl) — hence the `right_alt` veto.
    ctrl: bool,
    /// `VK_LMENU`.
    left_alt: bool,
    /// `VK_RMENU` — AltGr on every non-US layout.
    right_alt: bool,
    /// Either Shift.
    shift: bool,
}

/// Pack one binding into 11 bits (8 for the vk — every Windows VK fits in a byte — plus one bit per
/// modifier) so all three fit in a single `u64` the keyboard hook can read with one relaxed atomic
/// load. A `Mutex<KeyBindings>` would work too, but it puts a lock on the path Windows walks for
/// EVERY keystroke system-wide: a writer preempted while holding it stalls the whole machine's input
/// and, past `LowLevelHooksTimeout`, gets the hook silently uninstalled. That is precisely the class
/// of "the hotkeys sometimes stop working" this change exists to remove.
const fn pack_binding(b: Binding) -> u64 {
    (b.vk as u64 & 0xFF) | ((b.ctrl as u64) << 8) | ((b.alt as u64) << 9) | ((b.shift as u64) << 10)
}

// `--all-targets` strips cfg(test) from a bin unit, so the tests below grant NO liveness:
// off Windows the only non-test callers are gone and `-D warnings` fails on ubuntu CI, which
// a Windows dev box cannot reproduce (here the code is genuinely live). Scope the allow to the
// platform where it is truly dead, so the lint stays strict where deadness WOULD be a defect.
#[cfg_attr(not(windows), allow(dead_code))]
fn unpack_binding(v: u64) -> Binding {
    Binding {
        vk: (v & 0xFF) as u32,
        ctrl: v & (1 << 8) != 0,
        alt: v & (1 << 9) != 0,
        shift: v & (1 << 10) != 0,
    }
}

const fn pack_keys(k: &KeyBindings) -> u64 {
    pack_binding(k.interact) | (pack_binding(k.settings) << 11) | (pack_binding(k.visibility) << 22)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn unpack_keys(v: u64) -> KeyBindings {
    KeyBindings {
        interact: unpack_binding(v & 0x7FF),
        settings: unpack_binding((v >> 11) & 0x7FF),
        visibility: unpack_binding((v >> 22) & 0x7FF),
    }
}

/// The bindings the keyboard hook reads. Published by `install_bindings` on boot and on every config
/// save; see `pack_binding` for why it is an atomic word and not a lock.
static HOTKEY_BINDINGS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(pack_keys(&DEFAULT_KEYS));

/// Publish a (already sanitized) binding set to the keyboard hook.
fn install_bindings(k: &KeyBindings) {
    HOTKEY_BINDINGS.store(pack_keys(k), std::sync::atomic::Ordering::Relaxed);
}

/// Default interact-scrim opacity, in percent. The slider's RANGE lives in the frontend
/// (`const TINT_MIN … TINT_MAX … TINT_STEP` in `ui/index.html`), which is also what renders the
/// control — duplicating it here would just create a second copy to drift. The guard test
/// `the_interact_tint_is_configurable_and_defaults_to_30` parses the JS declaration and checks this
/// default, the CSS `:root` value and the slider range all agree.
const TINT_DEFAULT: f64 = 30.0;

fn default_local_port() -> u16 {
    local_api::DEFAULT_PORT
}

fn default_tint() -> f64 {
    TINT_DEFAULT
}

impl Default for HudConfig {
    fn default() -> Self {
        Self {
            server_url: std::env::var("HUD_SERVER")
                .unwrap_or_else(|_| "http://localhost:8080".to_string()),
            start_on_login: false,
            theme: "stanton".to_string(),
            corner: "bottom-left".to_string(),
            scale: 1.0,
            components: HashMap::new(),
            auto_hide: true,
            // Default OFF: it advertises a media session for audio this machine is not
            // producing, and competes for whichever session the OS deems current.
            media_keys: false,
            local_api: false,
            local_api_port: local_api::DEFAULT_PORT,
            positions: HashMap::new(),
            sizes: HashMap::new(),
            tint_opacity: TINT_DEFAULT,
            keys: DEFAULT_KEYS,
        }
    }
}

/// The overlay's three interaction modes. Exactly one is live at a time, and the hotkeys, the tray,
/// the frontend body classes and the Keys tab all name the same three:
///
/// | Mode | Trigger | Clicks | Backdrop |
/// |------|---------|--------|----------|
/// | `Passive`  | (default; Esc / Done / tray) | pass straight through to the game | none |
/// | `Interact` | **Ctrl+Alt+X** (rebindable)  | captured by the overlay           | theme tint + scrim (opacity is a user setting) |
/// | `Settings` | **Ctrl+Alt+C** (rebindable)  | captured by the overlay           | **none** — the game stays fully visible |
///
/// Only the CLICK behaviour separates `Settings` from `Passive`, and only the SURFACE separates it
/// from `Interact`: settings paints no backdrop of its own, so a user configuring the HUD can still
/// see what is happening in the game behind it.
///
/// Show/hide of the whole overlay is a SEPARATE axis (**Ctrl+Alt+Z** → `hidden_by_user`), not a
/// fourth mode: it can apply to any mode, and hiding always drops back to `Passive` so the overlay
/// is never invisible-but-capturing (see `plan_visibility_toggle`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HudMode {
    #[default]
    Passive,
    Interact,
    Settings,
}

impl HudMode {
    /// Whether the overlay swallows the mouse in this mode. This is the ONE predicate every consumer
    /// asks — click-through, the focus watcher's "always shown" reason, the hover watcher's whole-
    /// window capture and the hide-drops-to-passive invariant — so adding a fourth mode later only
    /// has to answer it once. `Passive` is the only pass-through mode, by definition.
    fn captures_clicks(self) -> bool {
        !matches!(self, HudMode::Passive)
    }

    /// Wire name shared with the frontend (`hud:mode` payload / the `set_mode` command argument).
    fn as_str(self) -> &'static str {
        match self {
            HudMode::Passive => "passive",
            HudMode::Interact => "interact",
            HudMode::Settings => "settings",
        }
    }

    /// Parse the wire name. Anything unrecognised falls back to `Passive` — the fail-safe direction:
    /// a typo can only ever *release* the mouse, never strand the user in a mode that captures it.
    fn parse(s: &str) -> Self {
        match s {
            "interact" => HudMode::Interact,
            "settings" => HudMode::Settings,
            _ => HudMode::Passive,
        }
    }
}

/// One press of a mode hotkey (or its tray item): pressing the mode you are already in returns to
/// `Passive`, pressing any other switches straight to it. Pure so the three-mode state machine is
/// testable — live, these transitions are only observable in a game session.
///
/// The cross-mode edges matter as much as the toggles: Settings → the **interact** key → Interact
/// drops the opaque backdrop without leaving interaction, and Interact → the **settings** key →
/// Settings escalates without a round-trip through `Passive`.
fn next_mode(current: HudMode, pressed: HudMode) -> HudMode {
    if current == pressed {
        HudMode::Passive
    } else {
        pressed
    }
}

pub(crate) struct AppState {
    /// Which of the three interaction modes is live. Replaced the old `interactive: bool` when
    /// settings became a mode of its own — a bool cannot express "captures clicks and shows the
    /// settings surface over an UNtinted game" as distinct from "captures clicks over a tinted one".
    mode: Mutex<HudMode>,
    /// True while the user has explicitly hidden the overlay with the show/hide hotkey (`keys.
    /// visibility`, default Ctrl+Alt+Z). The focus watcher MUST honour this — it recomputes "should the overlay be on screen?"
    /// every 700 ms and would otherwise re-show a manually hidden overlay within one tick. Entering
    /// any click-capturing mode clears it (an invisible overlay cannot be arranged or configured).
    hidden_by_user: Mutex<bool>,
    config: Mutex<HudConfig>,
    /// Outbound sender for the current member socket (None when disconnected). Commands push
    /// `ClientMessage`s here; the socket writer task forwards them.
    pub(crate) member_tx:
        Mutex<Option<tokio::sync::mpsc::UnboundedSender<hud_protocol::ClientMessage>>>,
    /// Window-relative physical-pixel boxes `[x, y, w, h]` of the controls that should be clickable in
    /// display mode (reported by the frontend). The hover watcher disables click-through only while the
    /// cursor is over one of these — so HUD buttons work in-game without entering full arrange mode.
    hit_rects: Mutex<Vec<[f64; 4]>>,
    /// Mirrors the server-pushed, privacy-gated `DiscordPresence` block onto the local Discord client
    /// via IPC. Best-effort — see `rich_presence` module doc.
    rich_presence: RichPresence,
    /// Windows media-key control of the op bed (C11). Default-off; see `smtc` module doc.
    smtc: smtc::Smtc,
    /// C10 local control API: fan the derived key board out to connected local tools.
    pub(crate) local_state_tx: tokio::sync::broadcast::Sender<String>,
    /// The last pushed op context, so a tool connecting mid-op gets a board immediately rather
    /// than waiting up to a full tick with dead keys.
    pub(crate) local_last_ctx: Mutex<Option<hud_protocol::OpContext>>,
    /// A live op the member could check into, when they are not on one. Cached because the board is
    /// pushed far more often than this can change.
    pub(crate) local_checkin: Mutex<Option<String>>,
    pub(crate) local_checkin_at: Mutex<Option<std::time::Instant>>,
}

fn config_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("config.json"))
}

fn load_config(app: &tauri::AppHandle) -> HudConfig {
    let mut cfg: HudConfig = config_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Never trust a persisted/tampered server URL — the bearer token rides on it.
    if !valid_server(&cfg.server_url) {
        cfg.server_url = HudConfig::default().server_url;
    }
    // …nor a persisted binding set. A hand-edited `config.json` could otherwise install a bare-key
    // hotkey that fires on every press during gameplay, with no way to see why.
    cfg.keys = cfg.keys.sanitized();
    cfg
}

fn save_config(app: &tauri::AppHandle, cfg: &HudConfig) -> Result<(), String> {
    let path = config_path(app).ok_or("no config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn server_url(app: &tauri::AppHandle) -> String {
    app.state::<AppState>()
        .config
        .lock()
        .unwrap()
        .server_url
        .clone()
}

/// A server URL is acceptable only if it's an http(s) URL with a host. The bearer token is ONLY ever
/// sent to this origin, so a malformed / non-http config value is rejected (falls back to default).
fn valid_server(s: &str) -> bool {
    Url::parse(s)
        .map(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
        .unwrap_or(false)
}

/// Derive the authenticated member WebSocket URL from the configured HTTP base:
/// `http(s)://host[:port]` → `ws(s)://host[:port]/ws/member`.
fn member_ws_url(server: &str) -> Result<String, String> {
    let mut u = Url::parse(server).map_err(|e| e.to_string())?;
    let ws_scheme = match u.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => return Err(format!("unsupported scheme: {other}")),
    };
    u.set_scheme(ws_scheme)
        .map_err(|_| "set_scheme failed".to_string())?;
    u.set_path("/ws/member");
    u.set_query(None);
    Ok(u.to_string().trim_end_matches('/').to_string())
}

/// Whether `u` is the same origin (scheme + host + port) as `base`. Used to gate token capture and
/// authenticated fetches so a credential can never be handed to a foreign origin.
fn same_origin(u: &Url, base: &Url) -> bool {
    u.origin() == base.origin()
}

// ---- authenticated member WebSocket ----

/// Long-lived task: keep an authenticated `/ws/member` connection up whenever we have a token and a
/// valid server. Re-emits `OrgContext` as `hud:context`; emits `hud:context-stale` while down.
fn spawn_member_socket(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut backoff = Duration::from_millis(1000);
        loop {
            let token = load_token();
            let url = member_ws_url(&server_url(&handle)).ok();
            let (Some(token), Some(url)) = (token, url) else {
                let _ = handle.emit("hud:context-stale", ());
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let _ = connect_member(&handle, &url, &token).await;
            // Connection ended (close, error, or token/url change) → mark stale + back off.
            *handle.state::<AppState>().member_tx.lock().unwrap() = None;
            let _ = handle.emit("hud:context-stale", ());
            // Local tools learn the socket is down the same moment the webview does. Without this
            // their board keeps saying `connected: true` until the NEXT context message, which by
            // definition cannot arrive while the socket is down.
            local_api::push_board(&handle).await;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(15));
            // A successful, long-lived connection resets the backoff for next time.
            if load_token().is_some() {
                backoff = Duration::from_millis(1000);
            }
        }
    });
}

/// One member-socket session: connect with the bearer header, register the outbound sender, then
/// pump inbound→Tauri events and outbound channel→socket until either side closes or the
/// token/server changes underneath us.
async fn connect_member(app: &tauri::AppHandle, url: &str, token: &str) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
    use tokio_tungstenite::tungstenite::Message as Ws;

    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert(
        AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "bad header".to_string())?,
    );
    let (stream, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| e.to_string())?;
    let (mut write, mut read) = stream.split();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<hud_protocol::ClientMessage>();
    *app.state::<AppState>().member_tx.lock().unwrap() = Some(tx);
    // Mirror of the socket-down push: without this, `connected` flips true with no fan-out, so a
    // local tool shows "HUD offline" on every key for up to a full tick while everything works.
    local_api::push_board_now(app);

    let connected_server = server_url(app);
    let connected_token = token.to_string();
    let mut guard = tokio::time::interval(Duration::from_secs(2));

    loop {
        tokio::select! {
            // Outbound: a command queued a ClientMessage.
            Some(msg) = rx.recv() => {
                let Ok(text) = serde_json::to_string(&msg) else { continue };
                if write.send(Ws::Text(text)).await.is_err() { break; }
            }
            // Inbound: server pushed a message.
            incoming = read.next() => match incoming {
                Some(Ok(Ws::Text(text))) => {
                    if let Ok(hud_protocol::ServerMessage::OrgContext(ctx)) =
                        serde_json::from_str::<hud_protocol::ServerMessage>(&text)
                    {
                        if let Some(state) = app.try_state::<AppState>() {
                            state.rich_presence.apply(ctx.discord_presence.as_ref());
                            apply_media_session(app, &state, ctx.op.as_ref());
                            local_api::on_context(app, ctx.op.as_ref()).await;
                        }
                        let _ = app.emit("hud:context", ctx);
                    }
                }
                Some(Ok(Ws::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            },
            // Stop if the user logged out or switched servers.
            _ = guard.tick() => {
                if load_token().as_deref() != Some(connected_token.as_str())
                    || server_url(app) != connected_server
                {
                    break;
                }
            }
        }
    }
    Ok(())
}

// ---- OS secret store ----

fn store_token(token: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .and_then(|e| e.set_password(token))
        .map_err(|e| e.to_string())
}

fn load_token() -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .ok()?
        .get_password()
        .ok()
}

fn clear_token() -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Pull a `token=…` value out of a completion URL — preferring the fragment (never logged by the
/// server), falling back to the query.
fn token_from_url(u: &Url) -> Option<String> {
    if let Some(frag) = u.fragment() {
        for part in frag.split('&') {
            if let Some(v) = part.strip_prefix("token=") {
                return Some(v.to_string());
            }
        }
    }
    u.query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

// ---- interactivity / window placement ----

/// Current interaction mode, or `Passive` if state isn't up yet.
fn current_mode(app: &tauri::AppHandle) -> HudMode {
    app.try_state::<AppState>()
        .map(|s| *s.mode.lock().unwrap())
        .unwrap_or_default()
}

/// Put one of the three modes into effect: record it, sync OS click-through, and tell the frontend
/// (which owns the visuals — the tint/scrim for `Interact`, and for `Settings` the settings surface
/// over NO backdrop at all).
fn apply_mode(app: &tauri::AppHandle, mode: HudMode) {
    let capture = mode.captures_clicks();
    let plan = focus_plan(mode);
    if let Some(state) = app.try_state::<AppState>() {
        *state.mode.lock().unwrap() = mode;
        // You cannot arrange or configure an invisible overlay: entering any click-capturing mode
        // always clears a manual hide, so the window, the focus watcher and the user all agree it
        // belongs on screen.
        if capture {
            *state.hidden_by_user.lock().unwrap() = false;
        }
    }
    if let Some(win) = app.get_webview_window("main") {
        // Focusability FIRST — the order is load-bearing. `set_ignore_cursor_events` and
        // `show_overlay` both end in a `ShowWindow`, and tao's `apply_diff` uses a plain `SW_SHOW`,
        // which ACTIVATES; setting the flag afterwards would let that activation through.
        set_focusable_for_mode(&win, plan.focusable);
        if plan.grab {
            // Remember who we are taking the foreground FROM, so leaving the mode can give it back.
            // Captured before the grab, and only when it is not already us.
            remember_foreground_owner(&win);
        }
        let _ = win.set_ignore_cursor_events(!capture);
        if plan.grab {
            // NEVER `win.show()` here — that routes through tao and is the other half of the
            // VISIBLE-flag desync documented on `show_overlay`/`hide_overlay`. A click-capturing
            // mode is the one place a focus grab is legitimate: the user asked for the window, and
            // the Esc escape hatch only works while it holds focus.
            show_overlay(&win);
            let _ = win.set_focus();
        }
        if plan.release {
            release_foreground(&win);
        }
    }
    let _ = app.emit("hud:mode", mode.as_str());
}

/// What a mode change may do to the OS foreground. Three separate answers because they are three
/// separate Win32 acts, and the bug this encodes came from them disagreeing.
#[derive(Debug, PartialEq, Eq)]
struct FocusPlan {
    /// May the overlay be activated at all (`WS_EX_NOACTIVATE` cleared)?
    focusable: bool,
    /// Take the foreground now.
    grab: bool,
    /// Hand the foreground back to whoever had it.
    release: bool,
}

/// Whether the overlay may hold the OS foreground in `mode`.
///
/// ## The bug this exists to prevent (measured 2026-08-02, on the live overlay — not reasoned)
///
/// **While this process owns the foreground window, Windows stops dispatching keystrokes to this
/// process's own `WH_KEYBOARD_LL` hook.** Every hotkey therefore dies for exactly as long as the
/// overlay holds focus, and comes back the moment focus goes elsewhere — which is precisely the
/// report: "after entering interactive mode and interacting, [exiting] slips back to the
/// passthrough and … the key combos are not triggering unless i first click elsewhere that's
/// passed through". That click lands on the window *underneath* the click-through overlay,
/// activates it, and the hotkeys start working again.
///
/// Reproduced in both directions: with the overlay foreground the hook proc was never invoked over
/// 30 consecutive injected keystrokes; with the foreground elsewhere the same injections reached it
/// 8 of 8. A second low-level hook in another process (which is NOT the foreground process) was
/// dispatched to normally and its `CallNextHookEx` ran our proc — so our hook, its thread, its
/// message pump and its registration were all healthy throughout. Re-installing the hook, and even
/// moving it to a fresh thread, did NOT recover it; only losing the foreground did. (`unconfirmed:`
/// the exact kernel rule. The fix does not rest on it — it rests on the measured behaviour.)
///
/// Two things used to leave the overlay holding focus indefinitely:
///   * `apply_mode` grabbed focus entering a capturing mode and never gave it back on exit; and
///   * tao's `apply_diff` fires `ShowWindow(SW_SHOW)` — which activates — on EVERY click-through
///     transition, so in passive mode merely moving the cursor across a HUD control grabbed it.
///
/// Both are closed here: passive is `WS_EX_NOACTIVATE` (so the `SW_SHOW` cannot activate) and every
/// exit from a capturing mode hands the foreground back.
///
/// Pure so the rule is testable without a window — the behaviour it guards is otherwise visible
/// only in a live game session.
fn focus_plan(mode: HudMode) -> FocusPlan {
    let capture = mode.captures_clicks();
    FocusPlan {
        // A capturing mode is the ONE place a focus grab is legitimate: the user asked for the
        // window, and the Esc escape hatch only works while it holds focus.
        focusable: capture,
        grab: capture,
        release: !capture,
    }
}

/// Let the overlay take the foreground only in a click-capturing mode. `WS_EX_NOACTIVATE` (what
/// tao's FOCUSABLE flag controls) is the only thing that can stop the overlay activating itself,
/// because the activation does not come from our code — see `focus_plan`.
///
/// Goes through the same window-flag machinery as everything else, so it must be called only on a
/// real mode change, never on a hover tick.
fn set_focusable_for_mode(win: &tauri::WebviewWindow, focusable: bool) {
    let _ = win.set_focusable(focusable);
}

/// The window that held the foreground when we last took it, so leaving a capturing mode can hand
/// it straight back. `0` = nothing to restore.
#[cfg_attr(not(windows), allow(dead_code))]
static PREV_FOREGROUND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Record the current foreground window, unless it is already ours (re-entering a capturing mode
/// from another one must not overwrite the game with the overlay itself).
#[cfg(windows)]
fn remember_foreground_owner(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let ours = win.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    let fg = unsafe { GetForegroundWindow() } as isize;
    if fg != 0 && fg != ours {
        PREV_FOREGROUND.store(fg, std::sync::atomic::Ordering::Relaxed);
    }
}
#[cfg(not(windows))]
fn remember_foreground_owner(_win: &tauri::WebviewWindow) {}

/// Give the foreground back to whoever had it before we took it — best effort, and only while we
/// still hold it (if the user has already clicked into something else, leave their choice alone).
///
/// `SetForegroundWindow` normally refuses a caller that does not own the foreground; here we DO own
/// it, which is precisely the documented case in which the call is permitted. If there is nobody to
/// restore, the window is simply made unfocusable and left to lose the foreground on the next click
/// — which now passes through, because passive mode is click-through.
#[cfg(windows)]
fn release_foreground(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};
    let Ok(ours) = win.hwnd() else { return };
    let ours = ours.0 as isize;
    if unsafe { GetForegroundWindow() } as isize != ours {
        return; // someone else already has it — don't yank it around.
    }
    let prev = PREV_FOREGROUND.swap(0, std::sync::atomic::Ordering::Relaxed);
    if prev != 0 && prev != ours {
        unsafe { SetForegroundWindow(prev as _) };
    }
}
#[cfg(not(windows))]
fn release_foreground(_win: &tauri::WebviewWindow) {}

/// One press of a mode hotkey / tray item. See `next_mode` for the transition table.
fn toggle_mode(app: &tauri::AppHandle, pressed: HudMode) {
    apply_mode(app, next_mode(current_mode(app), pressed));
}

/// `keys.interact` (default **Ctrl+Alt+X**) — widgets forward and interactable, over the tinted game.
fn toggle_interact(app: &tauri::AppHandle) {
    toggle_mode(app, HudMode::Interact);
}

/// `keys.settings` (default **Ctrl+Alt+C**) — the settings surface, with no backdrop over the game.
fn toggle_settings(app: &tauri::AppHandle) {
    toggle_mode(app, HudMode::Settings);
}

/// `keys.visibility` (default **Ctrl+Alt+Z**) show/hide of the whole overlay. A separate axis from
/// the three modes: this
/// is a "get it off my screen" toggle that survives the focus watcher, which would otherwise re-show
/// the overlay on its next 700 ms tick. Applied immediately so the key feels instant, and shown
/// passively so un-hiding never steals focus from the game.
///
/// **Hiding also drops to `Passive`.** A click-capturing mode with no window on screen is a dead
/// end: the window is never focused on un-hide, so the Esc keydown listener cannot fire, and the
/// next mode hotkey reads as "toggle that mode OFF" and looks like a no-op. The project rule is that
/// Esc / Done / tray must never depend on the hotkey, so the two states are kept mutually exclusive
/// — entering `Interact`/`Settings` clears a manual hide (`apply_mode`), and a manual hide returns
/// to `Passive` (here).
///
/// Reachable from the Windows keyboard hook's poller AND the tray's "Show / hide overlay" item, so
/// it is live on every platform (the tray is the only route on macOS/Linux, where the keyboard hook
/// is a no-op stub).
fn toggle_visibility(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let hide = {
        let mut g = state.hidden_by_user.lock().unwrap();
        *g = !*g;
        *g
    };
    let mode = *state.mode.lock().unwrap();
    let plan = plan_visibility_toggle(hide, mode);
    if plan.drop_to_passive {
        // Drop to passive BEFORE hiding, never after: `set_ignore_cursor_events` runs tao's
        // `apply_diff`, whose first act is `ShowWindow(SW_SHOW)` whenever the new flags contain
        // VISIBLE (which, post-fix, they always do) — doing it after the hide would immediately
        // undo it.
        apply_mode(app, HudMode::Passive);
    }
    if let Some(win) = app.get_webview_window("main") {
        if plan.hide {
            hide_overlay(&win);
        } else {
            show_overlay(&win);
            // Windows drops the WS_EX_TRANSPARENT click-through style whenever a window is
            // re-shown, so re-assert the pass-through state for the current mode — same reason the
            // focus watcher does it after its own show.
            let _ = win.set_ignore_cursor_events(!mode.captures_clicks());
            if plan.restore_focus {
                let _ = win.set_focus();
            }
        }
    }
}

/// What one press of the visibility toggle should do, given the state it lands on. Pure so the
/// invariant it encodes — **a manual hide and every click-capturing mode are mutually exclusive** —
/// is testable without a window, for all three modes.
#[derive(Debug, PartialEq, Eq)]
struct VisibilityPlan {
    /// Take the window off screen (vs. put it back).
    hide: bool,
    /// Return to `Passive` as part of hiding. `Interact`/`Settings` with no window on screen is a
    /// dead end: the window is never focused on un-hide, so the Esc keydown listener cannot fire,
    /// and the next mode hotkey reads as "toggle that mode OFF" and looks like a silent no-op.
    drop_to_passive: bool,
    /// Give the window focus back on un-hide. Only meaningful if a capturing mode somehow survived —
    /// belt and braces for the same Esc-must-work rule.
    restore_focus: bool,
}

fn plan_visibility_toggle(hide: bool, mode: HudMode) -> VisibilityPlan {
    VisibilityPlan {
        hide,
        drop_to_passive: hide && mode.captures_clicks(),
        restore_focus: !hide && mode.captures_clicks(),
    }
}

/// Size the overlay to cover the whole primary monitor — widgets are then free-positioned inside it
/// (and dragged/arranged by the user), instead of the window docking to a corner.
fn make_fullscreen(win: &tauri::WebviewWindow) {
    if let Ok(Some(mon)) = win.current_monitor() {
        let _ = win.set_position(PhysicalPosition::new(0, 0));
        let _ = win.set_size(*mon.size());
    }
}

// Pure title classifiers (cross-platform so they're unit-testable). The overlay's window title is
// "StarPlatform HUD" (set at build time); we must never treat our own window appearing in the foreground as
// "the game is gone" — that's what caused the auto-hide show/hide oscillation in-game.
//
// Their only non-test callers are `#[cfg(windows)]`, and this is a bin crate — `--all-targets`
// strips cfg(test) from the bin unit, so the tests below grant no liveness. On non-Windows that
// leaves them genuinely unreachable. Scope the allow to that platform so the lint stays live on
// Windows, where real deadness would still be a defect.
#[cfg_attr(not(windows), allow(dead_code))]
fn title_is_star_citizen(title: &str) -> bool {
    title.contains("Star Citizen")
}
#[cfg_attr(not(windows), allow(dead_code))]
fn title_is_overlay(title: &str) -> bool {
    title.contains("StarPlatform HUD")
}

/// The foreground window's title, if any. Windows-only; elsewhere there's no cheap foreground check.
#[cfg(windows)]
fn foreground_window_title() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// True if Star Citizen is the foreground window (so the overlay should be visible). On non-Windows
/// there's no cheap foreground check, so the overlay always shows.
#[cfg(windows)]
fn foreground_is_star_citizen() -> bool {
    foreground_window_title().is_some_and(|t| title_is_star_citizen(&t))
}
#[cfg(not(windows))]
fn foreground_is_star_citizen() -> bool {
    true
}

/// True when the overlay's OWN window holds the foreground. Used to break the auto-hide oscillation:
/// showing the overlay can momentarily put it in front, and we must not read that as "game gone".
#[cfg(windows)]
fn foreground_is_overlay() -> bool {
    foreground_window_title().is_some_and(|t| title_is_overlay(&t))
}
#[cfg(not(windows))]
fn foreground_is_overlay() -> bool {
    false
}

/// Show the overlay WITHOUT activating it, so it never steals foreground focus from the game — an
/// active `show()` would both pause some titles and make the focus watcher oscillate (show → overlay
/// becomes foreground → "game gone" → hide → repeat). Falls back to a normal show off-Windows.
///
/// See `hide_overlay` for why visibility must NEVER be routed through tao once this exists.
fn show_overlay(win: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};
        if let Ok(hwnd) = win.hwnd() {
            unsafe { ShowWindow(hwnd.0 as _, SW_SHOWNOACTIVATE) };
            return;
        }
    }
    let _ = win.show();
}

/// Hide the overlay with the same raw `ShowWindow` call `show_overlay` uses, deliberately bypassing
/// tao — **the two must be symmetric or the overlay can be hidden permanently.**
///
/// `show_overlay` pokes `ShowWindow(SW_SHOWNOACTIVATE)` at the HWND directly and never tells tao,
/// so tao's cached `WindowFlags::VISIBLE` does not track it. `win.hide()`, by contrast, goes
/// *through* tao and CLEARS that cached bit. Mixing the two desyncs the cache, and the cache is not
/// inert: every `set_ignore_cursor_events` call runs `WindowState::set_window_flags` →
/// `WindowFlags::apply_diff`, which ends with an unconditional
/// `if !new.contains(VISIBLE) { ShowWindow(SW_HIDE) }` — gated on the NEW state, not on the diff.
/// So after hide→show via the hotkey, the very next click-through *transition* (hovering any
/// reported control) re-hid the window for good: the hover watcher then `continue`s forever on
/// `!win.is_visible()`, and no show path runs again. (Verified against tao 0.35.3,
/// `platform_impl/windows/window_state.rs:325,420` and `:566`.)
///
/// The fix is to never let tao own visibility: with the cached bit left permanently true,
/// `apply_diff` can only ever *show*, and `IsWindowVisible` (what `win.is_visible()` reads —
/// `util.rs:203`) stays the single source of truth that both watchers already poll.
fn hide_overlay(win: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
        if let Ok(hwnd) = win.hwnd() {
            unsafe { ShowWindow(hwnd.0 as _, SW_HIDE) };
            return;
        }
    }
    let _ = win.hide();
}

/// The global cursor position in physical screen pixels. Windows-only; `None` elsewhere (so the
/// hover watcher is a no-op off-Windows).
#[cfg(windows)]
fn cursor_pos() -> Option<(f64, f64)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x as f64, p.y as f64))
    } else {
        None
    }
}
#[cfg(not(windows))]
fn cursor_pos() -> Option<(f64, f64)> {
    None
}

/// Hover-to-click: in passive mode, keep the overlay click-through EXCEPT while the cursor sits over
/// a reported interactive control — so HUD buttons (check-in, op controls) work in-game without
/// entering a full capturing mode. `Interact`/`Settings` capture the whole window; a hidden overlay
/// is left alone.
fn spawn_hover_watcher(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let Some(state) = handle.try_state::<AppState>() else {
                continue;
            };
            let capture = state.mode.lock().unwrap().captures_clicks();
            let Some(win) = handle.get_webview_window("main") else {
                continue;
            };
            if !win.is_visible().unwrap_or(false) {
                continue;
            }
            // A capturing mode owns the whole window; in passive mode capture only over a control.
            let over = !capture
                && match (cursor_pos(), win.outer_position()) {
                    (Some((cx, cy)), Ok(origin)) => {
                        let (ox, oy) = (origin.x as f64, origin.y as f64);
                        state.hit_rects.lock().unwrap().iter().any(|[x, y, w, h]| {
                            cx >= ox + x && cx <= ox + x + w && cy >= oy + y && cy <= oy + y + h
                        })
                    }
                    _ => false,
                };
            // Capture in a capturing mode or while hovering a control; pass through otherwise.
            // Idempotent at the OS level, so re-asserting every tick also self-heals any dropped
            // click-through style.
            let _ = win.set_ignore_cursor_events(!(capture || over));
        }
    });
}

/// ~2.8s of sustained "game gone" (4 × 700ms) before the focus watcher actually hides. Showing is
/// immediate; hiding is debounced so a single bad foreground read (a sub-window, a title blip, a
/// brief focus change) can't make the whole overlay randomly vanish mid-game.
const HIDE_AFTER: u8 = 4;

/// The focus watcher's whole decision, as a pure function: given what the window looks like *right
/// now* plus the debounce counter, should the overlay be on screen after this tick, and what is the
/// new counter? Returns `(want_shown, gone_ticks)`.
///
/// Extracted from the loop for two reasons. It is the only way to test any of this — the bugs it
/// encodes are invisible outside a live game session — and, more importantly, `prev_shown` is now a
/// PARAMETER rather than a variable the loop owns. It used to be a task-local `let mut shown`, and
/// only the hide path kept it in step with the visibility hotkey. So: watcher auto-hides
/// (`shown=false`) → user presses show/hide (hides: no-op, already hidden) → presses it again
/// (shows the window; `shown` is still `false`) → the hide branch is guarded by `shown` and can
/// never run again, leaving a fullscreen always-on-top overlay parked over the desktop, ignoring
/// `auto_hide` entirely. The caller now passes `win.is_visible()` — the live `IsWindowVisible`
/// read — so there is no second copy of the truth to drift.
fn next_visibility(
    prev_shown: bool,
    hidden_by_user: bool,
    mode: HudMode,
    auto_hide: bool,
    fg_is_game: bool,
    fg_is_overlay: bool,
    gone_ticks: u8,
) -> (bool, u8) {
    // Either capturing mode (Interact / Settings) or auto-hide off → always shown; else track focus.
    // Settings especially: it draws an OPAQUE backdrop the user is reading, so the foreground is
    // very often neither the game nor (during an alt-tab) the overlay — auto-hiding it mid-edit
    // would be indistinguishable from a crash. `captures_clicks()` covers both without enumerating.
    // Our own foreground counts as "show" so a passive show can't be misread as "game gone".
    // A manual hide (the show/hide hotkey) outranks EVERY show reason — without this veto the watcher
    // re-shows the overlay within one tick and the visibility hotkey looks broken.
    let want =
        !hidden_by_user && (mode.captures_clicks() || !auto_hide || fg_is_game || fg_is_overlay);
    if want {
        return (true, 0);
    }
    let ticks = gone_ticks.saturating_add(1);
    // A manual hide is deliberate, not a flaky foreground read, so it skips the anti-flicker
    // debounce. Until the debounce elapses the overlay simply stays as it is.
    if hidden_by_user || ticks >= HIDE_AFTER {
        (false, ticks)
    } else {
        (prev_shown, ticks)
    }
}

/// Background loop: show the overlay only while Star Citizen is focused (unless `auto_hide` is off or
/// the panel is interactive — settings must stay reachable). Polls the foreground window.
fn spawn_focus_watcher(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut gone_ticks: u8 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(700)).await;
            let Some(state) = handle.try_state::<AppState>() else {
                continue;
            };
            let mode = *state.mode.lock().unwrap();
            let hidden_by_user = *state.hidden_by_user.lock().unwrap();
            let auto_hide = state.config.lock().unwrap().auto_hide;
            let Some(win) = handle.get_webview_window("main") else {
                continue;
            };
            // Read the window, don't remember it. `is_visible()` is a live `IsWindowVisible` call,
            // and both watchers read it — one source of truth, no cached copy to desync.
            let visible = win.is_visible().unwrap_or(true);
            let (want_shown, ticks) = next_visibility(
                visible,
                hidden_by_user,
                mode,
                auto_hide,
                foreground_is_star_citizen(),
                foreground_is_overlay(),
                gone_ticks,
            );
            gone_ticks = ticks;
            if want_shown && !visible {
                show_overlay(&win);
                // Windows drops the WS_EX_TRANSPARENT click-through style when a window is
                // re-shown, so re-assert the cursor-passthrough state for the current mode —
                // otherwise the overlay captures every click after the first alt-tab back
                // into the game (passive → passthrough; interact/settings → capture).
                let _ = win.set_ignore_cursor_events(!mode.captures_clicks());
            } else if !want_shown && visible {
                hide_overlay(&win);
            }
        }
    });
}

fn sync_autostart(app: &tauri::AppHandle, on: bool) -> Result<(), String> {
    let mgr = app.autolaunch();
    let enabled = mgr.is_enabled().unwrap_or(false);
    if on && !enabled {
        mgr.enable().map_err(|e| e.to_string())?;
    } else if !on && enabled {
        mgr.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Push a config into effect: zoom + corner on the overlay, autostart, and a `hud:config` event so
/// the frontend re-applies theme / card / section visibility live.
fn apply_config(app: &tauri::AppHandle, cfg: &HudConfig) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_zoom(cfg.scale);
        make_fullscreen(&win);
    }
    let _ = sync_autostart(app, cfg.start_on_login);
    // Rebinding a hotkey has to take effect on the next keystroke, not the next launch — the whole
    // point of the control is recovering from a bad binding without restarting or editing JSON.
    install_bindings(&cfg.keys);
    let _ = app.emit("hud:config", cfg.clone());
}

// ---- commands exposed to the bundled frontend ----

#[tauri::command]
async fn login(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window("oauth") {
        let _ = existing.set_focus();
        return Ok(());
    }
    let base = Url::parse(&server_url(&app)).map_err(|e| e.to_string())?;
    let url = format!("{}/auth/login?desktop=1", server_url(&app));
    let parsed = Url::parse(&url).map_err(|e| e.to_string())?;
    let handle = app.clone();
    WebviewWindowBuilder::new(&app, "oauth", WebviewUrl::External(parsed))
        .title("Sign in to StarPlatform HUD")
        .inner_size(520.0, 760.0)
        .on_navigation(move |u| {
            // Capture the token ONLY from the configured server's own completion page — never from a
            // foreign origin we may have been redirected to (prevents token planting/fixation).
            if u.path() == "/auth/desktop/complete" && same_origin(u, &base) {
                if let Some(token) = token_from_url(u) {
                    let _ = store_token(&token);
                    let _ = handle.emit("hud:authed", ());
                    if let Some(w) = handle.get_webview_window("oauth") {
                        let _ = w.close();
                    }
                }
            }
            true
        })
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Whether a failed `/auth/desktop/verify` response should drop the stored token. Only a genuine auth
/// rejection (401/403) means the credential is dead; a transient 5xx / rate-limit / gateway error must
/// NOT log the user out — otherwise a momentary server blip silently wipes every member widget and
/// leaves only the public identity card until a manual re-login.
fn should_clear_token(status: u16) -> bool {
    status == 401 || status == 403
}

#[tauri::command]
async fn auth_status(app: tauri::AppHandle) -> Result<Option<serde_json::Value>, String> {
    let Some(token) = load_token() else {
        return Ok(None);
    };
    let url = format!("{}/auth/desktop/verify", server_url(&app));
    let resp = reqwest::Client::new()
        .get(url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.is_success() {
        Ok(Some(resp.json().await.map_err(|e| e.to_string())?))
    } else {
        if should_clear_token(status.as_u16()) {
            let _ = clear_token();
        }
        Ok(None)
    }
}

/// Authed GET returning parsed JSON, reusing `fetch`'s origin guard so the bearer token can never
/// leave the server's origin. `None` on any failure — callers here are all best-effort.
pub(crate) async fn api_get_json(app: &tauri::AppHandle, path: &str) -> Option<serde_json::Value> {
    let body = fetch(app.clone(), path.to_string()).await.ok()?;
    serde_json::from_str(&body).ok()
}

#[tauri::command]
async fn fetch(app: tauri::AppHandle, path: String) -> Result<String, String> {
    let token = load_token().ok_or("not signed in")?;
    // Resolve `path` against the server base and refuse if it would escape the server's origin
    // (e.g. `//evil.com/…` or an absolute URL) — the bearer token must never leave this origin.
    let base = Url::parse(&server_url(&app)).map_err(|e| e.to_string())?;
    let target = base.join(&path).map_err(|e| e.to_string())?;
    if target.origin() != base.origin() {
        return Err("refusing cross-origin request".to_string());
    }
    let resp = reqwest::Client::new()
        .get(target)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(format!("{status}: {body}"))
    }
}

#[tauri::command]
async fn logout(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(token) = load_token() {
        let url = format!("{}/auth/desktop/logout", server_url(&app));
        let _ = reqwest::Client::new()
            .post(url)
            .bearer_auth(token)
            .send()
            .await;
        let _ = clear_token();
    }
    Ok(())
}

/// Frontend-driven mode switch: the Esc / Done escape hatches (`"passive"`), the control bar's gear
/// (`"settings"` / back to `"interact"`), and a click into a text field while passive (`"interact"`,
/// because typing needs window focus). Unknown values fall back to `Passive` — see `HudMode::parse`.
#[tauri::command]
fn set_mode(app: tauri::AppHandle, mode: String) {
    apply_mode(&app, HudMode::parse(&mode));
}

#[tauri::command]
fn get_config(app: tauri::AppHandle) -> HudConfig {
    app.state::<AppState>().config.lock().unwrap().clone()
}

#[tauri::command]
fn set_config(app: tauri::AppHandle, config: HudConfig) -> Result<(), String> {
    let mut config = config;
    if !valid_server(&config.server_url) {
        config.server_url = HudConfig::default().server_url;
    }
    // Same gate as `load_config`: the frontend validates on capture, but this command is the trust
    // boundary, so an unusable set is replaced here rather than persisted and handed to the hook.
    config.keys = config.keys.sanitized();
    save_config(&app, &config)?;
    *app.state::<AppState>().config.lock().unwrap() = config.clone();
    apply_config(&app, &config);
    Ok(())
}

/// Reconcile the Windows media session (C11) with the pushed op context.
///
/// Runs on EVERY context push — the ~15s tick plus every telemetry-driven one — so it must be cheap
/// and idempotent. `Smtc::apply` skips a redundant OS update itself; the only work here is reading
/// the toggle and resolving the window.
///
/// The plan is computed unconditionally, on every platform, so `smtc::plan` stays live on
/// non-Windows CI: this is a bin crate, so tests grant no liveness and anything reachable only from
/// `#[cfg(windows)]` code would be dead there and fail `-D warnings`.
fn apply_media_session(
    app: &tauri::AppHandle,
    state: &AppState,
    op: Option<&hud_protocol::OpContext>,
) {
    let enabled = state.config.lock().map(|c| c.media_keys).unwrap_or(false);
    let plan = smtc::plan(enabled, op);

    // The interop demands a top-level window owned by this process; the overlay is one.
    #[cfg(windows)]
    let hwnd = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize)
        .unwrap_or(0);
    #[cfg(not(windows))]
    let hwnd = 0isize;

    // No window yet (very early startup) — a session cannot be attached, but a teardown still can
    // be skipped safely because there is nothing to tear down.
    if hwnd == 0 && matches!(plan, smtc::SessionPlan::Present(_)) {
        return;
    }

    let handle = app.clone();
    state.smtc.apply(hwnd, &plan, move |button| {
        // Best-effort: the socket may be down. A media key that does nothing is acceptable; a
        // panic on a WinRT callback thread is not.
        let _ = send_member(&handle, smtc::message_for(button));
    });
}

/// The C10 pairing token, generated on first read. Shown in Settings for the user to paste into a
/// local tool's configuration — it is a capability to REACH the HUD, never org authority.
#[tauri::command]
fn local_api_token() -> String {
    local_api::pairing_token(KEYRING_SERVICE)
}

/// Forget the pairing token, which revokes every tool currently paired with this HUD.
#[tauri::command]
fn local_api_revoke() -> Result<(), String> {
    local_api::revoke_token(KEYRING_SERVICE)
}

/// Whether the loopback listener is actually bound, as opposed to merely enabled in settings. The
/// two differ for the whole window between ticking the toggle and restarting the HUD.
#[tauri::command]
fn local_api_listening() -> bool {
    local_api::is_listening()
}

pub(crate) fn send_member(
    app: &tauri::AppHandle,
    msg: hud_protocol::ClientMessage,
) -> Result<(), String> {
    let tx = app.state::<AppState>().member_tx.lock().unwrap().clone();
    tx.ok_or("not connected to the live channel")?
        .send(msg)
        .map_err(|_| "live channel closed".to_string())
}

#[tauri::command]
fn set_objective(app: tauri::AppHandle, index: usize) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::SetObjective { index })
}

#[tauri::command]
fn set_ready(app: tauri::AppHandle, ready: bool) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::SetReady { ready })
}

// Param is single-word `event` on purpose: Tauri v2 maps JS camelCase keys to snake_case Rust
// params, so a multi-word `event_id` would require the JS to send `eventId`. A one-word name is
// unambiguous from either side.
#[tauri::command]
fn check_in(app: tauri::AppHandle, event: String) -> Result<(), String> {
    send_member(
        &app,
        hud_protocol::ClientMessage::CheckIn { event_id: event },
    )
}

#[tauri::command]
fn complete_op(app: tauri::AppHandle) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::CompleteOp)
}

#[tauri::command]
fn set_bed(app: tauri::AppHandle, key: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::SetBed { key })
}

#[tauri::command]
fn set_bed_volume(app: tauri::AppHandle, percent: i64) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::SetBedVolume { percent })
}

#[tauri::command]
fn set_bed_paused(app: tauri::AppHandle, paused: bool) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::SetBedPaused { paused })
}

#[tauri::command]
fn trigger_clip(app: tauri::AppHandle, key: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::TriggerClip { key })
}

/// Fire a clip from the MEMBER clip board (as opposed to the lead-only `trigger_clip`/soundboard
/// above) — separate command mirroring the separate wire variant, since the two have different
/// server-side authorization (any checked-in member + policy/roll-up/budget gates, vs. lead-only).
#[tauri::command]
fn trigger_member_clip(app: tauri::AppHandle, key: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::TriggerMemberClip { key })
}

#[tauri::command]
fn set_clip_policy(app: tauri::AppHandle, policy: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::SetClipPolicy { policy })
}

// ---- op objective sources (C3.5) --------------------------------------------------------------
//
// Four senders for the source editor. Every param is deliberately SINGLE-WORD (`kind`, `id`,
// `label`, `index`): Tauri v2 JSON-decodes the JS arg object and maps camelCase keys onto
// snake_case Rust params, so a two-word `source_id` would silently receive nothing unless the JS
// sent `sourceId`. One-word names cannot be got wrong from either side.
//
// None of these carry authority. `OpContext.can_edit_sources` only decides whether the editor is
// drawn; the server re-resolves lead + `event.manage` live on every message, and denies an
// unrecognised `kind` rather than assuming one.

/// The lead attaches an objective source (a contract or a Field Guide) to their live op. Appends
/// at the tail of the merged rail, so it never renumbers an objective the op is already on.
#[tauri::command]
fn attach_source(app: tauri::AppHandle, kind: String, id: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::AttachSource { kind, id })
}

/// The lead detaches an objective source. Detaching a source that is not last renumbers every
/// objective after it — the server remaps the active stage in the same transaction, and the editor
/// warns before sending (see `seDetachLabel` in the UI).
#[tauri::command]
fn detach_source(app: tauri::AppHandle, kind: String, id: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::DetachSource { kind, id })
}

/// The lead appends a free-text ad-hoc objective to the tail of their live op's rail.
#[tauri::command]
fn add_adhoc_step(app: tauri::AppHandle, label: String) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::AddAdhocStep { label })
}

/// The lead removes one ad-hoc objective by its index **within the ad-hoc tail** — NOT its index on
/// the merged rail. The editor derives that index by enumerating only the steps whose
/// `source_kind == "adhoc"`, which is why the wire stamps every step with its kind.
#[tauri::command]
fn remove_adhoc_step(app: tauri::AppHandle, index: usize) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::RemoveAdhocStep { index })
}

/// Parse a comms channel id received from the frontend as a STRING, not a `u64` — Discord
/// snowflakes exceed JS's safe-integer range (2^53), so a Tauri command param typed `u64` would
/// have the value silently corrupted at the IPC boundary (which JSON-decodes args from JS). The
/// frontend keeps channel ids as strings end-to-end and only this hop parses to `u64`, right
/// before it goes out over the (JSON-free, Rust-to-Rust) `send_member` channel. `None`/empty
/// means "no explicit channel" — the HUD's "Ready room" picker option.
fn parse_comms_channel(channel: Option<String>) -> Result<Option<u64>, String> {
    match channel.as_deref() {
        None | Some("") => Ok(None),
        Some(s) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|_| "invalid channel id".to_string()),
    }
}

#[tauri::command]
fn send_quick_action(
    app: tauri::AppHandle,
    channel: Option<String>,
    action: String,
) -> Result<(), String> {
    let channel = parse_comms_channel(channel)?;
    send_member(
        &app,
        hud_protocol::ClientMessage::SendQuickAction { channel, action },
    )
}

#[tauri::command]
fn send_text(app: tauri::AppHandle, channel: Option<String>, text: String) -> Result<(), String> {
    let channel = parse_comms_channel(channel)?;
    send_member(
        &app,
        hud_protocol::ClientMessage::SendText { channel, text },
    )
}

#[tauri::command]
fn transport_next(app: tauri::AppHandle) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::TransportNext)
}

#[tauri::command]
fn transport_prev(app: tauri::AppHandle) -> Result<(), String> {
    send_member(&app, hud_protocol::ClientMessage::TransportPrev)
}

/// Minimal RFC 4648 base64 encoder (standard alphabet, padded) — used only to inline cover-art
/// bytes into a `data:` URI for the webview. Avoids adding a `base64` crate dependency to this
/// binding-constrained file (`main.rs`/`index.html` only).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Fetch a bed's cover-art bytes from the member-gated `/api/voice/bed/cover` endpoint and return
/// them as a `data:image/<fmt>;base64,...` URI — an `<img src>` can't carry an `Authorization`
/// header, so the webview can't hit that endpoint directly; this command does the authed GET in
/// Rust and hands back an inline URI. `Err`/never-Ok on 404 or any failure so the caller falls back
/// to a placeholder tile.
#[tauri::command]
async fn bed_cover(app: tauri::AppHandle, key: String) -> Result<String, String> {
    let token = load_token().ok_or("not signed in")?;
    let base = Url::parse(&server_url(&app)).map_err(|e| e.to_string())?;
    let mut target = base
        .join("/api/voice/bed/cover")
        .map_err(|e| e.to_string())?;
    target.query_pairs_mut().append_pair("key", &key);
    let resp = reqwest::Client::new()
        .get(target)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(resp.status().to_string());
    }
    let fmt = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.strip_prefix("image/"))
        .unwrap_or("png")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok(format!("data:image/{fmt};base64,{}", base64_encode(&bytes)))
}

/// The frontend reports the physical-pixel boxes (window-relative) of its currently-visible clickable
/// controls. The hover watcher flips click-through off only while the cursor is over one of them.
#[tauri::command]
fn set_hit_rects(app: tauri::AppHandle, rects: Vec<[f64; 4]>) {
    if let Some(state) = app.try_state::<AppState>() {
        *state.hit_rects.lock().unwrap() = rects;
    }
}

/// Open a server-relative path in the system browser. Origin-guarded like `fetch`: a path that
/// resolves off the configured server origin is refused.
#[tauri::command]
fn open_url(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let base = Url::parse(&server_url(&app)).map_err(|e| e.to_string())?;
    let target = base.join(&path).map_err(|e| e.to_string())?;
    if !same_origin(&target, &base) {
        return Err("refusing cross-origin open".to_string());
    }
    app.opener()
        .open_url(target.as_str(), None::<&str>)
        .map_err(|e| e.to_string())
}

// ---- tray ----

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let is_auto = app.autolaunch().is_enabled().unwrap_or(false);
    // THREE toggles, one per hotkey, each named for what it actually does and each an independent
    // escape hatch — off Windows the keyboard hook is a no-op stub (`app`/`appimage`/`dmg` bundle
    // targets), so the tray is the ONLY route to any of them there, and on Windows it is the route
    // that survives a game swallowing the hotkey. Labels name the KEY as well as the mode so the
    // tray teaches the hotkeys instead of duplicating them silently.
    let mi_interact = MenuItemBuilder::with_id("interact", "Interact mode on / off").build(app)?;
    let mi_settings = MenuItemBuilder::with_id("settings", "Settings mode on / off").build(app)?;
    let mi_visible = MenuItemBuilder::with_id("visibility", "Show / hide overlay").build(app)?;
    let mi_auto = CheckMenuItemBuilder::with_id("autostart", "Start on login")
        .checked(is_auto)
        .build(app)?;
    let mi_quit = MenuItemBuilder::with_id("quit", "Quit StarPlatform HUD").build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&mi_interact)
        .item(&mi_settings)
        .item(&mi_visible)
        .item(&mi_auto)
        .separator()
        .item(&mi_quit)
        .build()?;

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("StarPlatform HUD")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "interact" => toggle_interact(app),
            "settings" => toggle_settings(app),
            "visibility" => toggle_visibility(app),
            "autostart" => {
                let want = mi_auto.is_checked().unwrap_or(false);
                let _ = sync_autostart(app, want);
                let state = app.state::<AppState>();
                let cfg = {
                    let mut g = state.config.lock().unwrap();
                    g.start_on_login = want;
                    g.clone()
                };
                let _ = save_config(app, &cfg);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // Left-clicking the tray icon is the cheapest possible escape hatch, so it maps to
                // the mode a user most often wants: Interact. (From Settings it therefore steps down
                // to Interact rather than all the way out — `next_mode`.)
                toggle_interact(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// On launch, check the configured update endpoint and silently install a newer signed release
/// (applied on next start). No-op if the updater isn't configured (no endpoint/pubkey).
fn spawn_update_check(app: &tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(updater) = handle.updater() {
            if let Ok(Some(update)) = updater.check().await {
                let _ = update.download_and_install(|_, _| {}, || {}).await;
            }
        }
    });
}

// Pending-hotkey codes carried by `HOTKEY_QUEUE`.
//
// These, `classify_hotkey`, `binding_matches` and the queue itself are compiled on every platform
// (unlike the hook) so the keyboard truth table and the no-drop invariant are unit-tested on ubuntu
// CI too. `server`-style caveat: this is a BIN crate, so `--all-targets` strips cfg(test) and the
// tests grant no liveness — off Windows the only non-test callers are gone, hence the scoped allow.
// It stays a hard error on Windows, where real deadness would be a defect.
#[cfg_attr(not(windows), allow(dead_code))]
const REQ_NONE: u8 = 0;
/// `keys.interact` (default **Ctrl+Alt+X**) — widgets forward + interactable (`HudMode::Interact`).
#[cfg_attr(not(windows), allow(dead_code))]
const REQ_INTERACT: u8 = 1;
/// `keys.settings` (default **Ctrl+Alt+C**) — the settings surface, with no backdrop.
#[cfg_attr(not(windows), allow(dead_code))]
const REQ_SETTINGS: u8 = 2;
/// `keys.visibility` (default **Ctrl+Alt+Z**) — show/hide the whole overlay.
#[cfg_attr(not(windows), allow(dead_code))]
const REQ_VISIBILITY: u8 = 3;

/// How many un-drained presses the hook can hold. Sized for a burst no human produces: at ~20
/// presses/second a full queue needs the poller stalled for well over a second.
const HOTKEY_QUEUE_LEN: usize = 32;

/// A single-producer / single-consumer ring of pending hotkey presses.
///
/// **Why not the `AtomicU8` this replaced.** One slot plus a 50 ms poller drops any press that lands
/// in the same window as another: the second `store` overwrites the first, and nothing anywhere
/// records that it happened. Two presses 40 ms apart — a deliberate "hide, then show again", or a
/// settings key right after an interact key — silently became one.
///
/// **Why not a bitmask of pending actions.** A bitmask fixes the *dropping* but not the *ordering*:
/// it cannot express "settings then interact" versus "interact then settings", and those two end in
/// different modes. It also collapses a genuine double-press of one key (which for a toggle means
/// "on then off again", i.e. back where you started) into a single toggle, which is a visible,
/// wrong outcome. A queue replays exactly what was pressed, in order.
///
/// Producer is the hook proc (WH_KEYBOARD_LL is serialised onto the installing thread, so there is
/// exactly one), consumer is the poller task. `head`/`tail` are free-running counters — the modulo
/// happens at index time, so a wrap is not a special case.
#[cfg_attr(not(windows), allow(dead_code))]
struct HotkeyQueue {
    slots: [std::sync::atomic::AtomicU8; HOTKEY_QUEUE_LEN],
    head: std::sync::atomic::AtomicUsize,
    tail: std::sync::atomic::AtomicUsize,
    /// Presses lost to a full queue. Never expected to move; a non-zero value here is the signal
    /// that the poller is wedged, which is worth being able to see rather than guess at.
    dropped: std::sync::atomic::AtomicUsize,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl HotkeyQueue {
    const fn new() -> Self {
        Self {
            slots: [const { std::sync::atomic::AtomicU8::new(REQ_NONE) }; HOTKEY_QUEUE_LEN],
            head: std::sync::atomic::AtomicUsize::new(0),
            tail: std::sync::atomic::AtomicUsize::new(0),
            dropped: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Enqueue one press. Producer side — called from the keyboard hook, so it must stay a handful
    /// of atomics: the hook runs on the path Windows walks for every keystroke on the machine, and
    /// exceeding `LowLevelHooksTimeout` (300 ms) gets the hook silently uninstalled.
    fn push(&self, req: u8) {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let head = self.head.load(Relaxed);
        if head.wrapping_sub(self.tail.load(Acquire)) >= HOTKEY_QUEUE_LEN {
            self.dropped.fetch_add(1, Relaxed);
            return;
        }
        self.slots[head % HOTKEY_QUEUE_LEN].store(req, Relaxed);
        // Release: the slot write must be visible before the consumer can observe the new head.
        self.head.store(head.wrapping_add(1), Release);
    }

    /// Dequeue the oldest press, or `None` when empty. Consumer side.
    fn pop(&self) -> Option<u8> {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let tail = self.tail.load(Relaxed);
        if tail == self.head.load(Acquire) {
            return None;
        }
        let req = self.slots[tail % HOTKEY_QUEUE_LEN].load(Relaxed);
        self.tail.store(tail.wrapping_add(1), Release);
        Some(req)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn dropped(&self) -> usize {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Presses seen by the keyboard hook and not yet acted on. See `HotkeyQueue` for why this is a queue.
#[cfg_attr(not(windows), allow(dead_code))]
static HOTKEY_QUEUE: HotkeyQueue = HotkeyQueue::new();

/// True while the settings UI is waiting for the user to press a new combo. Without it, rebinding is
/// self-defeating: pressing the interact combo to *describe* it also *fires* it, which switches out
/// of settings mode and closes the panel the user is rebinding from.
static HOTKEY_SUSPEND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 50 ms poller ticks a capture may stay armed before the poller force-clears it (10 s). A frontend
/// that reloaded or crashed mid-capture never gets to leave the hotkeys dead for the session — the
/// same "silent, sticky, session-long" failure shape as a lost hook.
#[cfg_attr(not(windows), allow(dead_code))]
const SUSPEND_MAX_TICKS: u32 = 200;

/// One poller tick of the capture-suspension watchdog: `(still_suspended, ticks)`. Pure so the
/// timeout is testable without a 10-second sleep, and cross-platform so ubuntu CI runs it.
#[cfg_attr(not(windows), allow(dead_code))]
fn next_suspend(suspended: bool, ticks: u32) -> (bool, u32) {
    // Not armed, or armed for too long (watchdog: assume the frontend went away mid-capture) —
    // either way the hotkeys go back to live and the counter resets.
    if suspended && ticks + 1 < SUSPEND_MAX_TICKS {
        (true, ticks + 1)
    } else {
        (false, 0)
    }
}

/// Arm / disarm the hotkeys while the settings UI captures a new combo. Called by the frontend
/// around a rebind; the watchdog in the poller un-arms it regardless, so a frontend that never
/// sends the `false` cannot strand the user.
#[tauri::command]
fn set_key_capture(capturing: bool) {
    HOTKEY_SUSPEND.store(capturing, std::sync::atomic::Ordering::Relaxed);
}

/// Does this keydown satisfy `b`? Pure, and cross-platform, because getting it wrong lets a hotkey
/// fire while the user is *typing* and there is no other way to observe that.
///
/// ## The AltGr problem, measured on this machine — not assumed
///
/// UK layout, `KLID 00000809`. Two independent measurements, both reproducible with the probe in
/// this branch's history:
///
/// **(a) Layout level, injection-independent.** `VkKeyScanExW` reports the shift state needed for a
/// character in its high byte (`1`=Shift, `2`=Ctrl, `4`=Alt). Twelve characters on this layout come
/// back with **both** the Ctrl and Alt bits set — `€` (`0x06`), `é`, `á`, `í`, `ó`, `ú`, `¦`, and
/// their capitals (`0x07`). Windows models AltGr **as Ctrl+Alt**, at the layout, before any hook.
///
/// **(b) Hook level.** A `WH_KEYBOARD_LL` hook fed by `SendInput(KEYEVENTF_SCANCODE)`, sampling
/// `GetAsyncKeyState` at the moment the letter arrives:
///
/// | Physical keys | msg | LCTRL | RCTRL | LMENU | RMENU |
/// |---|---|---|---|---|---|
/// | `Z`                          | `WM_KEYDOWN`    | up   | up   | up   | up   |
/// | `LeftCtrl`+`LeftAlt`+`Z`     | `WM_KEYDOWN`    | DOWN | up   | DOWN | up   |
/// | `AltGr`+`Z`                  | `WM_SYSKEYDOWN` | **DOWN** | up | up | DOWN |
/// | `RightCtrl`+`RightAlt`+`Z`   | `WM_KEYDOWN`    | up   | DOWN | up   | DOWN |
/// | `LeftCtrl`+`RightAlt`+`Z`    | `WM_KEYDOWN`    | DOWN | up   | up   | DOWN |
/// | `LeftAlt`+`Z` (no Ctrl)      | `WM_SYSKEYDOWN` | up   | up   | DOWN | up   |
///
/// Pressing AltGr alone emitted a **phantom `VK_LCONTROL` with scan code `0x21D`** — a value no
/// keyboard can produce — before the `VK_RMENU` event. So a naive `ctrl_down && alt_down` test is
/// **TRUE for AltGr+Z**, and a Ctrl+Alt binding on that test would fire every time the user typed an
/// AltGr character.
///
/// ## The rule
///
/// `ctrl && LMENU && !RMENU`. In the table above that is true for exactly one row —
/// `LeftCtrl+LeftAlt+Z` — and false for all three RMENU rows, for the bare key and for Alt-only.
/// Implemented as: **`right_alt` vetoes everything**, then an EXACT match of the remaining
/// modifiers (so `Ctrl+Alt+Shift+X` does not fire a `Ctrl+Alt+X` binding, and a hypothetical
/// `Ctrl+X` binding does not fire under `AltGr+X` either, since Alt reads down).
///
/// `RightCtrl+RightAlt` is deliberately rejected too: on an AltGr layout it is indistinguishable
/// from "the user is holding Ctrl while typing an AltGr character". The cost is that the left-hand
/// cluster is the only way to fire a Ctrl+Alt binding — which is the documented, one-handed intent
/// anyway. On a US layout (no AltGr) this rejects a Right-Alt chord that would in principle be safe;
/// that conservatism is accepted, and the Keys tab says so.
#[cfg_attr(not(windows), allow(dead_code))]
fn binding_matches(b: &Binding, vk: u32, m: Mods) -> bool {
    if m.right_alt {
        // AltGr, or a Right Alt we cannot prove is not AltGr. Either way, not a hotkey.
        return false;
    }
    b.vk != 0 && b.vk == vk && b.ctrl == m.ctrl && b.alt == m.left_alt && b.shift == m.shift
}

/// Which action (if any) a keydown triggers, given the live bindings. Ties are impossible —
/// `check_bindings` rejects a colliding set — but the order is fixed anyway so behaviour stays
/// deterministic even for a set that somehow bypassed validation.
#[cfg_attr(not(windows), allow(dead_code))]
fn classify_hotkey(keys: &KeyBindings, vk: u32, m: Mods) -> u8 {
    if binding_matches(&keys.interact, vk, m) {
        REQ_INTERACT
    } else if binding_matches(&keys.settings, vk, m) {
        REQ_SETTINGS
    } else if binding_matches(&keys.visibility, vk, m) {
        REQ_VISIBILITY
    } else {
        REQ_NONE
    }
}

/// Low-level keyboard hook proc. A `WH_KEYBOARD_LL` hook is invoked by the OS for every keystroke
/// system-wide BEFORE the foreground app sees it — so it fires even while Star Citizen holds exclusive
/// fullscreen (where a normal `RegisterHotKey` global hotkey never reaches us). It only READS the key
/// (never injects input into the game), so it's the same mechanism overlays/streamers use.
#[cfg(windows)]
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_SYSKEYDOWN,
    };
    // BOTH arms are load-bearing, and which one a combo takes is not obvious — measured: Alt WITHOUT
    // Ctrl arrives as WM_SYSKEYDOWN (`LeftAlt+Z`, `AltGr+Z`), while Ctrl+Alt arrives as a plain
    // WM_KEYDOWN (`LeftCtrl+LeftAlt+Z`). The shipped defaults take the KEYDOWN arm; a user who
    // rebinds to an Alt-only combo takes the SYSKEYDOWN one.
    if code == HC_ACTION as i32
        && (wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN)
        && !HOTKEY_SUSPEND.load(std::sync::atomic::Ordering::Relaxed)
    {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
        // `GetAsyncKeyState`'s high bit means "physically down right now" — KBDLLHOOKSTRUCT carries
        // no modifier state, so the modifiers must be sampled live at the instant the key fires.
        let down = |vk: i32| (GetAsyncKeyState(vk) as u16 & 0x8000) != 0;
        let mods = Mods {
            ctrl: down(VK_LCONTROL as i32) || down(VK_RCONTROL as i32),
            left_alt: down(VK_LMENU as i32),
            right_alt: down(VK_RMENU as i32),
            shift: down(VK_LSHIFT as i32) || down(VK_RSHIFT as i32),
        };
        let keys = unpack_keys(HOTKEY_BINDINGS.load(std::sync::atomic::Ordering::Relaxed));
        let req = classify_hotkey(&keys, kb.vkCode, mods);
        if req != REQ_NONE {
            HOTKEY_QUEUE.push(req);
        }
    }
    // Always chain — the hook must never swallow the key, so the combo still reaches the game.
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Install the hotkeys via a low-level keyboard hook (works under exclusive fullscreen), plus a
/// poller that turns each queued keypress into the matching toggle on the app side. The bindings
/// themselves come from the config (`HOTKEY_BINDINGS`); the defaults are **Ctrl+Alt+X** → interact,
/// **Ctrl+Alt+C** → settings, **Ctrl+Alt+Z** → overlay show/hide. No-op off-Windows. (Esc / tray /
/// the Done button remain hotkey-independent escapes from all three regardless.)
#[cfg(windows)]
fn spawn_keyboard_hook(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut suspend_ticks = 0u32;
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            use std::sync::atomic::Ordering::Relaxed;
            let (suspended, t) = next_suspend(HOTKEY_SUSPEND.load(Relaxed), suspend_ticks);
            suspend_ticks = t;
            if !suspended {
                HOTKEY_SUSPEND.store(false, Relaxed);
            }
            // DRAIN, don't sample. Acting on one press per tick would re-create the drop this queue
            // exists to remove: a second press already sitting behind the first would wait for the
            // next tick at best, and be overwritten by a third at worst. Draining (rather than
            // clearing from the capture command) also keeps this the queue's ONLY consumer.
            while let Some(req) = HOTKEY_QUEUE.pop() {
                if suspended {
                    continue; // a rebind capture is in flight — describe, don't invoke.
                }
                match req {
                    REQ_INTERACT => toggle_interact(&handle),
                    REQ_SETTINGS => toggle_settings(&handle),
                    REQ_VISIBILITY => toggle_visibility(&handle),
                    _ => {}
                }
            }
        }
    });
    // A low-level hook must live on a thread with a running message loop; give it a dedicated one.
    std::thread::spawn(|| unsafe {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL,
        };
        let hmod = GetModuleHandleW(std::ptr::null());
        // OUTER loop = re-install. Losing the hook kills all three hotkeys for the rest of the
        // session with no user-visible signal, which is exactly what "the hotkeys sometimes stop
        // working" looks like from the outside — so neither failure below is allowed to end the
        // thread. This crate initialises no tracing subscriber, so a `tracing::warn!` would be
        // swallowed; `eprintln!` reaches the console debug builds keep (release sets
        // `windows_subsystem`).
        //
        // NOTE (measured 2026-08-02): this loop does NOT cover the failure that made the hotkeys
        // die after interact mode. Windows stops dispatching keystrokes to this hook while THIS
        // process owns the foreground window, and reports that nowhere — the install succeeded,
        // the pump never returns, and neither branch below can fire. Re-installing (and even
        // moving the hook to a brand-new thread) was measured NOT to recover it; only losing the
        // foreground does. So the fix lives in `apply_mode`/`set_focusable_for_mode`, which stop
        // the overlay taking the foreground in the first place — not here.
        let mut backoff_secs = 1u64;
        loop {
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hmod, 0);
            if hook.is_null() {
                eprintln!(
                    "HUD: SetWindowsHookExW failed — hotkeys unavailable; retrying in \
                     {backoff_secs}s (tray items still work)"
                );
                std::thread::sleep(Duration::from_secs(backoff_secs));
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
            backoff_secs = 1;
            let mut msg: MSG = std::mem::zeroed();
            // `GetMessageW` has THREE outcomes, and the loop must not treat them as two: >0 is a
            // message, 0 is WM_QUIT, and -1 is an error.
            let quitting = loop {
                let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
                if r == -1 {
                    eprintln!(
                        "HUD: keyboard-hook message pump failed (GetMessageW returned -1); \
                         re-installing the hook"
                    );
                    break false;
                }
                if r == 0 {
                    break true; // WM_QUIT — normal shutdown.
                }
            };
            UnhookWindowsHookEx(hook);
            if quitting {
                break;
            }
        }
    });
}

#[cfg(not(windows))]
fn spawn_keyboard_hook(_app: &tauri::AppHandle) {}

fn main() {
    tauri::Builder::default()
        .manage(AppState {
            mode: Mutex::new(HudMode::Passive),
            hidden_by_user: Mutex::new(false),
            config: Mutex::new(HudConfig::default()),
            member_tx: Mutex::new(None),
            hit_rects: Mutex::new(Vec::new()),
            rich_presence: RichPresence::new(),
            smtc: smtc::Smtc::new(),
            // 16 is ample: this carries STATE snapshots, so a slow reader that lags simply skips
            // to the newest board rather than losing a command.
            local_state_tx: tokio::sync::broadcast::channel(16).0,
            local_last_ctx: Mutex::new(None),
            local_checkin: Mutex::new(None),
            local_checkin_at: Mutex::new(None),
        })
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            local_api_token,
            local_api_revoke,
            local_api_listening,
            login,
            auth_status,
            fetch,
            logout,
            set_mode,
            get_config,
            set_config,
            set_objective,
            set_ready,
            check_in,
            complete_op,
            set_bed,
            set_bed_volume,
            set_bed_paused,
            trigger_clip,
            trigger_member_clip,
            set_clip_policy,
            attach_source,
            detach_source,
            add_adhoc_step,
            remove_adhoc_step,
            send_quick_action,
            send_text,
            transport_next,
            transport_prev,
            bed_cover,
            set_hit_rects,
            set_key_capture,
            open_url
        ])
        .setup(move |app| {
            // Load persisted config into shared state.
            let cfg = load_config(app.handle());
            // Before `spawn_keyboard_hook` below, so the very first keystroke already sees the
            // user's own bindings rather than the compiled-in defaults.
            install_bindings(&cfg.keys);
            *app.state::<AppState>().config.lock().unwrap() = cfg.clone();

            let win = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("StarPlatform HUD")
                .inner_size(WIN_W, WIN_H)
                .transparent(true)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .resizable(false)
                // Born unfocusable, not made unfocusable a moment later. `WS_EX_NOACTIVATE` applied
                // AFTER the window already activated does not take the foreground back off it —
                // and nothing else can, because the overlay is topmost, fullscreen and (in passive
                // mode) click-through, so the user has no window to click. Launching focused is
                // therefore enough on its own to leave the hotkeys dead from boot; see `focus_plan`.
                .focused(false)
                .build()?;

            // Start in passive (display-only) mode: clicks fall through to the game beneath, and
            // the window is NOT focusable — an overlay that grabs the foreground on launch (or on
            // any later click-through transition) takes the user's keyboard away from the game.
            // `apply_mode` is the only thing that lifts this, and only for a capturing mode.
            set_focusable_for_mode(&win, focus_plan(HudMode::Passive).focusable);
            win.set_ignore_cursor_events(true)?;
            let _ = win.set_zoom(cfg.scale);
            make_fullscreen(&win);
            let _ = sync_autostart(app.handle(), cfg.start_on_login);

            build_tray(app)?;
            spawn_keyboard_hook(app.handle());
            spawn_update_check(app.handle());
            spawn_focus_watcher(app.handle());
            spawn_hover_watcher(app.handle());
            spawn_member_socket(app.handle());
            // C10: only bind a port when the user has actually asked for it. Read once at
            // startup — a listener that appears mid-session on a settings save would be a
            // surprising thing for a security-relevant socket to do, so this needs a restart.
            {
                let st = app.state::<AppState>();
                let wanted = st
                    .config
                    .lock()
                    .ok()
                    .map(|c| (c.local_api, c.local_api_port));
                if let Some((true, port)) = wanted {
                    local_api::spawn(app.handle().clone(), port, KEYRING_SERVICE);
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the HUD")
        .run(|app_handle, event| {
            // Best-effort: clear any active Discord Rich Presence on the way out so a killed/quit
            // HUD doesn't leave a stale "Playing StarPlatform" on the member's profile. Discord also
            // clears on IPC disconnect, but an explicit clear is the documented-safe pattern.
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    state.rich_presence.shutdown();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_capture_is_origin_gated() {
        let base = Url::parse("http://localhost:8080").unwrap();
        let good = Url::parse("http://localhost:8080/auth/desktop/complete#token=abc-123").unwrap();
        let evil = Url::parse("https://evil.com/auth/desktop/complete#token=stolen").unwrap();
        assert!(same_origin(&good, &base));
        assert!(!same_origin(&evil, &base)); // foreign origin → token NOT captured
                                             // Fragment preferred (never sent to the server); query also parses.
        assert_eq!(token_from_url(&good).as_deref(), Some("abc-123"));
        let q = Url::parse("http://localhost:8080/auth/desktop/complete?token=q-1").unwrap();
        assert_eq!(token_from_url(&q).as_deref(), Some("q-1"));
    }

    #[test]
    fn parse_comms_channel_none_and_empty_mean_default_room() {
        assert_eq!(parse_comms_channel(None).unwrap(), None);
        assert_eq!(parse_comms_channel(Some(String::new())).unwrap(), None);
    }

    #[test]
    fn parse_comms_channel_parses_a_valid_snowflake_string() {
        // A real Discord snowflake exceeds JS's safe-integer range — the whole point of keeping
        // it a string across the IPC boundary is that it survives round-trip intact here.
        assert_eq!(
            parse_comms_channel(Some("123456789012345678".to_string())).unwrap(),
            Some(123456789012345678u64)
        );
    }

    #[test]
    fn parse_comms_channel_rejects_garbage() {
        assert!(parse_comms_channel(Some("not-a-number".to_string())).is_err());
    }

    #[test]
    fn server_url_must_be_http_with_host() {
        assert!(valid_server("http://localhost:8080"));
        assert!(valid_server("https://org.example.com"));
        assert!(!valid_server("ftp://x"));
        assert!(!valid_server("not a url"));
        assert!(!valid_server(""));
    }

    /// The three modes, and the ONE rule that decides what each of them does everywhere:
    /// `captures_clicks()`. Passive is the only pass-through mode; every other consumer
    /// (click-through, the focus watcher's show reason, hover capture, the hide invariant) derives
    /// from this, so pin it here rather than in four places.
    #[test]
    fn only_passive_mode_lets_clicks_through() {
        assert!(!HudMode::Passive.captures_clicks());
        assert!(HudMode::Interact.captures_clicks());
        assert!(
            HudMode::Settings.captures_clicks(),
            "settings mode puts a whole panel of controls on screen — it MUST take the mouse, or \
             every click meant for a setting is handed to the game instead. (It draws no backdrop, \
             so this is now the ONLY thing separating it from passive.)"
        );
        assert_eq!(HudMode::default(), HudMode::Passive, "boot must be passive");
        // Round-trip the wire names the frontend and the `set_mode` command share.
        for m in [HudMode::Passive, HudMode::Interact, HudMode::Settings] {
            assert_eq!(HudMode::parse(m.as_str()), m);
        }
        assert_eq!(
            HudMode::parse("arrange"),
            HudMode::Passive,
            "an unrecognised mode must fail SAFE — releasing the mouse, never capturing it"
        );
    }

    /// The three-mode state machine, including the cross-mode edges that a bool could not express.
    /// Live, these are only observable in a game session, so this is the only place they are checked.
    #[test]
    fn a_mode_hotkey_toggles_its_own_mode_and_switches_between_the_others() {
        use HudMode::*;
        // Pressing the mode you are already in always returns to passive — the "off" half of every
        // toggle, and the reason a hotkey is never a dead end.
        for m in [Interact, Settings] {
            assert_eq!(next_mode(m, m), Passive, "{m:?} must toggle itself off");
            assert_eq!(next_mode(Passive, m), m, "…and back on from passive");
        }
        // Cross edges: settings steps DOWN to interact on the interact key (close the panel, keep
        // interacting) and interact steps UP to settings — neither routes through passive,
        // which would drop click capture for a frame mid-switch.
        assert_eq!(next_mode(Settings, Interact), Interact);
        assert_eq!(next_mode(Interact, Settings), Settings);
    }

    /// Every reason the overlay is allowed on screen, and the one veto that outranks them all.
    /// `gone_ticks` resets on any showing tick so the debounce always measures a *sustained* absence.
    #[test]
    fn overlay_shows_while_the_game_or_the_overlay_itself_is_focused() {
        // (prev_shown, hidden, mode, auto_hide, game, overlay) → shown
        assert_eq!(
            next_visibility(true, false, HudMode::Passive, true, true, false, 3),
            (true, 0)
        );
        assert_eq!(
            next_visibility(true, false, HudMode::Passive, true, false, true, 3),
            (true, 0)
        );
        // BOTH capturing modes are unconditional show reasons, and so is auto_hide=off. Settings
        // especially: auto-hiding the opaque configure screen mid-edit reads as a crash.
        for m in [HudMode::Interact, HudMode::Settings] {
            assert_eq!(
                next_visibility(false, false, m, true, false, false, 0),
                (true, 0),
                "{m:?} must keep the overlay on screen even with the game unfocused"
            );
        }
        assert_eq!(
            next_visibility(false, false, HudMode::Passive, false, false, false, 0),
            (true, 0)
        );
    }

    /// Hiding is debounced by `HIDE_AFTER` ticks; showing is immediate. A single bad foreground read
    /// must not make the overlay vanish mid-game, so the widget stays up until the absence sustains.
    #[test]
    fn auto_hide_waits_for_the_debounce_then_hides() {
        let mut shown = true;
        let mut ticks = 0u8;
        for expect in 1..HIDE_AFTER {
            let (s, t) = next_visibility(shown, false, HudMode::Passive, true, false, false, ticks);
            shown = s;
            ticks = t;
            assert_eq!(
                (shown, ticks),
                (true, expect),
                "must still be shown at tick {expect}"
            );
        }
        let (s, t) = next_visibility(shown, false, HudMode::Passive, true, false, false, ticks);
        assert_eq!(
            (s, t),
            (false, HIDE_AFTER),
            "the {HIDE_AFTER}th gone tick hides"
        );
    }

    /// B2/B3: a manual hide is deliberate, so it skips the debounce AND outranks every show reason —
    /// including BOTH capturing modes, which is exactly the combination that used to strand the user
    /// with an invisible-but-interactive overlay.
    #[test]
    fn a_manual_hide_outranks_every_show_reason_and_skips_the_debounce() {
        for (mode, auto_hide, game, overlay) in [
            (HudMode::Passive, true, true, false),
            (HudMode::Interact, true, true, true),
            (HudMode::Interact, false, false, false),
            (HudMode::Settings, true, true, true),
            (HudMode::Settings, false, false, false),
        ] {
            let (shown, ticks) = next_visibility(true, true, mode, auto_hide, game, overlay, 0);
            assert!(
                !shown,
                "a manual hide must win over mode={mode:?} game={game}"
            );
            assert_eq!(ticks, 1, "…on the very first tick, with no debounce");
        }
    }

    /// B3 — the regression that stranded a fullscreen always-on-top overlay over the desktop.
    /// `prev_shown` used to be a task-local `let mut shown` that only the HIDE path kept in step with
    /// the visibility hotkey: watcher auto-hides (shown=false) → hotkey (hide: already hidden) →
    /// hotkey (window shows; `shown` still false) → the hide branch, guarded by `shown`, could never
    /// run again. Reading the live window each tick is what makes this sequence terminate.
    #[test]
    fn a_manually_reshown_overlay_can_still_be_auto_hidden() {
        // The watcher has already auto-hidden, and the user has just re-shown the window by hotkey:
        // the WINDOW says visible, so that is what the decision sees.
        let mut shown = true;
        let mut ticks = 0u8;
        for _ in 0..HIDE_AFTER {
            let (s, t) = next_visibility(shown, false, HudMode::Passive, true, false, false, ticks);
            shown = s;
            ticks = t;
        }
        assert!(
            !shown,
            "with the live window state as input, auto-hide must still be able to hide it; a stale \
             `shown=false` would skip the hide branch forever and park the overlay on the desktop"
        );
    }

    /// B2, now for all three modes: hiding must drop to passive from EITHER capturing mode, and
    /// un-hiding must restore focus if one somehow survived. The project rule is that Esc / Done /
    /// tray never depend on the hotkey, and Esc is a keydown on a window that is only focused while
    /// a capturing mode holds it — so "invisible AND capturing" has to be unrepresentable.
    #[test]
    fn hiding_drops_to_passive_from_every_mode_and_unhiding_restores_focus() {
        for mode in [HudMode::Interact, HudMode::Settings] {
            let hide = plan_visibility_toggle(true, mode);
            assert!(hide.hide);
            assert!(
                hide.drop_to_passive,
                "hiding while in {mode:?} must drop to passive, or the user is left capturing the \
                 mouse with no window: Esc cannot fire and the next mode hotkey silently toggles \
                 {mode:?} OFF instead of bringing the overlay back"
            );
            let unhide = plan_visibility_toggle(false, mode);
            assert!(!unhide.hide);
            assert!(
                unhide.restore_focus,
                "an un-hide back into {mode:?} must re-focus the window or Esc stays dead"
            );
        }
        assert!(
            !plan_visibility_toggle(true, HudMode::Passive).drop_to_passive,
            "nothing to leave"
        );
        assert!(
            !plan_visibility_toggle(false, HudMode::Passive).restore_focus,
            "passive display never grabs focus"
        );
    }

    /// THE regression test for "after interact mode, no hotkey works until I click elsewhere".
    ///
    /// Measured root cause (2026-08-02, live): while this process owns the OS foreground window,
    /// Windows stops dispatching keystrokes to this process's own `WH_KEYBOARD_LL` hook — so every
    /// hotkey is dead for exactly as long as the overlay holds focus. `apply_mode` used to grab
    /// focus entering a capturing mode and never give it back, which left the overlay holding the
    /// foreground forever afterwards: topmost, fullscreen and click-through, so nothing could take
    /// it. Clicking a pass-through area activated the window underneath and was the only way out,
    /// which is exactly what the reporter found.
    ///
    /// The invariant, stated so it cannot be satisfied by accident: **passive mode may neither hold
    /// nor take the foreground, and every exit from a capturing mode must actively hand it back.**
    #[test]
    fn passive_mode_never_holds_the_os_foreground() {
        let passive = focus_plan(HudMode::Passive);
        assert!(
            !passive.focusable,
            "passive must be WS_EX_NOACTIVATE. tao's `apply_diff` fires ShowWindow(SW_SHOW) — which \
             ACTIVATES — on every click-through transition, and the hover watcher makes one every \
             time the cursor crosses a HUD control. Without this the overlay steals the foreground \
             from the game by itself, and the keyboard hook goes deaf while it holds it."
        );
        assert!(!passive.grab, "passive must never take the foreground");
        assert!(
            passive.release,
            "returning to passive MUST hand the foreground back — this is the reported bug. \
             Without it the overlay keeps focus after the user is done, and every hotkey stays \
             dead until they happen to click a pass-through area."
        );

        for mode in [HudMode::Interact, HudMode::Settings] {
            let plan = focus_plan(mode);
            assert!(
                plan.focusable && plan.grab,
                "{mode:?} captures the mouse and owns Esc, so it must be focusable and take focus"
            );
            assert!(
                !plan.release,
                "{mode:?} is the one place holding the foreground is correct — releasing it here \
                 would kill the Esc escape hatch"
            );
        }

        // Grab and release are opposites, for every mode there is and every mode added later: a
        // mode that does neither leaks the foreground (the bug), and one that does both fights
        // itself.
        for mode in [HudMode::Passive, HudMode::Interact, HudMode::Settings] {
            let plan = focus_plan(mode);
            assert_ne!(
                plan.grab, plan.release,
                "{mode:?} must either take the foreground or give it back, never both or neither"
            );
            assert_eq!(
                plan.focusable,
                mode.captures_clicks(),
                "focusability must track click-capture exactly — they are the same question"
            );
        }
    }

    /// `focus_plan` is only worth anything if `apply_mode` actually routes through it, and the
    /// ORDER matters: focusability has to be applied BEFORE the calls that show the window, because
    /// tao's `SW_SHOW` activates and a flag set afterwards cannot undo an activation that already
    /// happened. Neither fact is visible to a unit test — `apply_mode` needs a live window — so it
    /// is pinned at the source level, the same way this file already pins the tao visibility rule.
    #[test]
    fn apply_mode_applies_focusability_before_it_shows_the_window() {
        const SRC: &str = include_str!("main.rs");
        let body = js_between(
            SRC,
            "fn apply_mode(app: &tauri::AppHandle, mode: HudMode)",
            "\n}\n",
        );
        for needle in [
            "focus_plan(mode)",
            "set_focusable_for_mode(&win, plan.focusable)",
        ] {
            assert!(
                body.contains(needle),
                "`apply_mode` must go through `{needle}` — the focus rule is the whole fix for the \
                 dead-hotkey bug, and a hand-rolled copy here would drift from `focus_plan`; \
                 got:\n{body}"
            );
        }
        // Each foreground act must be GATED ON ITS OWN `plan` field, not merely present. Checking
        // only that the call appears lets `if false { release_foreground(..) }` pass — which is the
        // original bug with the fix still visible in the source.
        for (gate, call) in [
            ("if plan.grab {", "remember_foreground_owner(&win);"),
            ("if plan.release {", "release_foreground(&win);"),
        ] {
            let at_gate = body
                .find(gate)
                .unwrap_or_else(|| panic!("`apply_mode` must gate on `{gate}`; got:\n{body}"));
            let at_call = body
                .find(call)
                .unwrap_or_else(|| panic!("`apply_mode` must call `{call}`; got:\n{body}"));
            // Only comments may sit between the gate and the call — anything else means the call
            // is no longer the first thing that condition does.
            let between_is_comment = at_call > at_gate
                && body[at_gate + gate.len()..at_call]
                    .lines()
                    .all(|l| l.trim().is_empty() || l.trim_start().starts_with("//"));
            assert!(
                between_is_comment,
                "`{call}` must sit DIRECTLY inside `{gate}` — the foreground hand-back is the fix \
                 for the dead-hotkey bug, and a call left in place behind a dead condition looks \
                 correct while doing nothing; got:\n{body}"
            );
        }
        // Match CALL SITES, not the prose: the comment right above the call names
        // `set_ignore_cursor_events` and `show_overlay` too, and a bare-name search finds those
        // first and passes for the wrong reason.
        let focusable_at = body
            .find("set_focusable_for_mode(&win, plan.focusable)")
            .expect("checked above");
        for later in [
            "win.set_ignore_cursor_events(!capture)",
            "show_overlay(&win);",
            "win.set_focus()",
        ] {
            assert!(
                body.find(later).is_some_and(|at| at > focusable_at),
                "`{later}` must come AFTER `set_focusable_for_mode`: it ends in a ShowWindow, and \
                 tao's plain SW_SHOW activates the window — setting WS_EX_NOACTIVATE afterwards \
                 does not take the foreground back off it; got:\n{body}"
            );
        }
        // The window must be born unfocusable too: applying the style a moment after creation is
        // too late for exactly the same reason, and leaves the hotkeys dead from boot.
        let setup = js_between(SRC, "WebviewWindowBuilder::new(app,", "build()?;");
        assert!(
            setup.contains(".focused(false)"),
            "the overlay window must be created unfocused — a focused launch grabs a foreground \
             nothing can take back (topmost + fullscreen + click-through), and the keyboard hook \
             is deaf the whole time; got:\n{setup}"
        );
    }

    /// Same round-trip risk as the media-keys toggle, plus one of its own: `local_api_port` has no
    /// input control, so the save object must carry it FORWARD from the loaded config. Rebuilding
    /// the object without it would silently reset a user's custom port to the default the first time
    /// they touched any other setting.
    #[test]
    fn the_local_api_settings_survive_a_round_trip() {
        const UI: &str = include_str!("../ui/index.html");
        assert!(
            UI.contains(r#"id="s-localapi""#),
            "the local-API toggle needs markup in the settings pane"
        );
        assert!(
            UI.contains("el('s-localapi').checked = c.local_api === true;"),
            "must LOAD from config and compare against `true` — it is default-OFF"
        );
        assert!(
            UI.contains("local_api: el('s-localapi').checked"),
            "the SAVE object must carry local_api or the toggle reverts on any other save"
        );
        assert!(
            UI.contains("local_api_port: (cfg && cfg.local_api_port) || 48291"),
            "the port has no control, so it must be carried forward from the loaded config — and              guarded, because `cfg` is null before the first load (the idiom the rest of this              function already uses)"
        );
        assert!(
            UI.contains("'s-localapi'"),
            "the toggle must be in the change-listener list or it never persists"
        );
        // The pairing key is a capability the user pastes elsewhere; it must be revocable.
        // "Enabled" and "actually listening" differ for the whole window between ticking the
        // toggle and restarting, and a key copied in that window silently will not work.
        assert!(
            UI.contains("invoke('local_api_listening')"),
            "Settings must show whether the listener is REALLY up, not just whether the setting is              ticked — the gap between the two is the trap the first user fell into"
        );
        assert!(
            UI.contains("invoke('local_api_revoke')") && UI.contains("invoke('local_api_token')"),
            "Settings must be able to both show and REVOKE the pairing key — a capability with no              revocation path is one the user can never take back"
        );
    }

    /// `HudConfig` is a named-field struct, so serde silently defaults ANY field the frontend
    /// omits from the object it saves. That is not hypothetical: it is exactly how every widget
    /// added after the old fixed `components` struct was written had its toggle revert on save
    /// (PR #29). `media_keys` is the same shape of risk — a bool the user flips in Settings — so
    /// pin the whole round trip: markup, load, save, default, and the change listener.
    ///
    /// Without the `save` assertion in particular the toggle would look like it worked, then
    /// silently switch itself off the next time the user changed any OTHER setting.
    #[test]
    fn the_media_keys_toggle_survives_a_settings_round_trip() {
        const UI: &str = include_str!("../ui/index.html");
        assert!(
            UI.contains(r#"id="s-mediakeys""#),
            "the media-keys toggle needs markup in the settings pane"
        );
        assert!(
            UI.contains("el('s-mediakeys').checked = c.media_keys === true;"),
            "the toggle must LOAD from config, and compare against `true` — it is default-OFF, so              the `!== false` idiom used by default-on settings would invert it"
        );
        assert!(
            UI.contains("media_keys: el('s-mediakeys').checked"),
            "the SAVE object must carry media_keys; omit it and serde defaults it to false, so the              toggle silently reverts whenever any other setting is saved"
        );
        assert!(
            UI.contains("media_keys: false,"),
            "the frontend's own default config must include the field, default OFF"
        );
        assert!(
            UI.contains("'s-mediakeys'"),
            "the toggle must be in the change-listener list or it never persists at all"
        );
    }

    /// The frontend half of the same fix. In a capturing mode the overlay MUST hold focus (Esc
    /// depends on it), and that is precisely when the Rust hook cannot see the combos — so
    /// "press it again to come back out" is dead there unless the webview handles it. It does
    /// receive those keystrokes (same fact the Esc hatch rests on), so the handler lives there.
    #[test]
    fn the_mode_combos_still_work_while_the_overlay_holds_focus() {
        const UI: &str = include_str!("../ui/index.html");
        assert!(
            UI.contains("if (!interactive || capturingKey) return;"),
            "the in-webview combo handler must be scoped to a capturing mode and must stand aside \
             for a rebind capture, or it fires while the user is describing a new binding"
        );
        assert!(
            UI.contains("for (const action of ['interact', 'settings'])"),
            "both mode combos need the webview fallback — either one can be the mode the user is \
             trying to leave"
        );
        assert!(
            UI.contains("setModeRemote(hudMode === action ? 'passive' : action)"),
            "the fallback must reproduce Rust's `next_mode` table (press the mode you are in → \
             passive), and must go through `set_mode` so Rust stays the source of truth"
        );
        assert!(
            UI.contains("if (e.getModifierState && e.getModifierState('AltGraph')) return;"),
            "AltGr reports as Ctrl+Alt on Windows and `binding_matches` vetoes it — the webview \
             copy must veto it too, or the two disagree about the same keystroke"
        );
    }

    /// Build the modifier snapshot the hook takes, in the terms the measurement uses.
    fn mods(lctrl: bool, rctrl: bool, lmenu: bool, rmenu: bool, shift: bool) -> Mods {
        Mods {
            ctrl: lctrl || rctrl,
            left_alt: lmenu,
            right_alt: rmenu,
            shift,
        }
    }

    /// THE defect this whole disambiguation exists for, transcribed from the WH_KEYBOARD_LL capture
    /// in `binding_matches`'s doc comment (UK layout, `KLID 00000809`, measured — not assumed).
    ///
    /// On an AltGr layout Windows synthesises a phantom **LEFT Ctrl** (impossible scan code `0x21D`)
    /// alongside `VK_RMENU`, so at the moment an AltGr'd letter arrives, `ctrl_down && alt_down` is
    /// **TRUE**. A Ctrl+Alt binding tested that way fires every time the user types `€`, `é`, `á`,
    /// `í`, `ó`, `ú` or `¦` — the twelve characters `VkKeyScanExW` reports as needing Ctrl+Alt on
    /// this layout. There is no way to notice that from inside the app: the hotkey simply fires
    /// "randomly" while typing.
    #[test]
    fn altgr_cannot_fire_a_ctrl_alt_binding() {
        let k = DEFAULT_KEYS;
        // Row by row from the measured table, for the interact key (Ctrl+Alt+X, vk 0x58).
        // LeftCtrl + LeftAlt + X — the ONLY row that may fire.
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(true, false, true, false, false)),
            REQ_INTERACT,
            "the documented left-hand cluster must work"
        );
        // AltGr + X: LCTRL down (phantom), LMENU up, RMENU DOWN.
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(true, false, false, true, false)),
            REQ_NONE,
            "AltGr+X must NOT fire a Ctrl+Alt binding — the naive `ctrl && alt` test reads TRUE \
             here (measured: the phantom LEFT Ctrl at scan 0x21D), which is how a hotkey ends up \
             firing while the user types an accented character"
        );
        // RightCtrl + RightAlt + X, and LeftCtrl + RightAlt + X: both are indistinguishable from
        // "AltGr with a Ctrl held", so both are refused.
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(false, true, false, true, false)),
            REQ_NONE
        );
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(true, false, false, true, false)),
            REQ_NONE
        );
        // Right Alt vetoes EVERY binding, not just the one whose key was pressed.
        for (vk, name) in [(VK_X, "interact"), (VK_C, "settings"), (VK_Z, "visibility")] {
            assert_eq!(
                classify_hotkey(&k, vk, mods(true, true, true, true, false)),
                REQ_NONE,
                "RMENU down must veto the {name} binding too"
            );
        }
    }

    /// The rest of the matching rule: the right key with the wrong modifiers is not a hotkey, and
    /// each of the three defaults reaches its own action.
    #[test]
    fn the_three_default_hotkeys_cannot_collide() {
        let k = DEFAULT_KEYS;
        let good = mods(true, false, true, false, false);
        for (vk, want, name) in [
            (VK_X, REQ_INTERACT, "interact"),
            (VK_C, REQ_SETTINGS, "settings"),
            (VK_Z, REQ_VISIBILITY, "visibility"),
        ] {
            assert_eq!(classify_hotkey(&k, vk, good), want, "{name}");
        }
        // A key nobody bound.
        assert_eq!(classify_hotkey(&k, 0x41 /* A */, good), REQ_NONE);
        // Bare key, Ctrl-only, Alt-only: an EXACT modifier match is required, so a binding cannot be
        // fired by a subset (which would make Ctrl+X — cut — trip interact mode in a chat box).
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(false, false, false, false, false)),
            REQ_NONE
        );
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(true, false, false, false, false)),
            REQ_NONE,
            "Ctrl+X alone must not fire a Ctrl+Alt+X binding — that is the cut shortcut"
        );
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(false, false, true, false, false)),
            REQ_NONE
        );
        // …nor by a superset: Ctrl+Alt+Shift+X is a different combo, and a user may bind it.
        assert_eq!(
            classify_hotkey(&k, VK_X, mods(true, false, true, false, true)),
            REQ_NONE,
            "an extra Shift must not satisfy a Shift-less binding, or Shift+combo becomes unbindable"
        );
        // A vk of 0 is the `Binding::default()` hole — it must never match anything.
        let hole = KeyBindings {
            interact: Binding::default(),
            ..DEFAULT_KEYS
        };
        assert_eq!(classify_hotkey(&hole, 0, good), REQ_NONE);
        // Every request code is distinct, or two hotkeys silently share one intent.
        let codes = [REQ_NONE, REQ_INTERACT, REQ_SETTINGS, REQ_VISIBILITY];
        for (i, a) in codes.iter().enumerate() {
            assert!(
                !codes[i + 1..].contains(a),
                "the four request codes must be distinct"
            );
        }
    }

    /// A rebind takes effect on the NEXT keystroke, which means the hook has to read the live set.
    /// The bindings cross into the hook proc through a packed `u64`, so the pack/unpack pair is the
    /// one place a rebind can silently become a no-op (or, worse, a *different* combo).
    #[test]
    fn bindings_round_trip_through_the_packed_word_the_hook_reads() {
        for k in [
            DEFAULT_KEYS,
            KeyBindings {
                interact: Binding {
                    vk: 0xFF,
                    ctrl: true,
                    alt: true,
                    shift: true,
                },
                settings: Binding {
                    vk: 0x70,
                    ctrl: false,
                    alt: true,
                    shift: false,
                },
                visibility: Binding {
                    vk: 0x01,
                    ctrl: true,
                    alt: false,
                    shift: true,
                },
            },
        ] {
            assert_eq!(
                unpack_keys(pack_keys(&k)),
                k,
                "every field must survive the trip through the hook's atomic word"
            );
        }
        // The three bindings must not overlap in the word — a shifted-in bit landing on a neighbour
        // would rebind a DIFFERENT action than the one the user changed.
        let a = pack_keys(&KeyBindings {
            interact: Binding {
                vk: 0xFF,
                ctrl: true,
                alt: true,
                shift: true,
            },
            settings: Binding::default(),
            visibility: Binding::default(),
        });
        assert_eq!(unpack_keys(a).settings, Binding::default());
        assert_eq!(unpack_keys(a).visibility, Binding::default());
        // …and the live static starts at the compiled-in defaults, so the very first keystroke
        // after launch (before any config load) is already classified correctly.
        assert_eq!(
            unpack_keys(pack_keys(&DEFAULT_KEYS)),
            DEFAULT_KEYS,
            "HOTKEY_BINDINGS' initialiser is pack_keys(&DEFAULT_KEYS)"
        );
    }

    /// The defaults are the recovery path for a user who binds something unreachable, so they have
    /// to be legal by the same rules a user capture is held to.
    #[test]
    fn the_default_bindings_are_the_agreed_ctrl_alt_cluster_and_are_valid() {
        assert_eq!(check_bindings(&DEFAULT_KEYS), Ok(()));
        assert_eq!(KeyBindings::default(), DEFAULT_KEYS);
        assert_eq!(HudConfig::default().keys, DEFAULT_KEYS);
        for (b, vk, name) in [
            (DEFAULT_KEYS.interact, VK_X, "interact"),
            (DEFAULT_KEYS.settings, VK_C, "settings"),
            (DEFAULT_KEYS.visibility, VK_Z, "visibility"),
        ] {
            assert_eq!(
                b,
                Binding {
                    vk,
                    ctrl: true,
                    alt: true,
                    shift: false
                },
                "{name} must default to Ctrl+Alt+{}",
                (vk as u8) as char
            );
        }
    }

    /// Validation is the only thing standing between the user and a binding that fires while they
    /// type. Each rule is here because breaking it is silent: the hotkey just starts going off.
    #[test]
    fn a_binding_that_would_fire_while_typing_is_rejected() {
        let ok = Binding {
            vk: VK_X,
            ctrl: true,
            alt: true,
            shift: false,
        };
        assert_eq!(check_binding(&ok), Ok(()));
        // No key at all (the `Binding::default()` hole, or a truncated config object).
        assert!(check_binding(&Binding { vk: 0, ..ok }).is_err());
        assert!(check_binding(&Binding { vk: 0x100, ..ok }).is_err());
        // A modifier as the MAIN key fires the instant the user reaches for any chord.
        for vk in MODIFIER_VKS {
            assert!(
                check_binding(&Binding { vk, ..ok }).is_err(),
                "vk 0x{vk:02X} is a modifier and cannot be a binding's main key"
            );
        }
        // Bare key — fires on every press in-game.
        assert!(check_binding(&Binding {
            ctrl: false,
            alt: false,
            ..ok
        })
        .is_err());
        // Shift-only is the same hazard: it fires on every capital letter typed in chat.
        assert!(
            check_binding(&Binding {
                ctrl: false,
                alt: false,
                shift: true,
                ..ok
            })
            .is_err(),
            "Shift alone is not a modifier for this purpose — it fires while typing capitals"
        );
        // Ctrl alone and Alt alone are each enough.
        assert_eq!(check_binding(&Binding { alt: false, ..ok }), Ok(()));
        assert_eq!(check_binding(&Binding { ctrl: false, ..ok }), Ok(()));
        // A collision makes ONE action permanently unreachable, silently — the fixed resolution
        // order in classify_hotkey means the loser simply never fires.
        let collide = KeyBindings {
            settings: DEFAULT_KEYS.interact,
            ..DEFAULT_KEYS
        };
        assert!(check_bindings(&collide).is_err());
        assert_eq!(
            collide.sanitized(),
            DEFAULT_KEYS,
            "an unusable set must fall back to the defaults WHOLESALE — repairing one binding can \
             collide with the two that were kept"
        );
        assert_eq!(DEFAULT_KEYS.sanitized(), DEFAULT_KEYS, "a good set is kept");
        // A hand-edited config that leaves a bare key in place must not survive the load path.
        let bare = KeyBindings {
            visibility: Binding {
                vk: VK_Z,
                ctrl: false,
                alt: false,
                shift: false,
            },
            ..DEFAULT_KEYS
        };
        assert_eq!(bare.sanitized(), DEFAULT_KEYS);
    }

    /// C — the dropped keypress. The hook used to signal through ONE `AtomicU8` drained every 50 ms,
    /// so a second press inside that window overwrote the first and vanished with no trace. This is
    /// the regression test for that, and for the two properties a bitmask alternative would lose:
    /// ORDER (settings-then-interact ends in a different mode than the reverse) and COUNT (two
    /// presses of one toggle mean "on then off", not "on").
    #[test]
    fn the_hotkey_signal_cannot_drop_or_reorder_a_press() {
        let q = HotkeyQueue::new();
        assert_eq!(q.pop(), None, "an empty queue yields nothing");

        // Two presses inside one poll window: the OLD single-slot signal kept only the second.
        q.push(REQ_VISIBILITY);
        q.push(REQ_INTERACT);
        assert_eq!(
            q.pop(),
            Some(REQ_VISIBILITY),
            "the FIRST press must survive"
        );
        assert_eq!(q.pop(), Some(REQ_INTERACT));
        assert_eq!(q.pop(), None);

        // Order is preserved, and it matters: these two sequences end in different modes.
        for seq in [[REQ_SETTINGS, REQ_INTERACT], [REQ_INTERACT, REQ_SETTINGS]] {
            for r in seq {
                q.push(r);
            }
            assert_eq!([q.pop().unwrap(), q.pop().unwrap()], seq);
        }

        // Repeats are not coalesced — a bitmask would collapse these to one toggle, leaving the
        // overlay in the opposite state to the one the user's two presses asked for.
        q.push(REQ_INTERACT);
        q.push(REQ_INTERACT);
        assert_eq!(
            (q.pop(), q.pop(), q.pop()),
            (Some(REQ_INTERACT), Some(REQ_INTERACT), None)
        );

        // Capacity: a full queue drops the NEWEST (and says so) rather than corrupting the ring.
        assert_eq!(q.dropped(), 0);
        for _ in 0..HOTKEY_QUEUE_LEN {
            q.push(REQ_INTERACT);
        }
        q.push(REQ_SETTINGS);
        assert_eq!(q.dropped(), 1, "the overflow must be counted, not silent");
        for _ in 0..HOTKEY_QUEUE_LEN {
            assert_eq!(q.pop(), Some(REQ_INTERACT));
        }
        assert_eq!(q.pop(), None);

        // The head/tail counters free-run and wrap; drive them past one full lap to prove indexing
        // by modulo has no seam at the boundary.
        for i in 0..HOTKEY_QUEUE_LEN * 3 + 5 {
            let want = [REQ_INTERACT, REQ_SETTINGS, REQ_VISIBILITY][i % 3];
            q.push(want);
            assert_eq!(q.pop(), Some(want), "lap {i}");
        }
    }

    /// The hook proc is on the path Windows walks for EVERY keystroke on the machine; exceeding
    /// `LowLevelHooksTimeout` (300 ms) gets it silently uninstalled, which is itself a candidate
    /// cause of "the hotkeys stopped working". So the signal path must stay allocation-free and
    /// lock-free — a text guard, because the cost is invisible in every behavioural test.
    #[cfg(windows)]
    #[test]
    fn the_hook_proc_takes_no_lock_and_allocates_nothing() {
        const SRC: &str = include_str!("main.rs");
        let body = js_between(
            SRC,
            "unsafe extern \"system\" fn keyboard_hook_proc",
            "\n}\n",
        );
        for banned in ["lock()", "Vec", "String", "format!", "println!", "collect"] {
            assert!(
                !body.contains(banned),
                "`{banned}` in the hook proc: it runs before every keystroke system-wide, and a \
                 stall past LowLevelHooksTimeout makes Windows silently uninstall the hook; got:\n\
                 {body}"
            );
        }
        // …and the bindings must reach it through the atomic word, not a shared lock.
        assert!(
            body.contains("HOTKEY_BINDINGS.load("),
            "the hook must read the live bindings from the atomic; got:\n{body}"
        );
    }

    /// Losing the hook kills all three hotkeys for the rest of the session with NO user-visible
    /// signal. Both ways it can be lost have to re-install, not end the thread.
    #[cfg(windows)]
    #[test]
    fn a_lost_keyboard_hook_is_re_installed() {
        const SRC: &str = include_str!("main.rs");
        let body = js_between(
            SRC,
            "fn spawn_keyboard_hook(app: &tauri::AppHandle)",
            "\n}\n",
        );
        assert!(
            body.matches("SetWindowsHookExW(WH_KEYBOARD_LL").count() == 1
                && body.contains("continue;"),
            "an install failure must back off and retry, not `return` and leave the session with \
             no hotkeys; got:\n{body}"
        );
        assert!(
            body.contains("UnhookWindowsHookEx(hook)"),
            "the old hook must be released before a re-install, or each retry leaks one"
        );
        assert!(
            !body.contains("return;"),
            "no early return in the hook thread — every failure path has to loop back to a fresh \
             SetWindowsHookExW; got:\n{body}"
        );
        // The poller must DRAIN, not sample: acting on one press per 50 ms tick re-creates the very
        // drop the queue was introduced to remove.
        assert!(
            body.contains("while let Some(req) = HOTKEY_QUEUE.pop()"),
            "the poller must drain the whole queue each tick; got:\n{body}"
        );
    }

    /// The hook must never swallow the key: `CallNextHookEx` is unconditional and its value is what
    /// the proc returns, so the combo still reaches the game. A `return 1` slipped in behind a mode
    /// branch would be invisible in every other test here.
    #[cfg(windows)]
    #[test]
    fn the_keyboard_hook_always_chains() {
        const SRC: &str = include_str!("main.rs");
        let body = js_between(
            SRC,
            "unsafe extern \"system\" fn keyboard_hook_proc",
            "\n}\n",
        );
        assert!(
            body.trim_end()
                .ends_with("CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)"),
            "keyboard_hook_proc must END by returning CallNextHookEx — an early return swallows the \
             key from the game; got:\n{body}"
        );
        assert_eq!(
            body.matches("return").count(),
            0,
            "no early return in the hook proc — every path has to reach CallNextHookEx"
        );
    }

    /// B1 — the highest-severity defect this file has carried: mixing raw `ShowWindow` with tao's
    /// own visibility desyncs `WindowFlags::VISIBLE`, and `apply_diff` (reached by EVERY
    /// `set_ignore_cursor_events` transition) ends with an unconditional
    /// `if !new.contains(VISIBLE) { ShowWindow(SW_HIDE) }`. One `win.hide()` anywhere is enough to
    /// make hovering a control hide the overlay permanently. So: exactly one of each call in the
    /// whole file, and both inside the helpers that exist to own them.
    #[test]
    fn overlay_visibility_never_routes_through_tao() {
        const SRC: &str = include_str!("main.rs");
        let code = SRC
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must still have a non-test half");
        for (call, helper) in [
            ("let _ = win.hide();", "fn hide_overlay("),
            ("let _ = win.show();", "fn show_overlay("),
        ] {
            assert_eq!(
                code.matches(call).count(),
                1,
                "`{call}` must appear exactly once — inside {helper}. Every other visibility change \
                 has to go through show_overlay/hide_overlay, or tao's cached WindowFlags::VISIBLE \
                 desyncs and the next set_ignore_cursor_events transition hides the overlay for good"
            );
            assert!(
                js_between(code, helper, "\n}\n").contains(call),
                "the one `{call}` must be the fallback inside `{helper}`"
            );
        }
    }

    /// F4: the tray is the ONLY route to any of the three toggles on macOS/Linux (the keyboard hook
    /// compiles to a no-op stub there) and the hotkey-independent escape hatch on Windows, and its
    /// label used to promise "Show / hide panel" while calling the arrange toggle. Pin label ↔
    /// action for ALL THREE so they cannot drift apart, and so a new mode cannot ship tray-less.
    ///
    /// The labels deliberately no longer NAME a key. They used to ("Interact mode (Pause)…"), which
    /// was good documentation while the bindings were fixed — but the tray menu is built once at
    /// launch, so after a rebind a key-naming label states something false, and a tray item that
    /// lies about its shortcut is worse than one that stays silent. The Keys tab, which renders from
    /// the live config, is the single place the current combos are shown.
    #[test]
    fn tray_exposes_all_three_toggles_wired_to_what_they_name() {
        const SRC: &str = include_str!("main.rs");
        for (id, label, action) in [
            ("interact", "Interact mode on / off", "toggle_interact(app)"),
            ("settings", "Settings mode on / off", "toggle_settings(app)"),
            (
                "visibility",
                "Show / hide overlay",
                "toggle_visibility(app)",
            ),
        ] {
            assert!(
                SRC.contains(&format!("with_id(\"{id}\", \"{label}\")")),
                "the tray must offer a `{id}` item labelled \"{label}\""
            );
            assert!(
                SRC.contains(&format!("\"{id}\" => {action}")),
                "the tray's \"{label}\" item must call {action}"
            );
        }
    }

    /// Settings mode's ONLY visual difference from interact mode is that it drops the scrim
    /// entirely, and that lives in CSS keyed off a body class the JS sets — exactly the coupling
    /// that breaks silently (a renamed class kills the effect with no error, and no text-based
    /// guard notices because both halves still read fine on their own). So pin the whole chain:
    /// Rust emits the mode name → JS sets `body.settings` → CSS cancels the pseudo-element, in that
    /// source order.
    ///
    /// It has to be `content:none` and not a transparent background. The overlay is a fullscreen
    /// always-on-top window over a live game: a transparent-but-present box is still a real
    /// fullscreen box in the tree, which is the shape that regresses back into painting something
    /// the moment anyone "restores" a background. Passive mode already proves the pattern — it
    /// generates no box at all — and settings must match it.
    #[test]
    fn settings_mode_draws_no_backdrop() {
        const UI: &str = include_str!("../ui/index.html");
        const SRC: &str = include_str!("main.rs");
        let style = js_between(UI, "<style>", "</style>");
        let scrim = style
            .find("body.interactive #overlay::before")
            .expect("the interact-mode scrim rule must exist");
        let none = style.find("body.settings #overlay::before").expect(
            "settings mode must cancel the scrim with `body.settings #overlay::before` — without \
                 it the interact tint veils the game while the user is configuring the HUD",
        );
        assert!(
            none > scrim,
            "`body.settings #overlay::before` must be declared AFTER the interactive one: the two \
             selectors have identical specificity, so source order is the only thing letting it \
             cancel the scrim"
        );
        let rule = &style[none..];
        let rule = &rule[..rule.find('}').expect("the settings rule must have a body")];
        assert!(
            rule.contains("content:none"),
            "settings mode must generate NO backdrop box: the rule has to be `content:none`, the \
             same nothing passive mode renders; got: {rule}"
        );
        // Any paint at all here is the bug this replaced: an opaque `--bg` hid the game behind the
        // configure screen. `background` covers `background`/`background-color`/the shorthand.
        for token in ["background", "backdrop-filter", "var(--bg)"] {
            assert!(
                !rule.contains(token),
                "`{token}` in the settings backdrop rule paints something over the game — settings \
                 mode must be completely transparent; got: {rule}"
            );
        }
        // The JS half: the class must actually be set, and only ever alongside `interactive` — the
        // settings rule exists solely to cancel the box the interactive rule creates, so on its own
        // it is a no-op and the two classes have to travel together.
        let set_mode = js_between(UI, "function setMode(m){", "\n  }");
        for cls in [
            "'interactive', interactive",
            "'settings', hudMode === 'settings'",
        ] {
            assert!(
                set_mode.contains(&format!("classList.toggle({cls})")),
                "setMode must drive `body` with `classList.toggle({cls})`; got:\n{set_mode}"
            );
        }
        // …and Rust must be the one telling it, with the same three wire names.
        assert!(
            SRC.contains("app.emit(\"hud:mode\", mode.as_str())"),
            "apply_mode must push the mode to the frontend as `hud:mode`"
        );
        assert!(
            UI.contains("listen('hud:mode', e => setMode(e.payload))"),
            "the frontend must listen for `hud:mode` — otherwise the backdrop never changes"
        );
    }

    /// Strip `/* … */` comments out of a stylesheet, so a rule search cannot be satisfied (or
    /// derailed) by prose *about* the rule. Safe because `the_overlay_stylesheet_has_balanced_comments`
    /// separately proves every opener has a closer.
    fn css_without_comments(style: &str) -> String {
        let mut out = String::with_capacity(style.len());
        let mut rest = style;
        while let Some(open) = rest.find("/*") {
            out.push_str(&rest[..open]);
            match rest[open..].find("*/") {
                Some(close) => rest = &rest[open + close + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// The interact tint has FIVE copies of one number spread over three languages — the CSS
    /// default, the JS clamp range, the slider's markup attributes, Rust's serde default and the
    /// persisted field name — and every way they can disagree is silent:
    ///
    /// * a field-level `#[serde(default)]` on an f64 hands back 0.0, so every existing user would
    ///   upgrade to *no* tint and nothing would say so;
    /// * `saveFromForm()` rebuilds `cfg` from the DOM, so a `tint_opacity` missing from that object
    ///   literal is destroyed on the next settings save (the `sizes` trap, again);
    /// * a range input silently CLAMPS to its own markup attributes, so markup narrower than the JS
    ///   clamp rewrites the user's value behind their back;
    /// * and a theme that re-declares `--arrange-scrim` is simply dead, because `applyTint` writes
    ///   the property inline on `<html>`, which outranks every `:root[data-theme=…]` rule — the
    ///   slider would then read as broken on that one theme only.
    #[test]
    fn the_interact_tint_is_configurable_and_defaults_to_30() {
        const UI: &str = include_str!("../ui/index.html");
        let style = css_without_comments(js_between(UI, "<style>", "</style>"));

        // 1. ONE declaration of the custom property, at the documented default, and the scrim must
        //    actually read it.
        assert_eq!(
            style.matches("--arrange-scrim:").count(),
            1,
            "`--arrange-scrim` must be declared exactly once (in :root). A second declaration in a \
             theme block is dead code — applyTint sets the property inline on <html>, which wins — \
             so the slider would silently do nothing on that theme"
        );
        assert!(
            style.contains("--arrange-scrim:30%"),
            "the CSS default must be the same 30% Rust and the frontend default to; got: {style}"
        );
        let scrim = js_between(UI, "body.interactive #overlay::before{", "}");
        assert!(
            scrim.contains("var(--arrange-scrim)") && scrim.contains("var(--arrange-wash)"),
            "the interact scrim must mix --arrange-wash (accent-derived, so all four themes and a \
             runtime org accent work with no colour table) at --arrange-scrim; got: {scrim}"
        );

        // 2. The canonical JS range, parsed the same way the scale range is.
        let decl = UI
            .lines()
            .find(|l| l.trim_start().starts_with("const TINT_MIN ="))
            .expect("ui/index.html must declare the canonical `const TINT_MIN = …` range");
        let num = |name: &str| -> f64 {
            decl.split_once(&format!("{name} = "))
                .unwrap_or_else(|| {
                    panic!("`{name}` must be declared on the same line; got: {decl}")
                })
                .1
                .trim_start()
                .split([',', ';', ' '])
                .next()
                .and_then(|n| n.parse::<f64>().ok())
                .unwrap_or_else(|| panic!("`{name}` must be a literal number; got: {decl}"))
        };
        let (min, max, step, dflt) = (
            num("TINT_MIN"),
            num("TINT_MAX"),
            num("TINT_STEP"),
            num("TINT_DEFAULT"),
        );
        assert_eq!(
            min, 0.0,
            "the range must reach a TRUE zero — 'no tint at all' is a supported preference, and a \
             floor of 1% or 5% would leave a permanent veil on the game with no way off"
        );
        assert!(
            (10.0..100.0).contains(&max),
            "the range must offer a strong tint but must not reach opaque: the scrim paints over \
             the game and UNDER every widget, so 100% hides the GAME (the exact thing that was just \
             removed from settings mode) rather than the HUD; got max {max}"
        );
        let steps = (max - min) / step;
        assert!(
            step > 0.0 && (steps - steps.round()).abs() < 1e-6,
            "TINT_STEP must divide the range exactly, or the slider's top position is a value the \
             config cannot round-trip; got {min}..{max} step {step}"
        );

        // 3. All three declarations of the DEFAULT agree — CSS, JS and Rust.
        assert_eq!(
            dflt, TINT_DEFAULT,
            "the frontend's TINT_DEFAULT must equal Rust's — they are the two halves of one default"
        );
        assert_eq!(
            HudConfig::default().tint_opacity,
            TINT_DEFAULT,
            "a fresh config must start at the documented default"
        );
        assert_eq!(TINT_DEFAULT, 30.0, "the agreed default tint is 30%");
        assert!(
            (min..=max).contains(&TINT_DEFAULT),
            "the default must be inside the slider's own range, or a fresh install opens with a \
             value the control cannot show; got {min}..{max} default {TINT_DEFAULT}"
        );

        // 4. The slider's markup must not be narrower (or wider) than the clamp it feeds.
        let row = UI
            .lines()
            .find(|l| l.contains("id=\"s-tint\""))
            .expect("the General pane must render the `s-tint` slider");
        for (attr, want) in [("min", min), ("max", max), ("step", step)] {
            assert!(
                row.contains(&format!("{attr}=\"{want}\"")),
                "the tint slider's {attr} must be {want} (a range input clamps to its own markup, \
                 so drift here silently rewrites the saved value); got: {row}"
            );
        }

        // 5. saveFromForm() rebuilds cfg from the DOM — the field has to be NAMED in that literal.
        let save = js_between(
            UI,
            "async function saveFromForm()",
            "applyConfig(cfg, true)",
        );
        assert!(
            save.contains("tint_opacity:"),
            "saveFromForm() rebuilds cfg from scratch, so `tint_opacity` must appear in that object \
             literal — otherwise every settings save silently drops the user's tint back to the \
             default (the same trap `sizes` hit); got: {save}"
        );

        // 6. Applied on every config path, and LIVE while the slider moves — the whole point of the
        //    control is judging the tint against the game, which needs no save round-trip.
        assert!(
            js_between(UI, "function applyConfig(c, fromForm)", "renderData()")
                .contains("applyTint("),
            "applyConfig must push the saved tint into CSS, or a config pushed from Rust never \
             reaches the scrim"
        );
        let live = UI
            .lines()
            .find(|l| l.contains("el('s-tint').addEventListener('input'"))
            .expect(
                "the tint slider needs an `input` listener — on `change` alone the scrim only moves \
                 once the drag ends, which is exactly when the user can no longer aim it",
            );
        assert!(
            live.contains("applyTint("),
            "the tint slider's `input` handler must call applyTint; got: {live}"
        );
        assert!(
            UI.contains("el('s-tint').value = tintOf(c)"),
            "populateForm must seed the tint slider from the config, or opening settings shows an \
             empty control that saveFromForm then writes back"
        );
    }

    /// Adding `tint_opacity` must not disturb a config written before it existed — and it must not
    /// arrive as 0.0, which is a *valid* value meaning "no tint" and would be indistinguishable from
    /// a deliberate choice. `#[serde(default = "default_tint")]` is the whole fix; a bare
    /// `#[serde(default)]` on an f64 field overrides the container default and yields 0.0.
    #[test]
    fn config_written_before_the_tint_existed_still_loads() {
        let old = r#"{"server_url":"http://localhost:8080","start_on_login":false,"theme":"nyx",
            "corner":"bottom-left","scale":1.1,"auto_hide":true,
            "components":{"op":{"show":true,"scale":1.2}},
            "positions":{"card":[10.0,20.0]},"sizes":{"sect-op":[420.0,0.0]}}"#;
        let c: HudConfig = serde_json::from_str(old).expect("a pre-tint config must still load");
        assert_eq!(
            c.tint_opacity, TINT_DEFAULT,
            "a missing `tint_opacity` must fall back to the default tint, NOT to f64::default() — \
             0.0 is the legitimate 'no tint' setting, so a zero here silently turns the tint off \
             for every existing user and looks like they chose it"
        );
        assert_eq!(
            c.theme, "nyx",
            "existing preferences must survive the upgrade"
        );
        assert_eq!(c.scale, 1.1);
        assert_eq!(c.sizes.get("sect-op"), Some(&[420.0, 0.0]));

        // …and the other direction: a chosen value round-trips, including a deliberate zero.
        for want in [0.0, 15.0, 60.0] {
            let mut c2 = c.clone();
            c2.tint_opacity = want;
            let back: HudConfig =
                serde_json::from_str(&serde_json::to_string(&c2).unwrap()).unwrap();
            assert_eq!(
                back.tint_opacity, want,
                "a saved tint of {want}% must round-trip unchanged"
            );
        }
    }

    /// Parse one `name: { vk: 0xNN, ctrl: b, alt: b, shift: b },` line out of the frontend's
    /// `KEY_DEFAULTS` literal. Deliberately strict — a reshaped literal fails loudly here rather
    /// than silently stopping the comparison below from checking anything.
    fn js_default_binding(ui: &str, action: &str) -> Binding {
        let lit = js_between(ui, "const KEY_DEFAULTS = {", "\n  };");
        let line = lit
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("{action}:")))
            .unwrap_or_else(|| panic!("KEY_DEFAULTS must have a `{action}:` line; got:\n{lit}"));
        let field = |name: &str| -> &str {
            line.split_once(&format!("{name}: "))
                .unwrap_or_else(|| panic!("`{action}` must declare `{name}`; got: {line}"))
                .1
                .split([',', ' ', '}'])
                .next()
                .unwrap()
        };
        Binding {
            vk: u32::from_str_radix(field("vk").trim_start_matches("0x"), 16)
                .unwrap_or_else(|_| panic!("`{action}.vk` must be a hex literal; got: {line}")),
            ctrl: field("ctrl") == "true",
            alt: field("alt") == "true",
            shift: field("shift") == "true",
        }
    }

    /// The defaults live in TWO languages — Rust ships them and validates them, the frontend renders
    /// them and is the "reset to defaults" button's source. Every way they can disagree is silent:
    /// the Keys tab would confidently display a combo that does nothing, or Reset would write a set
    /// Rust then replaces. Same class as `TINT_DEFAULT`, same fix — pin them to each other.
    #[test]
    fn the_default_key_bindings_agree_across_rust_and_the_frontend() {
        const UI: &str = include_str!("../ui/index.html");
        for (action, want) in [
            ("interact", DEFAULT_KEYS.interact),
            ("settings", DEFAULT_KEYS.settings),
            ("visibility", DEFAULT_KEYS.visibility),
        ] {
            assert_eq!(
                js_default_binding(UI, action),
                want,
                "ui/index.html's KEY_DEFAULTS.{action} must equal Rust's DEFAULT_KEYS.{action}"
            );
        }
        // The frontend's action names ARE the field names of Rust's `KeyBindings` — they serialise
        // straight into the config object, so a rename on either side drops the binding silently.
        let acts = js_between(UI, "const KEY_ACTIONS = [", "\n  ];");
        for action in ["interact", "settings", "visibility"] {
            assert!(
                acts.contains(&format!("['{action}',")),
                "KEY_ACTIONS must list `{action}` — it is a serde field name, not a label"
            );
        }
    }

    /// The Keys tab is the only in-app documentation of the hotkeys AND now the only way to change
    /// them. A hard-coded key here would start lying the moment anyone rebound it, so the three
    /// hotkey rows must be RENDERED from the config — and the tab must still carry the fixed rows
    /// (Esc) plus the two recovery affordances.
    #[test]
    fn the_keys_tab_renders_the_live_bindings_and_can_reset_them() {
        const UI: &str = include_str!("../ui/index.html");
        let keys = js_between(UI, "data-pane=\"keys\"", "</div>\n        </div>");
        assert!(
            keys.contains("id=\"key-list\""),
            "the Keys tab must host the rendered binding rows in #key-list"
        );
        // Key NAMES are checked across the WHOLE file, not just this pane: the gear button's
        // tooltip named a key too ("Settings mode (Right Alt+Pause)"), and a stale tooltip is the
        // same lie somewhere nobody thinks to re-read. Scoped to markup-ish tokens so a JS comment
        // that merely mentions a key is not swept up.
        for stale in ["<kbd>Pause</kbd>", "Right Alt", "Right Ctrl"] {
            assert!(
                !UI.contains(stale),
                "ui/index.html still hard-codes the key `{stale}` — the bindings are configurable \
                 now, so a literal combo anywhere in this markup goes false on the first rebind"
            );
        }
        assert!(
            !keys.contains("Arrange mode"),
            "the Keys tab still describes the removed two-mode `arrange` design"
        );
        // Esc is deliberately NOT rebindable: it is the escape hatch the hotkeys are allowed to
        // fail without, so it must keep its own fixed row.
        let at = keys
            .find("<kbd>Esc</kbd>")
            .expect("the Keys tab must still document Esc");
        let row_end = keys[at..]
            .find("</div>")
            .map(|e| at + e)
            .unwrap_or(keys.len());
        assert!(keys[at..row_end].contains("pass-through"));
        // RECOVERY. A user who binds something their keyboard cannot produce has to be able to get
        // back without hand-editing config.json.
        assert!(
            keys.contains("id=\"b-keyreset\""),
            "the Keys tab must offer a reset-to-defaults control"
        );
        assert!(
            UI.contains("el('b-keyreset').onclick"),
            "…and it must be wired up"
        );
        assert!(
            js_between(UI, "el('b-keyreset').onclick", "};").contains("persist()"),
            "the reset must actually be saved, or it lasts until the next config push"
        );
        // The AltGr rule is the one piece of behaviour a user cannot discover by experiment (the
        // combo just does nothing), so the tab has to state it.
        assert!(
            keys.contains("AltGr") && keys.contains("left-hand"),
            "the Keys tab must explain that AltGr is unusable and Ctrl+Alt means the LEFT-hand \
             pair, or a user who tries AltGr concludes rebinding is broken; got:\n{keys}"
        );
        // The rows are rendered, and each one names its action and offers a rebind button.
        let render = js_between(UI, "function renderKeys()", "\n  }");
        assert!(
            render.contains("bindLabel(all[k])") && render.contains("data-bind=\"${k}\""),
            "renderKeys must draw the LIVE combo and a per-action rebind control; got:\n{render}"
        );
    }

    /// Capturing a combo must not INVOKE it. Pressing the interact key to rebind it would otherwise
    /// switch to interact mode, which closes the settings panel the user is rebinding from — the
    /// feature would be unusable for the one action people most want to change.
    #[test]
    fn a_rebind_capture_suspends_the_hotkeys_and_cannot_get_stuck() {
        const UI: &str = include_str!("../ui/index.html");
        assert!(
            js_between(UI, "function setCapture(action)", "\n  }")
                .contains("invoke('set_key_capture', { capturing: !!action })"),
            "arming a capture must suspend the Rust hook"
        );
        // …and closing the panel must disarm, or the suspension outlives the UI that set it.
        assert!(
            js_between(UI, "function showSettings(on)", "\n  }").contains("setCapture(null)"),
            "closing settings must cancel an in-flight capture"
        );
        // Esc cancels the capture WITHOUT leaving the mode: the handler is on the capture phase and
        // stops propagation, so the bubble-phase Esc → passive listener never sees it.
        let cap = js_between(UI, "if (!capturingKey) return;", "}, true);");
        assert!(
            cap.contains("e.stopPropagation()") && cap.contains("code === 'Escape'"),
            "the capture handler must swallow Esc and use it to cancel; got:\n{cap}"
        );
        assert!(
            cap.contains("getModifierState('AltGraph')"),
            "the capture must REJECT AltGr — the browser reports it as plain Ctrl+Alt, so it would \
             otherwise store a binding the hook deliberately refuses to fire; got:\n{cap}"
        );
        assert!(
            cap.contains("bindError(b, action, all)"),
            "the capture must run the shared validation (bare key / collision); got:\n{cap}"
        );
        // The watchdog: Rust un-arms on its own, so a reload mid-capture cannot kill the hotkeys.
        assert_eq!(next_suspend(false, 0), (false, 0));
        assert_eq!(next_suspend(true, 0), (true, 1));
        assert_eq!(
            next_suspend(true, SUSPEND_MAX_TICKS - 1),
            (false, 0),
            "an armed capture must time out — a frontend that went away mid-rebind would otherwise \
             leave every hotkey dead for the rest of the session, with no way to tell why"
        );
        // A suspended tick must DROP what it drains rather than replaying it once un-armed: the
        // presses were the user describing a combo, not asking for the action.
        let poller = js_between(
            include_str!("main.rs"),
            "fn spawn_keyboard_hook(app: &tauri::AppHandle)",
            "\n}\n",
        );
        assert!(
            poller.contains("if suspended {") && poller.contains("continue;"),
            "the poller must discard queued presses while a capture is armed; got:\n{poller}"
        );
    }

    /// The `sizes` trap, third time. `saveFromForm()` rebuilds `cfg` from the DOM and the hotkeys
    /// have NO control on that form — they are rebound by capture on the Keys tab — so leaving them
    /// out of the literal resets all three the moment the user ticks any checkbox, and the user has
    /// no way to connect the two events.
    #[test]
    fn key_bindings_survive_a_settings_save() {
        const UI: &str = include_str!("../ui/index.html");
        let body = js_between(
            UI,
            "async function saveFromForm()",
            "applyConfig(cfg, true)",
        );
        assert!(
            body.contains("keys: keysOf(cfg)"),
            "saveFromForm() must carry `keys` over explicitly (`keys: keysOf(cfg)`) — a bare \
             `cfg.keys` would write a hole for a config that predates the field; got: {body}"
        );
        // The offline fallback config must be COMPLETE for the same reason it must carry a tint:
        // populateForm renders from it, and the next save writes exactly what it rendered.
        assert!(
            js_between(UI, "const defaultCfg = () => ({", "});").contains("keys: keysOf(null)"),
            "defaultCfg() must include a full binding set"
        );
    }

    /// The three places a binding set crosses a trust or staleness boundary. Each one is silent when
    /// missed: an unsanitized set means a hand-edited `config.json` can install a bare-key hotkey
    /// that fires all game; an unpublished set means a rebind appears to save and then does nothing
    /// until the next launch, which reads as "rebinding is broken".
    #[test]
    fn load_and_save_both_sanitize_the_binding_set() {
        const SRC: &str = include_str!("main.rs");
        let code = SRC
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must still have a non-test half");
        for (func, marker) in [
            (
                "fn load_config(app: &tauri::AppHandle) -> HudConfig",
                "cfg.keys = cfg.keys.sanitized();",
            ),
            (
                "fn set_config(app: tauri::AppHandle, config: HudConfig)",
                "config.keys = config.keys.sanitized();",
            ),
            // apply_config is on every save path, so this is what makes a rebind live immediately.
            (
                "fn apply_config(app: &tauri::AppHandle, cfg: &HudConfig)",
                "install_bindings(&cfg.keys);",
            ),
        ] {
            assert!(
                js_between(code, func, "\n}\n").contains(marker),
                "`{func}` must contain `{marker}`"
            );
        }
        // And boot must publish BEFORE the hook is installed, or the first keystroke of the session
        // is classified against the compiled-in defaults rather than the user's own bindings.
        let setup = js_between(code, ".setup(move |app| {", "spawn_update_check");
        let install = setup
            .find("install_bindings(&cfg.keys)")
            .expect("setup must publish the loaded bindings");
        let hook = setup
            .find("spawn_keyboard_hook(")
            .expect("setup must install the keyboard hook");
        assert!(
            install < hook,
            "install_bindings must run before spawn_keyboard_hook"
        );
    }

    /// Adding `keys` must not disturb a config written before it existed, and — the whole point of
    /// `#[serde(default = "default_keys")]` over a bare `#[serde(default)]` — it must not arrive as
    /// a `Binding::default()` hole, which is a set of three unbindable keys and would read to the
    /// user as "the hotkeys just stopped working after an update".
    #[test]
    fn config_written_before_the_keys_existed_still_loads() {
        let old = r#"{"server_url":"http://localhost:8080","start_on_login":false,"theme":"nyx",
            "corner":"bottom-left","scale":1.1,"auto_hide":true,"tint_opacity":45.0,
            "components":{"op":{"show":true,"scale":1.2}},
            "positions":{"card":[10.0,20.0]},"sizes":{"sect-op":[420.0,0.0]}}"#;
        let c: HudConfig = serde_json::from_str(old).expect("a pre-keys config must still load");
        assert_eq!(
            c.keys, DEFAULT_KEYS,
            "a missing `keys` must fall back to the real defaults, NOT to three zeroed Bindings"
        );
        assert_eq!(
            c.theme, "nyx",
            "existing preferences must survive the upgrade"
        );
        assert_eq!(c.tint_opacity, 45.0);

        // A PARTIAL set (one action rebound, or a truncated write) fills the rest from the defaults
        // instead of failing the whole config or zeroing the others.
        let partial = r#"{"server_url":"http://localhost:8080",
            "keys":{"interact":{"vk":112,"ctrl":true,"alt":false,"shift":false}}}"#;
        let p: HudConfig = serde_json::from_str(partial).unwrap();
        assert_eq!(p.keys.interact.vk, 0x70, "the rebound action must be kept");
        assert!(p.keys.interact.ctrl && !p.keys.interact.alt);
        assert_eq!(p.keys.settings, DEFAULT_KEYS.settings);
        assert_eq!(p.keys.visibility, DEFAULT_KEYS.visibility);

        // …and the other direction: a chosen set round-trips unchanged through serde.
        let mut c2 = c.clone();
        c2.keys.settings = Binding {
            vk: 0x77,
            ctrl: false,
            alt: true,
            shift: true,
        };
        let back: HudConfig = serde_json::from_str(&serde_json::to_string(&c2).unwrap()).unwrap();
        assert_eq!(back.keys, c2.keys, "a saved binding set must round-trip");
    }

    #[test]
    fn foreground_titles_classify_game_vs_overlay() {
        // The auto-hide watcher must tell the game apart from its own overlay window, or showing the
        // overlay gets misread as "game gone" and it flickers.
        assert!(title_is_star_citizen("Star Citizen"));
        assert!(!title_is_star_citizen("StarPlatform HUD"));
        assert!(title_is_overlay("StarPlatform HUD"));
        assert!(!title_is_overlay("Star Citizen"));
    }

    #[test]
    fn only_auth_rejections_clear_the_token() {
        assert!(should_clear_token(401)); // unauthorized → token really is dead
        assert!(should_clear_token(403)); // forbidden → drop it
        assert!(!should_clear_token(500)); // transient server error must NOT log the user out
        assert!(!should_clear_token(502));
        assert!(!should_clear_token(429)); // rate limited → keep the token
    }

    /// The frontend reports the screen boxes of its clickable controls to `set_hit_rects`; the hover
    /// watcher drops click-through ONLY over one of them. So anything the selector misses is dead in
    /// display mode — the click falls through to the game — and it only works in arrange mode, which
    /// drops click-through globally. That masked the defect for a long time: `#control` (gear, Done)
    /// and the whole `#settings` panel were never reported because the selector was scoped to `.sect`.
    /// The JS has no test runner, so pin the selector here (same guard pattern the panel uses for its
    /// toggle-JS ↔ CSS-class coupling).
    #[test]
    fn hit_rect_selector_covers_every_overlay_control() {
        const UI: &str = include_str!("../ui/index.html");
        let sel = UI
            .lines()
            .find(|l| l.contains("const HIT_SELECTOR"))
            .expect("ui/index.html must declare `const HIT_SELECTOR` for the hit-rect report");
        // Every interactive tag the overlay uses, matched anywhere under #overlay.
        for tag in ["button", "input", "select", "textarea", "a"] {
            assert!(
                sel.contains(&format!("#overlay {tag}")),
                "hit-rect selector must cover <{tag}> anywhere under #overlay — got: {sel}"
            );
        }
        // Re-narrowing to a widget class is the exact regression that made #control/#settings dead.
        assert!(
            !sel.contains(".sect"),
            "hit-rect selector must not be re-scoped to .sect — #control and #settings would stop \
             being reported and become unclickable outside arrange mode; got: {sel}"
        );
        // The control containers the widened selector exists to reach must still be in the markup,
        // and inside #overlay (the selector's root).
        let overlay = UI
            .split_once("<div id=\"overlay\">")
            .expect("ui/index.html must have the #overlay root")
            .1;
        for id in ["id=\"control\"", "id=\"settings\"", "id=\"card\""] {
            assert!(
                overlay.contains(id),
                "{id} must live inside #overlay for its controls to be reported"
            );
        }
    }

    /// The settings panel groups its component toggles under headings (`COMP_GROUPS`), but the saved
    /// shape is still the flat `COMPS` map — and `saveFromForm()` rebuilds that map *entirely* from
    /// the DOM. So a `COMPS` entry that no group lists renders no row, and the next save writes it
    /// back as `{show:true, scale:1.0}` — silently discarding whatever the user had chosen. That is
    /// the same class of bug as PR #29's fixed-struct config (unknown keys dropped on round-trip),
    /// and it is invisible until someone notices a widget keeps reappearing. No JS test runner
    /// exists, so pin the two lists against each other here.
    /// Also pins `COMP_PRESETS` against `COMPS`, because it needs the same parser and the same
    /// list — one place should know how to read these literals.
    #[test]
    fn settings_groups_cover_every_component() {
        const UI: &str = include_str!("../ui/index.html");

        /// Quoted strings in a JS array literal, restricted to one bracket depth. `COMPS` is
        /// `[['key','Label'], …]` so its keys are the FIRST string at depth 2; `COMP_GROUPS` is
        /// `[['Title', ['key', …]], …]` so its keys are every string at depth 3+ (depth 2 is the
        /// group heading). Scanning by depth rather than by `split('[')` is what keeps the two
        /// shapes apart — a flat split would read group titles as keys and miss all but the first
        /// key of each cluster.
        fn strings_at(block: &str, depth_eq: usize, first_only: bool) -> Vec<String> {
            let b = block.as_bytes();
            let (mut out, mut depth, mut want, mut i) = (Vec::new(), 0usize, false, 0usize);
            while i < b.len() {
                match b[i] {
                    b'[' => {
                        depth += 1;
                        if depth == depth_eq {
                            want = true;
                        }
                        i += 1;
                    }
                    b']' => {
                        depth = depth.saturating_sub(1);
                        i += 1;
                    }
                    q if q == b'\'' || q == b'"' => {
                        let start = i + 1;
                        let mut j = start;
                        while j < b.len() && b[j] != q {
                            j += 1;
                        }
                        let hit = if first_only {
                            depth == depth_eq && want
                        } else {
                            depth >= depth_eq
                        };
                        if hit {
                            out.push(block[start..j].to_string());
                            want = false;
                        }
                        i = j + 1;
                    }
                    _ => i += 1,
                }
            }
            out
        }
        fn literal<'a>(ui: &'a str, decl: &str) -> &'a str {
            let start = ui
                .find(decl)
                .unwrap_or_else(|| panic!("ui/index.html must declare `{decl}`"));
            let body = &ui[start..];
            let end = body
                .find("];")
                .unwrap_or_else(|| panic!("`{decl}` must be an array literal ending in `];`"));
            &body[..end]
        }

        let comps = strings_at(literal(UI, "const COMPS = ["), 2, true);
        let grouped = strings_at(literal(UI, "const COMP_GROUPS = ["), 3, false);
        // Same shape as COMP_GROUPS — `[['Name', 'why', ['key', …]], …]` — so the widget keys are
        // again every string at depth 3; the name and blurb sit at depth 2.
        let preset_keys = strings_at(literal(UI, "const COMP_PRESETS = ["), 3, false);
        assert!(
            comps.len() >= 17,
            "expected to parse the full COMPS list, got {comps:?}"
        );
        // Floor, not an equality — a real coverage gap must report WHICH key below, not just a count.
        assert!(
            grouped.len() >= 5,
            "expected to parse the COMP_GROUPS clusters, got {grouped:?}"
        );
        // Panes are hidden with CSS, never unmounted — the invariant the coverage check rests on.
        // If a redesign ever swaps this for removing/rebuilding panes, every component not on the
        // active tab disappears from the DOM and saveFromForm() resets it.
        let dense: String = UI.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            dense.contains(".set-pane{display:none;}"),
            "settings panes must be hidden with `display:none`, not removed from the DOM"
        );

        // A preset that names a widget which no longer exists would quietly show one fewer thing
        // than it claims — and because `applyPreset` writes the checkboxes, the stale key simply
        // matches nothing and the widget stays hidden with no error anywhere.
        assert!(
            preset_keys.len() >= 3,
            "expected to parse the COMP_PRESETS clusters, got {preset_keys:?}"
        );
        for k in &preset_keys {
            assert!(
                comps.contains(k),
                "preset key `{k}` is not in COMPS — that preset would silently show one fewer                  widget than it lists; got components {comps:?}"
            );
        }

        for k in &comps {
            assert!(
                grouped.contains(k),
                "component `{k}` is in COMPS but in no COMP_GROUPS cluster — it would render no \
                 settings row, and saveFromForm() would then reset it to its default on the next \
                 save; got groups {grouped:?}"
            );
        }
        for k in &grouped {
            assert!(
                comps.contains(k),
                "COMP_GROUPS lists `{k}`, which is not a COMPS component — it renders nothing and \
                 is almost certainly a typo of a real key (whose row is therefore missing)"
            );
        }
        // A key listed twice would render two rows bound to the same [data-show] attribute;
        // querySelector() takes the first, so the second silently does nothing when clicked.
        let mut seen = grouped.clone();
        seen.sort();
        let before = seen.len();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "a component is grouped twice: {grouped:?}"
        );
    }

    /// Slice out one JS function body from the UI, from `start` up to (and excluding) `end`.
    /// Both markers are asserted to exist, so a rename shows up as a clear failure here rather than
    /// as a guard that silently stops guarding anything.
    fn js_between<'a>(ui: &'a str, start: &str, end: &str) -> &'a str {
        let from = ui
            .find(start)
            .unwrap_or_else(|| panic!("ui/index.html must contain `{start}`"));
        let rest = &ui[from..];
        let to = rest
            .find(end)
            .unwrap_or_else(|| panic!("`{start}` must still contain `{end}`"));
        &rest[..to]
    }

    /// Every name listed inside `tauri::generate_handler![…]`.
    fn registered_commands(src: &str) -> Vec<String> {
        js_between(src, "tauri::generate_handler![", "])")
            .split(['[', ',', '\n'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .map(str::to_string)
            .collect()
    }

    /// Every function carrying `#[tauri::command]`.
    fn defined_commands(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        let lines: Vec<&str> = src.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            if l.trim() != "#[tauri::command]" {
                continue;
            }
            let name = lines[i + 1..]
                .iter()
                .find_map(|n| {
                    let t = n
                        .trim()
                        .trim_start_matches("pub ")
                        .trim_start_matches("async ");
                    t.strip_prefix("fn ").and_then(|r| r.split('(').next())
                })
                .unwrap_or_else(|| panic!("`#[tauri::command]` on line {} names no fn", i + 1));
            out.push(name.to_string());
        }
        out
    }

    /// A `#[tauri::command]` that is DEFINED but never listed in `generate_handler!` compiles clean,
    /// passes clippy, and is a **silently dead control** at runtime: `invoke('x')` rejects with
    /// "command not found" and every call site in the overlay swallows it in a `.catch()`. That has
    /// already cost this HUD one whole feature, and `CLAUDE.md` calls it out by name. Nothing in the
    /// language pairs the two lists, so pin them here.
    #[test]
    fn every_tauri_command_is_registered() {
        const SRC: &str = include_str!("main.rs");
        let registered = registered_commands(SRC);
        let defined = defined_commands(SRC);
        assert!(
            defined.len() >= 25,
            "expected the full command surface, found {} — the scanner broke, not the code",
            defined.len()
        );
        for name in &defined {
            assert!(
                registered.contains(name),
                "`{name}` is a #[tauri::command] but is NOT in generate_handler![…] — it is a \
                 silently dead control: invoke('{name}') will reject at runtime with no UI error"
            );
        }
        // The C3.5 source editor's four senders specifically — named so a future refactor that
        // drops one fails on the name rather than on a count.
        for name in [
            "attach_source",
            "detach_source",
            "add_adhoc_step",
            "remove_adhoc_step",
        ] {
            assert!(
                defined.contains(&name.to_string()) && registered.contains(&name.to_string()),
                "the source editor's `{name}` command must be defined AND registered"
            );
        }
    }

    /// The other half of the same trap, from the JS side: the overlay calling `invoke('x')` for an
    /// `x` no Rust command answers. Identical symptom — a control that does nothing, with the
    /// failure buried in a `.catch()` — but invisible to the guard above, because the dead name is
    /// in the HTML rather than in `main.rs`.
    #[test]
    fn every_invoke_in_the_ui_names_a_registered_command() {
        const UI: &str = include_str!("../ui/index.html");
        const SRC: &str = include_str!("main.rs");
        let registered = registered_commands(SRC);
        let mut seen = 0usize;
        for (i, _) in UI.match_indices("invoke('") {
            let rest = &UI[i + "invoke('".len()..];
            let name = &rest[..rest.find('\'').expect("unterminated invoke('…')")];
            seen += 1;
            assert!(
                registered.contains(&name.to_string()),
                "ui/index.html calls invoke('{name}') but no such #[tauri::command] is registered \
                 — the control it drives is silently dead"
            );
        }
        assert!(
            seen >= 25,
            "expected the overlay's full invoke surface, found {seen} — the scanner broke"
        );
    }

    /// The build-once rule, pinned. `sectionWidget()` assigns `node.innerHTML` wholesale on every
    /// render, so any widget owning a text input must build its shell ONCE and thereafter rewrite
    /// only its body — otherwise a context push (~15s, plus every op event) wipes what the lead is
    /// halfway through typing. The Field Guide already lives by this rule; the C3.5 source editor is
    /// the second widget to, and it owns TWO inputs.
    ///
    /// Three independent ways to break it, all silent, all pinned here:
    ///   1. dropping the `if (el('sect-opedit'))` early return, so every push rebuilds the shell;
    ///   2. moving an input inside `#se-body`, which `seRender()` legitimately overwrites;
    ///   3. leaving `sect-opedit` out of `renderData()`'s `selfManaged` list, so the 15s REST poll's
    ///      end-of-pass cleanup deletes the whole widget — the exact flap CLAUDE.md documents.
    #[test]
    fn the_source_editor_is_built_once_and_self_managed() {
        const UI: &str = include_str!("../ui/index.html");

        let ensure = js_between(UI, "function ensureSourceEditor(", "\n  function ");
        assert!(
            ensure.contains("if (el('sect-opedit')){ seRender(); placeSourceEditor(); return; }"),
            "ensureSourceEditor must return early once the widget exists (build once, update in \
             place) — without it every context push rebuilds the shell and wipes both inputs"
        );
        for id in ["id=\"se-q\"", "id=\"se-adhoc\"", "id=\"se-body\""] {
            assert!(
                ensure.contains(id),
                "{id} must be created by ensureSourceEditor (the build-once shell)"
            );
        }
        // The shell must hand seRender an EMPTY body. Written this way, nothing the shell builds can
        // end up inside the element seRender legitimately overwrites — which is what keeps the two
        // inputs out of harm's way structurally rather than by convention.
        assert!(
            ensure.contains("<div id=\"se-body\"></div>"),
            "the build-once shell must give seRender an empty #se-body — anything nested inside it \
             is wiped on the next context push"
        );

        let render = js_between(UI, "function seRender(", "\n  // Contracts come from");
        assert!(
            render.contains("const b = el('se-body');"),
            "seRender must write into #se-body"
        );
        for id in ["se-q", "se-adhoc"] {
            assert!(
                !render.contains(id),
                "seRender must not touch #{id} — the stateful inputs live OUTSIDE #se-body, and \
                 that separation is what survives a push"
            );
        }

        let cleanup = js_between(UI, "const selfManaged = id =>", ";\n");
        assert!(
            cleanup.contains("'sect-opedit'"),
            "sect-opedit must be excluded from renderData()'s end-of-pass cleanup, or the 15s REST \
             poll removes it every time — got: {cleanup}"
        );
    }

    /// The four source-editor commands are one-line adapters from a JS arg object to a
    /// `ClientMessage`, so the thing worth pinning is what actually leaves the process: the
    /// externally-tagged JSON the server's `ws_member` dispatch matches on. Paired with the
    /// browser-measured `invoke()` payloads, this closes the loop from a click to the wire — the
    /// param names on the Rust side are what Tauri decodes the JS object into, and these literals
    /// are what the server then reads.
    #[test]
    fn the_source_editor_commands_serialise_to_the_expected_wire_json() {
        use hud_protocol::ClientMessage as M;
        let j = |m: M| serde_json::to_string(&m).unwrap();
        assert_eq!(
            j(M::AttachSource {
                kind: "contract".into(),
                id: "c-aaa".into()
            }),
            r#"{"type":"attach_source","kind":"contract","id":"c-aaa"}"#
        );
        assert_eq!(
            j(M::DetachSource {
                kind: "guide".into(),
                id: "g-111".into()
            }),
            r#"{"type":"detach_source","kind":"guide","id":"g-111"}"#
        );
        assert_eq!(
            j(M::AddAdhocStep {
                label: "Refuel at Everus".into()
            }),
            r#"{"type":"add_adhoc_step","label":"Refuel at Everus"}"#
        );
        assert_eq!(
            j(M::RemoveAdhocStep { index: 1 }),
            r#"{"type":"remove_adhoc_step","index":1}"#
        );

        // …and each command must construct the variant its own name promises. The four bodies are
        // near-identical one-liners, so a copy-paste that leaves `attach_source` sending
        // `DetachSource` compiles, passes clippy, and silently detaches what the lead meant to
        // attach — with the HUD reporting success.
        const SRC: &str = include_str!("main.rs");
        for (cmd, variant) in [
            ("attach_source", "AttachSource"),
            ("detach_source", "DetachSource"),
            ("add_adhoc_step", "AddAdhocStep"),
            ("remove_adhoc_step", "RemoveAdhocStep"),
        ] {
            let body = js_between(SRC, &format!("fn {cmd}(app: tauri::AppHandle"), "\n}");
            assert!(
                body.contains(&format!("ClientMessage::{variant}")),
                "`{cmd}` must send ClientMessage::{variant} — got: {body}"
            );
        }
    }

    /// Per-step source attribution on a merged rail, and the one subtle way to lose it.
    ///
    /// `objectiveLine(o, groupName)` suppresses the suffix when the phase header above the step
    /// already names its source. Reverting the call site to the point-free `items.map(objectiveLine)`
    /// still runs — `Array.map` just passes the ARRAY INDEX as the second argument, so `groupName`
    /// becomes a number, the comparison never matches, and every row grows a redundant suffix. It
    /// throws nothing and renders something plausible, so only a guard catches it.
    #[test]
    fn the_objective_rail_attributes_each_step_to_its_source() {
        const UI: &str = include_str!("../ui/index.html");

        let line = js_between(
            UI,
            "function objectiveLine(",
            "\n  // Mirrors the Rust `group()`",
        );
        assert!(
            line.contains("function objectiveLine(o, groupName)"),
            "objectiveLine must take the enclosing phase header, to suppress a redundant suffix"
        );
        assert!(
            line.contains("o.source !== groupName"),
            "the suffix must be suppressed when the phase header already names the source"
        );
        assert!(
            line.contains("esc(o.source)"),
            "the source name is server-supplied text and must go through esc()"
        );

        let render_op = js_between(UI, "function renderOp(", "\n  function renderSquad(");
        assert!(
            render_op.contains("objectiveLine(o, g.name)"),
            "renderOp must pass the group name explicitly — `.map(objectiveLine)` silently passes \
             the array index instead and defeats the suppression"
        );
        assert!(
            !render_op.contains(".map(objectiveLine)"),
            "point-free `.map(objectiveLine)` passes the array index as groupName"
        );
    }

    /// THE trap of the settings panel: `saveFromForm()` does not patch `cfg`, it REBUILDS it from
    /// scratch out of the DOM. Every persisted key without a form control therefore has to be copied
    /// across by hand. `positions` and `sizes` are both written by dragging, never by a control — so
    /// if `sizes` is left out of that object literal, every widget size the user has set is wiped the
    /// next time they tick any checkbox in Settings. Silent, immediate, and destroys user data.
    /// (Same failure shape as the fixed-struct config that dropped unknown component keys, PR #29.)
    #[test]
    fn widget_sizes_and_positions_survive_a_settings_save() {
        const UI: &str = include_str!("../ui/index.html");
        let body = js_between(
            UI,
            "async function saveFromForm()",
            "applyConfig(cfg, true)",
        );
        for key in ["positions", "sizes"] {
            assert!(
                body.contains(&format!("{key}: (cfg && cfg.{key})")),
                "saveFromForm() rebuilds cfg from the DOM, so it must carry `{key}` over explicitly \
                 (`{key}: (cfg && cfg.{key}) || {{}}`) — otherwise every settings save silently \
                 discards it; got: {body}"
            );
        }
        // Reset layout has to clear both halves of a widget's box, or widgets snap back to the
        // default columns while keeping whatever size the user stretched them to.
        let reset = UI
            .lines()
            .find(|l| l.contains("el('b-reset').onclick"))
            .expect("ui/index.html must wire the Reset layout button");
        for key in ["cfg.positions = {}", "cfg.sizes = {}"] {
            assert!(
                reset.contains(key),
                "Reset layout must clear `{key}`; got: {reset}"
            );
        }
        // …and it must WAIT for the save. persist() is an async IPC round-trip, so an unawaited
        // save races location.reload() tearing the page down: the reset silently does nothing.
        assert!(
            reset.contains("await persist()"),
            "Reset layout must await persist() before reloading; got: {reset}"
        );
        // Restoring a widget's box is one operation: applyPos is the single call site that every
        // layout pass already goes through, so it must restore the size too or a saved size only
        // reappears on whichever paths happen to call applySize directly.
        // A commented-out call still *contains* the call text, so require a real statement: a line
        // inside applyPos whose first non-space characters are the call itself.
        assert!(
            js_between(UI, "function applyPos(", "\n  }")
                .lines()
                .any(|l| l.trim_start().starts_with("applySize(node, wid)")),
            "applyPos() must also applySize(), as a live statement — it is the one call site every \
             layout pass already goes through, so without it a saved size only reappears on \
             whichever paths happen to call applySize directly"
        );
    }

    /// A fullscreen always-on-top game overlay must not reach the network at launch, and must not
    /// depend on a font CDN resolving before its own text is legible. The stylesheet used to
    /// `@import` Google Fonts and the CSP allowlisted both font hosts.
    ///
    /// But deleting that import is only half a fix: **Geist is not installed on a normal Windows
    /// machine**, so the token stacks fell straight through to Segoe UI and the whole overlay
    /// silently changed typeface for every user. The two variable faces are therefore bundled next
    /// to this UI (SIL OFL 1.1 — redistribution permitted, licence shipped alongside). This test
    /// pins BOTH halves: no remote fetch, and the local files that replace it actually exist.
    #[test]
    fn overlay_ui_bundles_its_fonts_and_fetches_none() {
        const UI: &str = include_str!("../ui/index.html");
        const CONF: &str = include_str!("../tauri.conf.json");
        for host in ["fonts.googleapis.com", "fonts.gstatic.com"] {
            assert!(
                !UI.contains(host),
                "ui/index.html must not reference {host} — the overlay would fetch a font at launch"
            );
            assert!(
                !CONF.contains(host),
                "tauri.conf.json must not allowlist {host} in its CSP once the import is gone"
            );
        }
        for open in [
            "@import url('http",
            "@import url(\"http",
            "@import url(http",
        ] {
            assert!(
                !UI.contains(open),
                "ui/index.html must not @import a remote stylesheet ({open}…)"
            );
        }
        assert!(
            CONF.contains("font-src 'self'"),
            "the CSP must keep font loading same-origin only — bundled faces need nothing more"
        );
        // The bundled faces: declared under the family each token asks for FIRST, pointing at a file
        // that is really there. A missing file is silent — the browser just falls through the stack.
        let fonts_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/fonts");
        for (token, family, file) in [
            ("--font:", "Geist Variable", "Geist-Variable.woff2"),
            ("--mono:", "Geist Mono Variable", "GeistMono-Variable.woff2"),
        ] {
            assert!(
                UI.contains(&format!("font-family:'{family}';")),
                "ui/index.html must declare a local @font-face for '{family}'"
            );
            assert!(
                UI.contains(&format!("url('fonts/{file}') format('woff2')")),
                "the '{family}' @font-face must load the bundled ui/fonts/{file}"
            );
            let path = fonts_dir.join(file);
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("ui/fonts/{file} must ship with the UI: {e}"));
            assert!(
                bytes.starts_with(b"wOF2"),
                "ui/fonts/{file} must be a real woff2 (magic `wOF2`), not a stub or an LFS pointer"
            );
            let decl = UI
                .lines()
                .find(|l| l.trim_start().starts_with(token))
                .unwrap_or_else(|| panic!("ui/index.html must declare a {token} token"));
            assert!(
                decl.contains(&format!("'{family}'")),
                "{token} must ask for the bundled '{family}' first; got: {decl}"
            );
            // …and the local stack behind it must survive, so a face that fails to decode still
            // lands on a real system font rather than the default serif.
            assert!(
                decl.contains("ui-sans-serif") || decl.contains("ui-monospace"),
                "{token} must keep a locally-available fallback stack; got: {decl}"
            );
        }
        assert!(
            fonts_dir.join("LICENSE.txt").is_file(),
            "the SIL OFL licence must ship alongside the fonts we redistribute"
        );
    }

    /// A stray `*/` in the stylesheet is SILENT and disproportionately destructive. It closes
    /// nothing; the prose before it is parsed as a selector prelude, swallows the NEXT rule's block,
    /// and that rule is dropped entirely. This bit while writing this very branch: a note appended
    /// after a comment's `*/` left `.rsz{display:none;position:absolute;z-index:2}` unparsed, so
    /// every resize handle silently became a static, in-flow `<div>` — arrange mode looked fine and
    /// resizing was dead. Nothing else in this suite noticed, because every guard asserts on the
    /// rule's TEXT, which was still right there. Only a browser measurement caught it.
    #[test]
    fn the_overlay_stylesheet_has_balanced_comments() {
        const UI: &str = include_str!("../ui/index.html");
        let style = js_between(UI, "<style>", "</style>");
        let b = style.as_bytes();
        let (mut i, mut in_comment, mut opened) = (0usize, false, 0usize);
        while i + 1 < b.len() {
            let two = &b[i..i + 2];
            if in_comment {
                if two == b"*/" {
                    in_comment = false;
                    i += 2;
                    continue;
                }
            } else if two == b"/*" {
                in_comment = true;
                opened += 1;
                i += 2;
                continue;
            } else if two == b"*/" {
                let ctx: String = style[i.saturating_sub(90)..i].chars().collect();
                panic!(
                    "stray `*/` in the <style> block — the comment it tries to close was already \
                     closed, so everything before it is being parsed as CSS and the next rule is \
                     silently dropped. Context:\n…{ctx}"
                );
            }
            i += 1;
        }
        assert!(
            !in_comment,
            "unterminated `/*` in the <style> block — the rest of the stylesheet is a comment"
        );
        assert!(
            opened > 5,
            "expected the stylesheet's explanatory comments to still be present; found {opened}"
        );
    }

    /// B5 — the default columns must be laid out in PAINTED pixels. A widget's per-component size
    /// slider is applied as CSS `zoom`, which multiplies the used value of its own left/top/width/
    /// height. Advancing the column cursor by a raw `offsetHeight` while `style.top` paints at
    /// `top × zoom` collides the moment two widgets differ in scale: measured, `op` at 1.5 buried
    /// `squad` by 110.8px and overran the Field Guide column. Nothing in the JS can catch this, and
    /// it only shows on a machine where someone moved a slider.
    #[test]
    fn default_columns_are_laid_out_in_painted_pixels() {
        const UI: &str = include_str!("../ui/index.html");
        let body = js_between(UI, "function stackColumn(", "\n  }");
        assert!(
            body.contains("node.offsetHeight * z"),
            "stackColumn must advance by the PAINTED height (offsetHeight × zoom); got: {body}"
        );
        assert!(
            body.contains("node.offsetWidth * z"),
            "stackColumn must measure the PAINTED width so the next column can clear it"
        );
        assert_eq!(
            body.matches("/ z) + 'px'").count(),
            2,
            "both of the widget's own coordinates (left AND top) must be divided back out of \
             painted space; got: {body}"
        );
        assert!(
            !UI.contains("node.offsetHeight + COL_GAP"),
            "the unzoomed column advance is the bug itself — it must not survive anywhere"
        );
        // Both layout passes must go through the one helper, or they drift apart again.
        for pass in [
            "colRight.rest = stackColumn(",
            "colRight.push = stackColumn(",
        ] {
            assert!(
                UI.contains(pass),
                "both default-column passes must use stackColumn — missing `{pass}`"
            );
        }
    }

    /// B6 — a widget with an explicit height must scroll its INNER body, never itself. The `.rsz`
    /// handles are absolutely-positioned children, so a scrolling widget scrolls its own resize
    /// handles out of view (measured: at scrollTop 200 the south handle sat 87px above the top of
    /// the widget's viewport) and can never be resized back. Same hazard CLAUDE.md records for the
    /// panel: scroll containers trap positioned elements.
    #[test]
    fn a_sized_widget_scrolls_its_body_not_its_own_box() {
        const UI: &str = include_str!("../ui/index.html");
        assert!(
            js_between(UI, "function sectionWidget(", "\n  }").contains("<div class=\"w-body\">"),
            "every rendered widget must wrap its content in `.w-body` — that wrapper is the only \
             thing that can take the scroll instead of the widget box"
        );
        let sh = js_between(UI, "function setHeightBox(", "\n  }");
        assert!(
            sh.contains("node.style.overflow = 'hidden'"),
            "the widget box must CLIP, never scroll; got: {sh}"
        );
        assert!(
            sh.contains("sc.style.overflowY = 'auto'"),
            "the inner body must be the scroller; got: {sh}"
        );
        assert!(
            !sh.contains("node.style.overflowY = 'auto'"),
            "putting the scroll back on the widget re-breaks resizing entirely; got: {sh}"
        );
        assert!(
            !sh.contains("node.style.display"),
            "the flex-column must come from CSS, not an inline style: an inline `display` on \
             #settings beats `#settings{{display:none}}` and pins the settings panel open"
        );
        let dense: String = UI.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            dense.contains(".widget[data-sized=\"wh\"]{display:flex;flex-direction:column;}"),
            "a vertically-sized widget needs the flex column for its body to absorb the height"
        );
        assert!(
            dense.contains("#sect-fieldguide>.w-body{max-height:72vh;overflow-y:auto;}"),
            "the Field Guide's height cap must sit on its BODY — on the widget it made the guide a \
             permanent scroll container, so its handles scrolled away as soon as it had content"
        );
        assert!(
            !dense.contains("#sect-fieldguide{width:300px;max-height:72vh"),
            "the old widget-level cap must be gone, not merely duplicated onto the body"
        );
    }

    /// B7 + F6 — a gesture must commit only when the pointer really moved, and must let go of its
    /// listeners when the gesture is cancelled. Without the movement test, a stray click on a 6px
    /// edge handle converts a widget from auto-height to a frozen height plus a scrolling body,
    /// permanently, with no UI to undo it short of Reset layout.
    #[test]
    fn gestures_commit_only_on_movement_and_clean_up_when_cancelled() {
        const UI: &str = include_str!("../ui/index.html");
        for (event, count) in [("addEventListener", 3), ("removeEventListener", 3)] {
            assert_eq!(
                UI.matches(&format!("{event}('pointercancel', up)")).count(),
                count,
                "all THREE gestures (drag, edge-resize AND corner-scale) must {event} for \
                 pointercancel — a cancelled gesture otherwise leaks its move/up listeners onto the \
                 widget forever"
            );
        }
        let rz = js_between(UI, "function startResize(", "\n  }");
        assert!(
            rz.contains("if (dx || dy) moved = true;"),
            "the resize gesture must record whether the pointer actually moved; got: {rz}"
        );
        assert!(
            rz.contains("if (!moved){ applySize(node, wid); return; }"),
            "a resize that never moved must restore the stored box and commit NOTHING; got: {rz}"
        );
        // The corner-scale gesture is the third one, and it has MORE to undo on a stray click: the
        // anchor maths rewrites left/top on every pointermove, so a no-op click that skipped the
        // guard would commit a scale AND a position.
        let sc = js_between(UI, "function startScale(", "\n  }");
        assert!(
            sc.contains("if (dx || dy) moved = true;"),
            "the scale gesture must record whether the pointer actually moved; got: {sc}"
        );
        assert!(
            sc.contains(
                "if (!moved){ node.style.zoom = z0; node.style.left = l0 + 'px'; \
                 node.style.top = t0 + 'px'; return; }"
            ),
            "a corner drag that never moved must restore the widget's zoom AND the position the \
             anchor maths rewrote, then commit nothing; got: {sc}"
        );
        assert!(
            js_between(UI, "function makeDraggable(", "\n  }").contains("if (!moved) return;"),
            "a plain click must not re-save (and re-persist) the widget's position"
        );
        // F3: reinterpreting the stylesheet's CONTENT width as a border width without restating it
        // snapped the widget 328→300 on pointerdown, before any movement.
        assert!(
            rz.contains("node.style.boxSizing = 'border-box'; node.style.width = w0 + 'px';"),
            "border-box and the explicit width must land in the same statement; got: {rz}"
        );
    }

    /// F1 — `components` is an OPEN map on the Rust side precisely so a widget added later
    /// round-trips without a struct change (PR #29). Rebuilding it purely from the `COMPS` constant
    /// reintroduces that bug from the other direction: any key this build has never heard of is
    /// dropped the first time the user ticks a checkbox.
    ///
    /// F2 — and if `get_config` ever fails, `init` must still populate the form. The form starts at
    /// its markup defaults (every checkbox unchecked, the scale slider empty) and `saveFromForm()`
    /// rebuilds the config from exactly those controls, so the next settings change would write
    /// `show:false` for all 17 widgets and blank the entire HUD.
    #[test]
    fn a_settings_save_can_never_blank_the_hud() {
        const UI: &str = include_str!("../ui/index.html");
        let save = js_between(
            UI,
            "async function saveFromForm()",
            "applyConfig(cfg, true)",
        );
        assert!(
            save.contains("Object.assign({}, (cfg && cfg.components) || {})"),
            "saveFromForm() must SEED components from the saved map before overwriting the keys it \
             knows, or unknown keys are silently dropped on every save; got: {save}"
        );
        assert!(
            save.contains("Number.isFinite(scale)"),
            "an unpopulated scale slider reads '' → NaN → a null scale in the saved config"
        );
        let init = js_between(UI, "(async function init()", "connect();");
        assert_eq!(
            init.matches("populateForm(").count(),
            2,
            "BOTH init branches must populate the form — the failure branch especially; got: {init}"
        );
        assert!(
            init.contains("defaultCfg()"),
            "a failed get_config must fall back to a COMPLETE default config, not a bare stub"
        );
        // F7: a rejected save must be visible, or a lost settings change looks identical to a saved one.
        let persist = js_between(UI, "const persist = ", ";\n");
        assert!(
            persist.contains("console.error"),
            "persist() must surface a failed set_config instead of swallowing it; got: {persist}"
        );
    }

    /// The now-playing progress bar was filled with `var(--v, #4fc3f7)`. `--v` is not a custom
    /// property anywhere in the file — `.v` is a CSS *class* — so the fallback always won and the
    /// bar stayed a fixed light blue under all four themes, ignoring an org's runtime accent.
    #[test]
    fn now_playing_progress_bar_uses_the_theme_accent() {
        const UI: &str = include_str!("../ui/index.html");
        let fill = UI
            .lines()
            .find(|l| l.contains("id=\"np-bar-fill\""))
            .expect("ui/index.html must render the now-playing progress fill");
        assert!(
            fill.contains("var(--accent)"),
            "the progress fill must use the theme accent token; got: {fill}"
        );
        assert!(
            !UI.contains("var(--v,") && !UI.contains("var(--v)"),
            "`--v` is not defined anywhere in this stylesheet — a var(--v…) reference can only ever \
             resolve to its hardcoded fallback, which is exactly the theme-blind bug this pins"
        );
    }

    /// A widget's scale has TWO editors — the Widgets-tab size slider and a corner drag — writing the
    /// SAME value (`components[key].scale`). If their ranges ever differ, the feature silently undoes
    /// itself: `saveFromForm()` rebuilds `components` out of the slider DOM, and a range input clamps
    /// anything outside its own min/max, so a drag that wrote past the slider's max would survive
    /// right until the user next touched any setting and then snap back with no warning. The only way
    /// to keep them equal is to derive both from one declaration — which is exactly what this pins.
    #[test]
    fn the_size_slider_and_the_corner_drag_share_one_scale_range() {
        const UI: &str = include_str!("../ui/index.html");
        let decl = UI
            .lines()
            .find(|l| l.trim_start().starts_with("const SCALE_MIN ="))
            .expect("ui/index.html must declare the canonical `const SCALE_MIN = …` range");
        // Parse the three constants out of `const SCALE_MIN = 0.7, SCALE_MAX = 1.5, SCALE_STEP = 0.05;`
        let num = |name: &str| -> f64 {
            let rest = decl
                .split_once(&format!("{name} = "))
                .unwrap_or_else(|| {
                    panic!("`{name}` must be declared on the same line; got: {decl}")
                })
                .1;
            rest.trim_start()
                .split([',', ';', ' '])
                .next()
                .and_then(|n| n.parse::<f64>().ok())
                .unwrap_or_else(|| panic!("`{name}` must be a literal number; got: {decl}"))
        };
        let (min, max, step) = (num("SCALE_MIN"), num("SCALE_MAX"), num("SCALE_STEP"));
        assert!(
            min > 0.0 && min <= 1.0 && max >= 1.0 && min < max,
            "the scale range must be positive and bracket 1.0 (the default every widget starts at), \
             else a fresh widget is out of range on first render; got {min}..{max}"
        );
        let steps = (max - min) / step;
        assert!(
            step > 0.0 && (steps - steps.round()).abs() < 1e-6,
            "SCALE_STEP must divide the range exactly — the drag snaps to it on release, so a step \
             the slider cannot land on puts the two editors in different value spaces; got \
             {min}..{max} step {step}"
        );
        // The slider's markup must be GENERATED from the constants, not re-typed beside them.
        let row = js_between(UI, "function compRow(", "\n  }");
        for (attr, name) in [
            ("min", "SCALE_MIN"),
            ("max", "SCALE_MAX"),
            ("step", "SCALE_STEP"),
        ] {
            assert!(
                row.contains(&format!("{attr}=\"${{{name}}}\"")),
                "the per-component size slider must take its {attr} from ${{{name}}} rather than a \
                 literal — a literal is what lets the two ranges drift apart; got: {row}"
            );
        }
        // …and the drag must clamp to the very same two.
        let sc = js_between(UI, "function startScale(", "\n  }");
        assert_eq!(
            sc.matches("SCALE_MIN, SCALE_MAX").count(),
            2,
            "the corner drag must clamp to the canonical range both live and when it snaps on \
             release; got: {sc}"
        );
        // Two-way sync without a feedback loop: the drag writes the slider's `.value` directly.
        // Dispatching an event there would re-enter saveFromForm() and loop the editors together.
        let set = js_between(UI, "function setCompScale(", "\n  }");
        assert!(
            set.contains("[data-scale=\"${key}\"]") && set.contains("slider.value = scale"),
            "setCompScale must push the new value into the open settings panel's slider, or a save \
             while the panel is open writes the STALE slider value back over the drag; got: {set}"
        );
        assert!(
            !set.contains("dispatchEvent"),
            "syncing the slider must not dispatch an event — that re-enters saveFromForm() and \
             loops the two editors into each other; got: {set}"
        );
    }

    /// The two handle families do DIFFERENT things and must not bleed into each other: corners scale
    /// (the widget and its whole subtree, via `zoom`), edges resize the box (and reflow). They are
    /// independent axes that compose, so the scale gesture must never write a width/height and the
    /// box gesture must never write a zoom — otherwise a widget with both a stored size and a scale
    /// has one silently overwrite the other.
    #[test]
    fn corners_scale_the_widget_and_edges_resize_its_box() {
        const UI: &str = include_str!("../ui/index.html");
        let handles = js_between(UI, "function ensureResizeHandles(", "\n  }");
        assert!(
            handles.contains("d.length === 2 ? scaleKeyOf(wid) : null"),
            "a two-letter direction is a corner — the handle builder must route corners to the scale \
             gesture and edges to the box gesture; got: {handles}"
        );
        assert!(
            handles.contains("if (key) startScale(node, wid, key, d, ev); else startResize("),
            "a corner on a widget with no component row (#control/#settings has no scale to write) \
             must fall back to a box resize rather than silently doing nothing; got: {handles}"
        );
        let sc = js_between(UI, "function startScale(", "\n  }");
        for forbidden in [
            "node.style.width",
            "node.style.height",
            "setHeightBox(",
            "cfg.sizes",
        ] {
            assert!(
                !sc.contains(forbidden),
                "the scale gesture must not touch the widget's BOX (`{forbidden}`) — the stored size \
                 stays in the widget's own unzoomed space so the two axes compose; got: {sc}"
            );
        }
        assert!(
            sc.contains("setCompScale(key, zf)"),
            "the scale gesture must write the same per-component scale the slider drives, so the \
             widget's CONTENTS shrink with it — resizing the box alone only crops them; got: {sc}"
        );
        let rz = js_between(UI, "function startResize(", "\n  }");
        assert!(
            !rz.contains("style.zoom"),
            "the edge gesture must not touch `zoom` — it resizes the box only; got: {rz}"
        );
        // Trap 4: this gesture changes the zoom WHILE it runs, so its reference zoom has to be taken
        // once at pointerdown. Re-reading it inside `move` feeds the zoom just written back into the
        // next delta — the widget accelerates away from the cursor and oscillates at the clamp.
        assert_eq!(
            sc.matches("zoomOf(").count(),
            1,
            "the scale gesture must read the widget's zoom EXACTLY once, at pointerdown; got: {sc}"
        );
        assert!(
            sc.contains("const z0 = zoomOf(node);")
                && sc.contains("const r = node.getBoundingClientRect();"),
            "…and take its painted reference box in the same place; got: {sc}"
        );
        // A scale changes the widget's PAINTED height, which is what the column pass advances by — so
        // committing one has to re-run the same layout a slider change does. applyConfig is that path
        // and it must keep driving BOTH render passes: renderData() owns the REST widgets,
        // renderContext() the push-driven ones, and dropping either leaves half the HUD stale.
        assert!(
            sc.contains("applyConfig(cfg, true)"),
            "committing a scale must re-run the layout through applyConfig, exactly as a settings \
             save does — the painted height stackColumn advances by just changed; got: {sc}"
        );
        // …and it must NOT pin a widget that is still column-managed. The anchor rewrites the
        // widget's own origin, so saving it is tempting — but stackColumn skips a positioned widget
        // WITHOUT advancing its cursor, so the next widget in that column lands on top of it
        // (measured: shrinking a column's first widget buried it under the second by 182–257px).
        // Scaling must not take a widget out of its column; only moving it does.
        assert!(
            sc.contains("if (cfg.positions && cfg.positions[wid]) savePos(node, wid);"),
            "a scale commit may only save the position of an ALREADY user-placed widget — pinning a \
             column-managed one makes the next widget in that column overlap it; got: {sc}"
        );
        let apply = js_between(UI, "function applyConfig(", "\n  }");
        assert!(
            apply.contains("renderData();") && apply.contains("renderContext(lastCtx)"),
            "applyConfig must drive BOTH render passes (PR #29): renderData() for the REST widgets \
             and renderContext() for the push-driven ones; got: {apply}"
        );
        // The Field Guide is built once and updated in place, so it never returns through
        // sectionWidget's `node.style.zoom = compScale(key)`. Without a re-assert on its own layout
        // pass, its size slider moved nothing until the next restart — and a corner drag on it would
        // then disagree with the slider, which is the whole thing this pair of controls must not do.
        assert!(
            js_between(UI, "function placeFieldGuide(", "\n  }")
                .contains("n.style.zoom = compScale('fieldguide');"),
            "placeFieldGuide must re-assert the guide's scale — it is the only pass that runs for a \
             widget which is never re-rendered"
        );
    }

    /// Resize handles sit INSIDE the widget they resize, which puts them on top of two live wires:
    /// the widget's own drag listener (a grab on an edge must resize or move, never both), and the
    /// hit-rect report (they are arrange-mode-only, where click-through is already dropped globally,
    /// so reporting them would make the overlay swallow clicks over widget edges in display mode).
    #[test]
    fn resize_handles_are_arrange_mode_only_and_never_start_a_drag() {
        const UI: &str = include_str!("../ui/index.html");
        let ignore = UI
            .lines()
            .find(|l| l.contains("e.target.closest("))
            .expect("makeDraggable must still filter pointerdown targets");
        assert!(
            ignore.contains(".rsz"),
            "makeDraggable must ignore pointerdown on a resize handle, or dragging an edge both \
             resizes and moves the widget; got: {ignore}"
        );
        let sel = UI
            .lines()
            .find(|l| l.contains("const HIT_SELECTOR"))
            .expect("ui/index.html must declare `const HIT_SELECTOR`");
        assert!(
            !sel.contains(".rsz"),
            "resize handles must stay OUT of the hit-rect report — they only work in arrange mode; \
             got: {sel}"
        );
        // They are <div>s precisely so the tag-based HIT_SELECTOR cannot pick them up by accident.
        assert!(
            js_between(UI, "function ensureResizeHandles(", "\n  }")
                .contains("createElement('div')"),
            "resize handles must be <div>s — HIT_SELECTOR reports button/input/select/textarea/a"
        );
        let dense: String = UI.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            dense.contains(".rsz{display:none;"),
            "resize handles must be display:none at rest, so they cannot be grabbed outside arrange mode"
        );
        assert!(
            dense.contains("body.interactive.rsz{display:block"),
            "resize handles must be revealed by body.interactive — the same gate dragging uses"
        );
    }

    /// Adding `sizes` must not invalidate a config file written before it existed: `#[serde(default)]`
    /// has to fill it in, or an upgrade fails the load and silently resets every preference. And the
    /// map must stay open (arbitrary widget ids round-trip), the same contract `components` has.
    #[test]
    fn config_written_before_sizes_existed_still_loads() {
        let old = r#"{"server_url":"http://localhost:8080","start_on_login":false,"theme":"pyro",
            "corner":"bottom-left","scale":1.0,"auto_hide":true,
            "components":{"nowplaying":{"show":false,"scale":1.2}},
            "positions":{"card":[10.0,20.0]}}"#;
        let c: HudConfig = serde_json::from_str(old).expect("a pre-sizes config must still load");
        assert!(c.sizes.is_empty(), "missing `sizes` must default to empty");
        assert_eq!(
            c.theme, "pyro",
            "existing preferences must survive the upgrade"
        );
        assert_eq!(c.positions.get("card"), Some(&[10.0, 20.0]));
        assert_eq!(c.components.get("nowplaying").map(|x| x.show), Some(false));

        // A widget id Rust has never heard of must survive a full round trip, and h == 0 (the
        // "height is automatic" sentinel) must not be normalised away.
        let mut c2 = c.clone();
        c2.sizes.insert("sect-op".into(), [420.0, 0.0]);
        c2.sizes.insert("sect-fieldguide".into(), [300.0, 540.0]);
        let back: HudConfig = serde_json::from_str(&serde_json::to_string(&c2).unwrap()).unwrap();
        assert_eq!(back.sizes.get("sect-op"), Some(&[420.0, 0.0]));
        assert_eq!(back.sizes.get("sect-fieldguide"), Some(&[300.0, 540.0]));
    }

    #[test]
    fn member_ws_url_swaps_scheme_and_path() {
        assert_eq!(
            member_ws_url("http://localhost:8080").unwrap(),
            "ws://localhost:8080/ws/member"
        );
        assert_eq!(
            member_ws_url("https://org.example.com").unwrap(),
            "wss://org.example.com/ws/member"
        );
        assert!(member_ws_url("not a url").is_err());
    }
}

// Packaging/release-shape guards (version agreement, bundle targets, updater-key state). Test-only,
// so it adds no item to the binary — which matters in a bin crate, where `--all-targets` strips the
// cfg(test) unit and anything reachable only from a test is dead code.
//
// 🔴 THIS DECLARATION MUST STAY BELOW `mod tests`, at the very bottom of the file. Two source-scan
// guards above (`overlay_visibility_never_routes_through_tao`, and the UI/JS slicer used by
// `load_and_save_both_sanitize_the_binding_set`) carve out "the non-test half" of this file with
// `SRC.split("#[cfg(test)]").next()` — i.e. everything before the FIRST such attribute. Putting this
// module's `#[cfg(test)]` near the top truncates that region to a handful of lines, and both guards
// then fail with a message that points at the overlay code rather than at the declaration that moved.
// (Observed, not theorised: that is exactly what happened when this line sat next to `mod
// rich_presence`.)
#[cfg(test)]
mod release_guards;
