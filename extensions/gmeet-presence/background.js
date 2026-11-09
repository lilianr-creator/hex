// Forwards the content script's reports to the local Hex API, and checks in
// with Hex even when no meeting records, so the popup and Hex's settings show
// the pairing before it matters. The native host hands over the endpoint and
// token automatically; the popup's fields remain as a manual fallback.

const DEFAULTS = { endpoint: '', token: '', enabled: false, hexClientId: '' };
const HOST = 'com.anomaly.hex.presence';
const HEARTBEAT_ALARM = 'hex-heartbeat';
const HEARTBEAT_MINUTES = 0.5;
/// A content script checks in every 15 s; older than this, the tab is gone.
const MEET_FRESH_MS = 45_000;
/// Hex listens on loopback only; any other endpoint never gets the token.
const LOCAL_ENDPOINT = /^http:\/\/127\.0\.0\.1:\d{1,5}\/?$/;
/// After a network failure, ask the native host for a new port at most this
/// often, so a stopped Hex does not start the host on every heartbeat.
const REPAIR_INTERVAL_MS = 5 * 60_000;
const ICONS = {
  off: {
    16: 'icons/hex-off-16.png',
    32: 'icons/hex-off-32.png',
  },
  on: {
    16: 'icons/hex-on-16.png',
    32: 'icons/hex-on-32.png',
  },
};
let lastStatus = '';
let lastRepairAt = 0;

// The content script writes the Meet status (participant names) to session
// storage, which stays in memory; it needs this to reach it.
chrome.storage.session.setAccessLevel?.({ accessLevel: 'TRUSTED_AND_UNTRUSTED_CONTEXTS' });

function isLocalEndpoint(endpoint) {
  return LOCAL_ENDPOINT.test(endpoint || '');
}

async function config() {
  return chrome.storage.local.get(DEFAULTS);
}

/// One opaque id per extension install: two Chrome profiles are two ids.
async function clientId() {
  const stored = await chrome.storage.local.get({ hexClientId: '' });
  if (stored.hexClientId) return stored.hexClientId;
  const id = crypto.randomUUID();
  await chrome.storage.local.set({ hexClientId: id });
  return id;
}

async function report(ok, status, message, extra) {
  const payload = { ok, status, message, connected: ok, ...extra, updatedAt: Date.now() };
  const key = JSON.stringify(payload, (field, value) =>
    field === 'updatedAt' ? undefined : value,
  );
  if (key === lastStatus) return;
  lastStatus = key;
  await chrome.storage.local.set({ hexBridgeStatus: payload });
}

/// The toolbar icon says at a glance whether the extension is reporting:
/// slate when off, blue on a Meet page, green with the participant count in a
/// call, red REC while Hex records this Meet.
async function refreshIcon() {
  const { enabled, hexBridgeStatus } = await chrome.storage.local.get({
    enabled: false,
    hexBridgeStatus: null,
  });
  const { hexMeetStatus } = await chrome.storage.session.get({ hexMeetStatus: null });
  const now = Date.now();
  const fresh = Boolean(
    hexMeetStatus?.meeting && now - (hexMeetStatus.updatedAt || 0) < MEET_FRESH_MS,
  );
  const participants = fresh ? (hexMeetStatus.participants || []).length : 0;
  const sameMeet =
    !hexBridgeStatus?.meetingCode ||
    !hexMeetStatus?.code ||
    hexBridgeStatus.meetingCode === hexMeetStatus.code;
  const recordingHere = Boolean(fresh && hexBridgeStatus?.recording && sameMeet);

  await chrome.action.setIcon({ path: enabled && fresh ? ICONS.on : ICONS.off });
  let badge = '';
  let color = '#2563EB';
  if (enabled && recordingHere) {
    badge = 'REC';
    color = '#DC2626';
  } else if (enabled && participants > 0) {
    badge = String(participants);
    color = '#16A34A';
  } else if (enabled && fresh) {
    badge = '•';
    color = '#2563EB';
  }
  await chrome.action.setBadgeBackgroundColor({ color });
  await chrome.action.setBadgeText({ text: badge });
  await chrome.action.setTitle({
    title: !enabled
      ? 'Hex Meet Participants — off'
      : recordingHere
        ? 'Hex Meet Participants — recording this meeting'
        : participants > 0
          ? `Hex Meet Participants — in a call (${participants})`
          : fresh
            ? 'Hex Meet Participants — on a Meet page'
            : 'Hex Meet Participants — no Meet tab',
  });
}

function presenceUrl(endpoint, client) {
  return `${endpoint.replace(/\/+$/, '')}/meeting/presence?client=${encodeURIComponent(client)}`;
}

/// Asks Hex's native host for the endpoint and token, and stores them.
async function pair() {
  const hello = await new Promise((resolve) => {
    chrome.runtime.sendNativeMessage(HOST, { type: 'hello' }, (response) => {
      if (chrome.runtime.lastError) {
        resolve({ ok: false, error: chrome.runtime.lastError.message });
        return;
      }
      resolve(response || { ok: false, error: 'No answer from the Hex host.' });
    });
  });
  if (hello?.ok && isLocalEndpoint(hello.endpoint) && hello.token) {
    await chrome.storage.local.set({
      endpoint: hello.endpoint,
      token: hello.token,
      hexPairedAuto: true,
    });
    return { ok: true, endpoint: hello.endpoint };
  }
  return { ok: false, error: hello?.error || 'The Hex native host is not installed.' };
}

