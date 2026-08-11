// StarPlatform HUD — Stream Deck plugin.
//
// ZERO dependencies and NO build step, on purpose. The obvious route is `@elgato/streamdeck` plus
// TypeScript, npm and rollup, which would drop a whole second toolchain into an all-Rust workspace
// for what is, underneath, a JSON-over-WebSocket protocol. Node 22+ ships a global `WebSocket`, and
// the manifest pins Node 24, so this file is the entire plugin. Nothing to install, nothing to
// compile, and no lockfile to keep in step with the Rust side.
//
// TWO sockets, and they are not alike:
//
//   Stream Deck  <-- ws -->  THIS  <-- ws -->  StarPlatform HUD local API  --> member socket --> server
//
// The left one is opened FOR us: the Stream Deck app launches this process with -port/-pluginUUID/
// -registerEvent/-info and we connect back and register. The right one is ours to open, and it is
// the only way to reach the HUD.
//
// 🔴 This plugin holds no org credential. The pairing key authorises reaching the HUD and nothing
// else; every action is relayed over the HUD's own member socket and re-checked server-side. A
// plugin that authenticated directly to the server would be a second credential sitting in a
// user-readable Node process.

'use strict';

// ---------------------------------------------------------------------------
// Argument parsing (Stream Deck passes these; order is not guaranteed).
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length - 1; i++) {
    const flag = argv[i];
    if (flag && flag.startsWith('-')) out[flag.slice(1)] = argv[i + 1];
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
const SD_PORT = args.port;
const SD_UUID = args.pluginUUID;
const SD_REGISTER = args.registerEvent;

const LAUNCHED_BY_STREAM_DECK = Boolean(SD_PORT && SD_UUID && SD_REGISTER);

const SUBPROTOCOL = 'starplatformhud.v1';
const DEFAULT_HUD_PORT = 48291;

// ---------------------------------------------------------------------------
// Action UUID -> local API action. Curated on both sides: the HUD exposes a fixed action set, and
// this maps the deck's buttons onto it. A generic "send any ClientMessage" button is deliberately
// not offered.
// ---------------------------------------------------------------------------

/** Actions the server acknowledges only by side effect: it discards the handler's own result
 *  (`let _ = apply_*`) and does not re-push context for them, so an `Ack` means "queued", never
 *  "accepted". A tick on these would assert a success nobody verified. */
const UNCONFIRMED = new Set([
  'com.thecodesaiyan.starplatformhud.bed.pause',
  'com.thecodesaiyan.starplatformhud.transport.next',
  'com.thecodesaiyan.starplatformhud.transport.prev',
  'com.thecodesaiyan.starplatformhud.clip',
  'com.thecodesaiyan.starplatformhud.volume',
  'com.thecodesaiyan.starplatformhud.quickaction',
  'com.thecodesaiyan.starplatformhud.memberclip',
]);

const ACTIONS = {
  'com.thecodesaiyan.starplatformhud.checkin': () => ({ action: 'check_in' }),
  'com.thecodesaiyan.starplatformhud.ready': () => ({ action: 'toggle_ready' }),
  'com.thecodesaiyan.starplatformhud.objective.next': () => ({ action: 'objective_next' }),
  'com.thecodesaiyan.starplatformhud.objective.prev': () => ({ action: 'objective_prev' }),
  'com.thecodesaiyan.starplatformhud.transport.next': () => ({ action: 'transport_next' }),
  'com.thecodesaiyan.starplatformhud.transport.prev': () => ({ action: 'transport_prev' }),
};

/** Why a key is unavailable, or `null` when it is fine.
 *
 * A reason, not a boolean: the previous version threw the distinction away and every disabled key
 * read "Lead only" — so the LEAD of an op with no audible bed saw their transport keys blame them
 * for not being the lead, which is maximally confusing to the one person who is.
 */
function keyReason(uuid, state) {
  if (!state || !state.connected) return 'HUD\noffline';
  const isCheckIn = uuid === 'com.thecodesaiyan.starplatformhud.checkin';
  if (!state.live_op) return isCheckIn && state.checkin ? null : 'No op';
  if (isCheckIn) return 'On op';           // already checked in: nothing left to do
  // Member-level: any member ON the op, lead or not. This is what makes a deck worth owning if
  // you are not leading — every other action below this line is lead-only.
  if (uuid === 'com.thecodesaiyan.starplatformhud.ready') return null;
  if (uuid === 'com.thecodesaiyan.starplatformhud.quickaction') {
    return (state.quick_actions || []).length ? null : 'No actions';
  }
  if (uuid === 'com.thecodesaiyan.starplatformhud.memberclip') {
    return (state.member_clips || []).length ? null : 'Clips off';
  }
  if (!state.is_lead) return 'Lead only';
  if (uuid.startsWith('com.thecodesaiyan.starplatformhud.transport')
      || uuid === 'com.thecodesaiyan.starplatformhud.bed.pause'
      || uuid === 'com.thecodesaiyan.starplatformhud.volume') {
    return state.bed_active ? null : 'No bed';
  }
  if (uuid.startsWith('com.thecodesaiyan.starplatformhud.objective')) {
    const o = state.objective;
    if (!o || o[1] === 0) return 'No objectives';
    // At the ends of the rail the key would refuse; grey it rather than let it look live.
    if (uuid.endsWith('next') && o[0] >= o[1]) return 'At end';
    if (uuid.endsWith('prev') && o[0] === 0) return 'At start';
  }
  return null;
}

// ---------------------------------------------------------------------------
// Stream Deck socket
// ---------------------------------------------------------------------------

let sd = null;
/** Latest board pushed by the HUD; null until the first push. */
let board = null;
/** contextId -> action UUID, for every visible key. */
const visible = new Map();
/** contextId -> per-key settings (clip key, etc). */
const settings = new Map();
/** Global settings, where the pairing key and port live. */
let globals = {};
/** contextId -> timer, for the two-press confirm on Complete Op. */
const armed = new Map();

function sdSend(obj) {
  if (sd && sd.readyState === 1) sd.send(JSON.stringify(obj));
}

function setTitleFor(context, title) {
  sdSend({ event: 'setTitle', context, payload: { title, target: 0 } });
}

function setStateFor(context, state) {
  sdSend({ event: 'setState', context, payload: { state } });
}

/** Repaint every visible key from the current board.
 *
 * One title per key, decided once. An earlier version set an informative title and then overwrote it
 * with a placeholder in a trailing `if (!on)`, so the useful text never survived to the key.
 *
 * An empty title restores whatever the manifest declared, which is how an available key gets its
 * default face back without this file having to know what that face is.
 */
function repaint() {
  for (const [context, uuid] of visible) {
    const reason = keyReason(uuid, board);

    // Two-state keys carry their state regardless of availability, so the face stays truthful.
    if (uuid === 'com.thecodesaiyan.starplatformhud.ready') {
      setStateFor(context, board && board.my_ready ? 1 : 0);
    } else if (uuid === 'com.thecodesaiyan.starplatformhud.bed.pause') {
      setStateFor(context, board && board.bed_paused ? 1 : 0);
    }

    if (uuid === 'com.thecodesaiyan.starplatformhud.volume') {
      const np = (board && board.now_playing) || (reason || '—');
      const vol = board && typeof board.bed_volume === 'number' ? board.bed_volume + '%' : '';
      sdSend({ event: 'setFeedback', context, payload: { title: vol ? `Bed ${vol}` : 'Bed', value: np } });
      continue;
    }

    setTitleFor(context, titleFor(uuid, reason, board));
  }
}

/** The one title a key should show right now. `''` means "use the manifest's own title". */
function titleFor(uuid, reason, state) {
  if (reason) return reason;
  if (uuid === 'com.thecodesaiyan.starplatformhud.objective.next' || uuid === 'com.thecodesaiyan.starplatformhud.objective.prev') {
    const o = state.objective;
    const arrow = uuid.endsWith('next') ? 'Obj +' : 'Obj -';
    return o ? `${arrow}\n${o[0]}/${o[1]}` : arrow;
  }
  return '';
}

function connectStreamDeck() {
  sd = new WebSocket(`ws://127.0.0.1:${SD_PORT}`);
  sd.addEventListener('open', () => {
    sdSend({ event: SD_REGISTER, uuid: SD_UUID });
    sdSend({ event: 'getGlobalSettings', context: SD_UUID });
  });
  sd.addEventListener('message', (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    onStreamDeckEvent(msg);
  });
  // The Stream Deck app owns this process's lifetime; if its socket closes, the app is going away.
  sd.addEventListener('close', () => process.exit(0));
}

function onStreamDeckEvent(msg) {
  switch (msg.event) {
    case 'didReceiveGlobalSettings':
      globals = (msg.payload && msg.payload.settings) || {};
      connectHud();
      break;
    case 'willAppear':
      visible.set(msg.context, msg.action);
      settings.set(msg.context, (msg.payload && msg.payload.settings) || {});
      repaint();
      break;
    case 'willDisappear':
      visible.delete(msg.context);
      settings.delete(msg.context);
      break;
    case 'didReceiveSettings':
      settings.set(msg.context, (msg.payload && msg.payload.settings) || {});
      break;
    case 'keyDown':
      onKeyDown(msg);
      break;
    case 'dialRotate':
      onDialRotate(msg);
      break;
    case 'dialDown':
      // Toggle, mirroring the pause key. Always sending `bed_pause` makes a push on an
      // already-paused bed a no-op that still flashes OK.
      send({ action: board && board.bed_paused ? 'bed_resume' : 'bed_pause' }, msg.context);
      break;
    case 'sendToPlugin':
      // The property inspector asks for the clip list so its picker is populated from what the
      // member may actually fire, rather than a hand-typed key.
      sdSend({
        event: 'sendToPropertyInspector',
        context: msg.context,
        payload: {
          clips: (board && board.clips) || [],
          quickActions: (board && board.quick_actions) || [],
          memberClips: (board && board.member_clips) || [],
          connected: !!(board && board.connected),
        },
      });
      break;
    default:
      break;
  }
}

function onKeyDown(msg) {
  const uuid = msg.action;
  const context = msg.context;

  if (uuid === 'com.thecodesaiyan.starplatformhud.complete') {
    // 🔴 Two presses, five seconds. The SDK has no confirmation primitive, and ending an op is
    // irreversible — a single stray press on a deck must not do it. The HUD enforces this too
    // (`confirm: true` is required there), so a buggy or hand-rolled plugin cannot skip it.
    if (armed.has(context)) {
      clearTimeout(armed.get(context));
      armed.delete(context);
      setStateFor(context, 0);
      send({ action: 'complete_op', confirm: true }, context);
    } else {
      setStateFor(context, 1);
      armed.set(context, setTimeout(() => { armed.delete(context); setStateFor(context, 0); }, 5000));
    }
    return;
  }

  if (uuid === 'com.thecodesaiyan.starplatformhud.quickaction') {
    const key = (settings.get(context) || {}).quick;
    if (!key) { sdSend({ event: 'showAlert', context }); return; }
    send({ action: 'quick_action', key }, context);
    return;
  }

  if (uuid === 'com.thecodesaiyan.starplatformhud.memberclip') {
    const key = (settings.get(context) || {}).mclip;
    if (!key) { sdSend({ event: 'showAlert', context }); return; }
    send({ action: 'member_clip', key }, context);
    return;
  }

  if (uuid === 'com.thecodesaiyan.starplatformhud.clip') {
    const key = (settings.get(context) || {}).clip;
    if (!key) { sdSend({ event: 'showAlert', context }); return; }
    send({ action: 'trigger_clip', key }, context);
    return;
  }

  if (uuid === 'com.thecodesaiyan.starplatformhud.bed.pause') {
    send({ action: board && board.bed_paused ? 'bed_resume' : 'bed_pause' }, context);
    return;
  }

  const build = ACTIONS[uuid];
  if (!build) { sdSend({ event: 'showAlert', context }); return; }
  send(build(), context);
}

function onDialRotate(msg) {
  if (keyReason('com.thecodesaiyan.starplatformhud.volume', board)) { sdSend({ event: 'showAlert', context: msg.context }); return; }
  const ticks = (msg.payload && msg.payload.ticks) || 0;
  // Start from the board's REAL volume rather than a guess, so the first turn nudges from where the
  // bed actually is instead of jumping to wherever a hardcoded default happened to sit. `pending`
  // holds our optimistic value between turns, because the board only catches up on the next push.
  const base = pendingVolume !== null ? pendingVolume : (board.bed_volume || 0);
  pendingVolume = Math.max(0, Math.min(100, base + ticks * 5));
  sdSend({ event: 'setFeedback', context: msg.context, payload: { title: 'Bed', value: pendingVolume + '%' } });
  send({ action: 'bed_volume', percent: pendingVolume }, msg.context);
}

// ---------------------------------------------------------------------------
// HUD socket
// ---------------------------------------------------------------------------

let hud = null;
let hudRetry = 0;
/** requestId -> contextId, so an outcome lands on the key that asked for it. */
const pending = new Map();
let nextRequestId = 0;
/** Optimistic dial volume between turns; null = follow the board. */
let pendingVolume = null;

function connectHud() {
  const token = (globals.pairingKey || '').trim();
  const port = Number(globals.hudPort) || DEFAULT_HUD_PORT;
  if (!token) {
    // Unpaired: every key reads as offline rather than silently doing nothing.
    board = null;
    repaint();
    return;
  }
  // Close any previous socket FIRST. The inspector writes global settings on every keystroke-ish
  // change, each of which is broadcast back as `didReceiveGlobalSettings` — so without this, editing
  // the key leaks a live socket per edit whose listeners still write the shared `board`, and whose
  // eventual close schedules yet another retry.
  if (hud) {
    try { hud.onclose = null; hud.close(); } catch (e) { /* already gone */ }
    hud = null;
  }
  try {
    // The token rides as a SUBPROTOCOL because the WHATWG WebSocket API cannot set request headers,
    // and a query parameter would put the secret into anything that logs request lines.
    hud = new WebSocket(`ws://127.0.0.1:${port}`, [SUBPROTOCOL, token]);
  } catch {
    scheduleHudRetry();
    return;
  }
  hud.addEventListener('open', () => { hudRetry = 0; });
  hud.addEventListener('message', (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    if (msg.type === 'state') {
      board = msg;
      // The board is authoritative again once the HUD has told us where the volume landed.
      pendingVolume = null;
      repaint();
      return;
    }
    // An id-less nack is a PARSE failure on the HUD side (it could not read an id to echo), which
    // is exactly the plugin/HUD action-set skew the design promises will produce a reason. Falling
    // through silently here made it produce nothing at all — worse than the single-context guess
    // this correlation replaced. Surface it on every key still waiting.
    let ctx = msg.id !== undefined ? pending.get(msg.id) : undefined;
    if (msg.id !== undefined) pending.delete(msg.id);
    if (!ctx) {
      if (msg.type === 'nack') {
        for (const c of pending.values()) sdSend({ event: 'showAlert', context: c });
        pending.clear();
      }
      return;
    }
    if (msg.type === 'ack') {
      // 🔴 `Ack` means the intent reached the member socket, NOT that the server accepted it: the
      // server's audio handlers discard their own bool and deliberately do not re-push context. So
      // for those a tick would assert a success nobody verified — show nothing and let the next
      // board push be the confirmation. Actions that DO move visible state keep the tick.
      if (!UNCONFIRMED.has(visible.get(ctx))) sdSend({ event: 'showOk', context: ctx });
    } else if (msg.type === 'nack') {
      // Roll back the optimistic dial value; otherwise the touch strip keeps showing a level the
      // server rejected and later turns compound from it.
      pendingVolume = null;
      // A refusal WITH a reason: `not_lead`, `no_bed`, `needs_confirm`, … Showing it is what turns
      // "the key did nothing" into "the key told me why".
      sdSend({ event: 'showAlert', context: ctx });
    }
  });
  hud.addEventListener('close', () => { board = null; repaint(); scheduleHudRetry(); });
  hud.addEventListener('error', () => { /* close follows; retry there */ });
}

function scheduleHudRetry() {
  // Capped backoff: the HUD may simply not be running, and hammering a closed port forever is rude.
  hudRetry = Math.min(hudRetry + 1, 6);
  setTimeout(connectHud, 1000 * hudRetry);
}

/** Send an action and remember which key to report the outcome on.
 *
 * The id matters: without it we could only assume the next reply belongs to the last thing we sent,
 * so two keys pressed in quick succession would swap outcomes — one flashing OK for a refusal that
 * was really about the other. We also do NOT flash OK here: a tick shown at send time means "it
 * left the building", which is a lie the moment the HUD refuses it.
 */
function send(payload, context) {
  if (!hud || hud.readyState !== 1) {
    if (context) sdSend({ event: 'showAlert', context });
    return;
  }
  const id = ++nextRequestId;
  if (context) {
    pending.set(id, context);
    // A reply that never comes (HUD wedged, killed, socket dropped mid-flight) is a FAILURE, not a
    // non-event. Dropping the entry quietly would leave the key showing nothing at all.
    setTimeout(() => {
      if (pending.delete(id)) sdSend({ event: 'showAlert', context });
    }, 10000);
  }
  hud.send(JSON.stringify(Object.assign({ id }, payload)));
}

// Only dial out when Stream Deck actually launched us. Guarding this (rather than exiting) is what
// lets the drift guard load this file to inspect it.
if (LAUNCHED_BY_STREAM_DECK) connectStreamDeck();

// Exported for the unit tests; ignored by Stream Deck, which only ever runs this file.
if (typeof module !== 'undefined') {
  module.exports = { parseArgs, keyReason, ACTIONS, SUBPROTOCOL };
}
