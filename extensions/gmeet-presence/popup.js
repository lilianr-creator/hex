const DEFAULTS = { endpoint: '', token: '', meetName: '', enabled: false };
const endpoint = document.getElementById('endpoint');
const token = document.getElementById('token');
const meetName = document.getElementById('meetName');
const manual = document.getElementById('manual');
const enabled = document.getElementById('enabled');
const status = document.getElementById('status');
const save = document.getElementById('save');
const pairButton = document.getElementById('pair');
const test = document.getElementById('test');
const probe = document.getElementById('probe');
const probeOut = document.getElementById('probeOut');

const FRESH_MS = 30_000;
const LOCAL_ENDPOINT = /^http:\/\/127\.0\.0\.1:\d{1,5}\/?$/;

function describe(bridge, meet) {
  const lines = [];
  if (bridge) {
    if (bridge.connected) {
      const parts = ['Hex: connected'];
      parts.push(bridge.recording ? 'recording' : 'not recording');
      if (Number(bridge.extensions) > 1) parts.push(`${bridge.extensions} extensions`);
      lines.push(`${parts.join(' · ')}.`);
    } else {
      lines.push(`Hex: ${bridge.message || 'not connected'}`);
    }
  }
  const fresh = meet && Date.now() - (meet.updatedAt || 0) < FRESH_MS;
  if (meet && meet.meeting && fresh) {
    const names = (meet.participants || []).join(', ') || 'no names read yet';
    const speaking = (meet.speaking || []).join(', ');
    lines.push(`Meet: ${names}${speaking ? ` · speaking: ${speaking}` : ''}`);
    if (meet.selfFound === false && (meet.participants || []).length > 1) {
      lines.push('Meet: your own tile is not marked — fill Your name in Meet.');
    }
  } else if (meet && meet.meeting === false && fresh) {
    lines.push('Meet: the extension is off (turn on Report speakers to Hex).');
  } else {
    lines.push('Meet: no Meet tab reporting (open or reload meet.google.com).');
  }
  return lines.join('\n');
}

function paint(stored) {
  // A check-in from before the switch was turned on is not worth showing.
  if (stored.enabled && stored.hexBridgeStatus?.message === 'The extension is off.') {
    status.textContent = 'Checking…';
    status.className = '';
    return;
  }
  status.textContent = describe(stored.hexBridgeStatus, stored.hexMeetStatus);
  status.className = stored.hexBridgeStatus?.ok ? 'ok' : '';
}

function fillFields(stored) {
  endpoint.value = stored.endpoint;
  token.value = stored.token;
  meetName.value = stored.meetName;
  enabled.checked = stored.enabled;
  // Paired by the native host: keep the manual fallback out of the way.
  manual.open = !stored.hexPairedAuto;
}

/// Settings and Hex's state live in local storage; the Meet status, which
/// names participants, only in session storage (memory).
function stored(callback) {
  chrome.storage.local.get(
    { ...DEFAULTS, hexPairedAuto: false, hexBridgeStatus: null },
    (local) => {
      chrome.storage.session.get({ hexMeetStatus: null }, (session) => {
        callback({ ...local, hexMeetStatus: session?.hexMeetStatus || null });
      });
    },
  );
}

function refresh(done) {
  stored((values) => {
    fillFields(values);
    paint(values);
    if (done) done(values);
  });
}

refresh();
// Refresh the check-in now, so opening the popup never shows a stale state.
chrome.runtime.sendMessage({ type: 'hex-heartbeat' }, () => void chrome.runtime.lastError);

pairButton.addEventListener('click', () => {
  // Pairing stores the connection; the toggle is the user's consent and must
  // not be lost to the storage refresh that follows.
  const wantsReporting = enabled.checked;
  status.textContent = 'Pairing…';
  status.className = '';
  chrome.runtime.sendMessage({ type: 'hex-pair' }, (response) => {
    if (chrome.runtime.lastError) {
      status.textContent = chrome.runtime.lastError.message;
      status.className = 'bad';
      return;
    }
    if (response?.ok) {
      manual.open = false;
      chrome.storage.local.set({ enabled: wantsReporting }, () => {
        refresh((stored) => {
          status.textContent = stored.enabled
            ? 'Paired with Hex.'
            : 'Paired with Hex. Turn on Report speakers to Hex.';
          status.className = 'ok';
        });
      });
    } else {
      status.textContent = response?.error || 'The Hex native host is not installed.';
      status.className = 'bad';
    }
  });
});

// The switch applies at once: a popup close must not lose it.
enabled.addEventListener('change', () => {
  chrome.storage.local.set({ enabled: enabled.checked });
});

save.addEventListener('click', () => {
  const address = endpoint.value.trim();
  if (address && !LOCAL_ENDPOINT.test(address)) {
    status.textContent = 'The endpoint must be http://127.0.0.1:<port>.';
    status.className = 'bad';
    return;
  }
  chrome.storage.local.set(
    {
      endpoint: address,
      token: token.value.trim(),
      meetName: meetName.value.trim(),
      enabled: enabled.checked,
    },
    () => {
      status.textContent = 'Saved.';
      status.className = 'ok';
    },
  );
});

test.addEventListener('click', () => {
  status.textContent = 'Testing…';
  status.className = '';
  chrome.runtime.sendMessage({ type: 'hex-heartbeat' }, (response) => {
    if (chrome.runtime.lastError) {
      status.textContent = chrome.runtime.lastError.message;
      status.className = 'bad';
      return;
    }
    status.textContent = response?.message || 'No answer.';
    status.className = response?.ok ? 'ok' : 'bad';
  });
});

probe.addEventListener('click', () => {
  chrome.tabs.query({ active: true, currentWindow: true }, ([tab]) => {
    if (!tab) {
      status.textContent = 'No active tab.';
      status.className = 'bad';
      return;
    }
    chrome.tabs.sendMessage(tab.id, { type: 'hex-probe' }, async (snapshot) => {
      if (chrome.runtime.lastError || !snapshot) {
        status.textContent = 'No Meet tab answering. Open or reload meet.google.com.';
        status.className = 'bad';
        return;
      }
      const text = JSON.stringify(snapshot, null, 2);
      try {
        await navigator.clipboard.writeText(text);
        status.textContent = 'Probe copied. Attach it to a bug report about this Meet.';
        status.className = 'ok';
      } catch {
        probeOut.hidden = false;
        probeOut.value = text;
        probeOut.select();
        status.textContent = 'Press ⌘C to copy the probe.';
        status.className = '';
      }
    });
  });
});

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'session') {
    if (changes.hexMeetStatus) stored(paint);
    return;
  }
  if (area !== 'local') return;
  if (
    changes.endpoint ||
    changes.token ||
    changes.meetName ||
    changes.enabled ||
    changes.hexPairedAuto
  ) {
    chrome.storage.local.get(
      { ...DEFAULTS, hexPairedAuto: false },
      (stored) => fillFields(stored),
    );
    return;
  }
  stored(paint);
});