async function configured() {
  let { endpoint, token, enabled } = await config();
  if (!enabled) {
    const message = 'The extension is off.';
    await report(false, 0, message, { connected: false, recording: false });
    return null;
  }
  if (!endpoint || !token) {
    await pair();
    ({ endpoint, token } = await config());
  }
  if (!endpoint || !token) {
    const message = 'Pair with Hex, or paste the endpoint and token.';
    await report(false, 0, message, { connected: false, recording: false });
    return null;
  }
  if (!isLocalEndpoint(endpoint)) {
    const message = 'The endpoint must be http://127.0.0.1:<port>. Pair with Hex again.';
    await report(false, 0, message, { connected: false, recording: false });
    return null;
  }
  return { endpoint, token };
}

/// One authenticated request to `/meeting/presence`. A refused token pairs
/// again (Hex has a new one); an unreachable endpoint pairs again at most
/// every few minutes, in case Hex moved to another port. `null` when Hex
/// cannot be reached.
async function authedFetch(init = {}) {
  let settings = await configured();
  if (!settings) return { settings: null, response: null };
  const client = await clientId();
  const attempt = async () => {
    try {
      return await fetch(presenceUrl(settings.endpoint, client), {
        ...init,
        headers: { ...(init.headers || {}), Authorization: `Bearer ${settings.token}` },
      });
    } catch {
      return null;
    }
  };
  let response = await attempt();
  const unreachable = !response && Date.now() - lastRepairAt >= REPAIR_INTERVAL_MS;
  if (response?.status === 401 || unreachable) {
    lastRepairAt = Date.now();
    const before = settings.endpoint;
    await pair();
    settings = await configured();
    if (settings && (response || settings.endpoint !== before)) response = await attempt();
  }
  return { settings, response };
}

/// `GET /meeting/presence`: Hex is reachable and the token is accepted, with
/// or without a meeting recording.
async function check() {
  const { settings, response } = await authedFetch();
  if (!settings) return { ok: false, status: 0, message: 'Not configured.' };
  if (!response) {
    const message = 'Hex is not reachable. Is it running?';
    await report(false, 0, message, { connected: false, recording: false });
    return { ok: false, status: 0, message };
  }
  if (!response.ok) {
    return failed(response);
  }
  const body = await response.json().catch(() => ({}));
  const recording = Boolean(body.recording);
  const extensions = Number(body.extensions) || 1;
  const message = recording
    ? 'Connected. Hex is recording.'
    : 'Connected. Hex is not recording.';
  await report(true, response.status, message, {
    connected: true,
    recording,
    extensions,
    meetingCode: body.meetingCode || null,
  });
  return { ok: true, status: response.status, message, connected: true, recording, extensions };
}

async function failed(response) {
  const message = response.status === 401
    ? 'Hex refused the token. Pair again, or copy the setup with `hex meetings presence`.'
    : `Hex answered ${response.status}.`;
  await report(false, response.status, message, { connected: false, recording: false });
  return { ok: false, status: response.status, message };
}

async function send(payload) {
  const { settings, response } = await authedFetch({
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(payload),
  });
  if (!settings) return { ok: false, status: 0, message: 'Not configured.' };
  if (!response) {
    const message = 'Hex is not reachable. Is it running?';
    await report(false, 0, message, { connected: false, recording: false });
    return { ok: false, status: 0, message };
  }
  if (response.ok) {
    const body = await response.json().catch(() => ({}));
    const message = 'Connected. Hex is recording this meeting.';
    await report(true, response.status, message, {
      connected: true,
      recording: true,
      meetingCode: payload.code || null,
      meetingId: body.meetingId || null,
    });
    return { ok: true, status: response.status, message };
  }
  if (response.status === 409) {
    // The token was accepted; Hex is idle or recording another Meet, and
    // either way not this one.
    const body = (await response.text()).trim();
    const otherMeeting = body.includes('presence-other-meeting');
    const message = otherMeeting
      ? 'Connected. Hex is recording another Meet.'
      : 'Connected. Hex is not recording yet.';
    await report(true, 409, message, { connected: true, recording: false, otherMeeting });
    return { ok: true, status: 409, message, otherMeeting };
  }
  return failed(response);
}

async function ensureAlarm() {
  const { enabled } = await config();
  if (!enabled) {
    await chrome.alarms.clear(HEARTBEAT_ALARM);
    return;
  }
  const existing = await chrome.alarms.get(HEARTBEAT_ALARM);
  if (!existing) {
    chrome.alarms.create(HEARTBEAT_ALARM, { periodInMinutes: HEARTBEAT_MINUTES });
  }
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message?.type === 'hex-presence') {
    send(message.payload).then(sendResponse, () => sendResponse({ ok: false }));
    return true;
  }
  if (message?.type === 'hex-pair') {
    pair().then(sendResponse, () => sendResponse({ ok: false }));
    return true;
  }
  if (message?.type === 'hex-test' || message?.type === 'hex-heartbeat') {
    check().then(sendResponse, () => sendResponse({ ok: false }));
    return true;
  }
  return undefined;
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === HEARTBEAT_ALARM) check().then(refreshIcon);
});

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'session') {
    if (changes.hexMeetStatus) refreshIcon();
    return;
  }
  if (area !== 'local') return;
  if (changes.enabled || changes.hexBridgeStatus) refreshIcon();
  if (!changes.enabled) return;
  if (changes.enabled.newValue) {
    ensureAlarm().then(check);
  } else {
    chrome.alarms.clear(HEARTBEAT_ALARM);
  }
});

chrome.runtime.onInstalled.addListener(() => {
  pair().then(ensureAlarm).then(check).then(refreshIcon);
});
chrome.runtime.onStartup.addListener(() => {
  pair().then(ensureAlarm).then(check).then(refreshIcon);
});
ensureAlarm();
refreshIcon();
