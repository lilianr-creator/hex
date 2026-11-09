// Reads Google Meet's own participant tiles and reports who is speaking to
// Hex. Read-only: it never touches audio, never injects into the page's
// JavaScript world, and never talks to anything but the extension's own
// background worker.
(() => {
  const POLL_MS = 250;
  // Meet's glow dims between words: a tile must stay dark this long before
  // its speaking interval closes, so one sentence is one interval.
  const RELEASE_MS = 600;
  const FLUSH_MS = 2_000;
  // Unsent intervals kept while the worker or Hex is unreachable.
  const MAX_PENDING = 2_000;
  const STATUS_MS = 1_000;
  // A status older than this in the popup means the tab is gone.
  const STATUS_HEARTBEAT_MS = 15_000;

  // Meet's speaking glow has used these class names. Obfuscated names change;
  // an unknown one only costs names, never wrong ones, so nothing is learned
  // automatically.
  const SPEAKING_CLASSES = [
    'Oaajhc', 'HX2H7', 'wEsLMd', 'OgVli',
    'speaking', 'active-speaker', 'speaker-active', 'speaking-indicator',
  ];
  const JUNK_NAME = /^Google Participant \(|spaces\/|devices\//i;
  const JUNK_PHRASES = [
    'let participants', 'send messages', 'turn on captions', 'join now',
    'switch camera', 'present now', 'chat with everyone',
  ];

  let enabled = false;
  let meetName = '';
  let selfName = null;
  let selfFound = false;
  let participants = [];
  let speaking = new Set();
  let open = new Map();
  let lastLit = new Map();
  let pending = [];
  let rosterDirty = true;
  let lastFlush = 0;
  let lastStatus = 0;
  let lastStatusAt = 0;
  let lastStatusKey = '';

  const text = (node) => (node?.textContent || '').trim();

  /// `abc-defg-hij` from the Meet URL, so Hex can tell this tab from another
  /// Chrome profile's Meet.
  function meetCode() {
    const match = location.pathname.match(/(?:^|\/)([a-z]{3}-[a-z]{4}-[a-z]{3})(?:\/|$)/i);
    return match ? match[1].toLowerCase() : null;
  }

  function tileName(tile) {
    let name = text(tile.querySelector('span.notranslate'));
    if (!name) {
      const marker = tile.querySelector('[data-self-name]');
      name = (marker?.getAttribute('data-self-name') || '').trim();
    }
    if (!name || name.length < 2 || name.length > 50) return null;
    if (JUNK_NAME.test(name)) return null;
    const lower = name.toLowerCase();
    if (JUNK_PHRASES.some((phrase) => lower.includes(phrase))) return null;
    return name;
  }

  function selfId() {
    const marker = document.querySelector('[data-self-name]');
    const tile = marker?.closest('[data-participant-id]');
    return tile?.getAttribute('data-participant-id') || null;
  }

  function selfNames() {
    return [...document.querySelectorAll('[data-self-name]')]
      .map((node) => (node.getAttribute('data-self-name') || '').trim())
      .filter(Boolean);
  }

  function normalized(name) {
    return (name || '').trim().toLowerCase();
  }

  function isSelf(tile, id, self) {
    return tile.hasAttribute('data-self-name')
      || Boolean(tile.querySelector('[data-self-name]'))
      || (self !== null && id === self);
  }

  // The local preview is mirrored and its track is this machine's camera,
  // with a groupId and a device label; a received track has no groupId.
  // Two of those signals mark the self tile even on Meet versions that
  // render no data-self-name marker, and no name is ever compared: two
  // participants may share one.
  function mediaIsLocal(tile) {
    const video = tile.querySelector('video');
    if (!video) return false;
    let votes = 0;
    const transform = getComputedStyle(video).transform;
    const mirrored = /^matrix\((-?\d+(?:\.\d+)?),/.exec(transform);
    if (mirrored && Number(mirrored[1]) < 0) votes += 2;
    const stream = video.srcObject;
    const tracks = stream && typeof stream.getVideoTracks === 'function'
      ? stream.getVideoTracks()
      : [];
    const track = tracks[0];
    if (track) {
      const settings = track.getSettings ? track.getSettings() : {};
      if (settings.groupId) votes += 1;
      const label = String(track.label || '').toLowerCase();
      if (label && !label.includes('remote')) votes += 1;
    }
    return votes >= 2;
  }

  function isSpeaking(tile) {
    for (const name of SPEAKING_CLASSES) {
      if (tile.classList.contains(name) || tile.querySelector(`.${CSS.escape(name)}`)) {
        return true;
      }
    }
    return false;
  }

  function scan() {
    const self = selfId();
    const markers = selfNames();
    const seen = new Set();
    const tiles = [];
    for (const node of document.querySelectorAll('[data-participant-id]')) {
      const id = node.getAttribute('data-participant-id') || '';
      if (!id || seen.has(id)) continue;
      seen.add(id);
      const name = tileName(node);
      tiles.push({
        id,
        name,
        self: isSelf(node, id, self) || mediaIsLocal(node),
        speaking: isSpeaking(node),
      });
    }
    // A single tile is not assumed to be yours: with "Hide self view" or
    // the speaker layout, it is the other person.
    // Meet sometimes keeps the marker away from the tile. Its name decides
    // only when nothing structural did and exactly one tile carries it, so
    // two people who share a name are never merged.
    if (!tiles.some((tile) => tile.self)) {
      for (const name of markers) {
        const value = normalized(name);
        const matches = tiles.filter((tile) => tile.name && normalized(tile.name) === value);
        if (matches.length === 1) matches[0].self = true;
      }
    }
    // `meetName` only renames the self tile once it is found structurally:
    // matching people by name would merge two participants who share one.
    if (meetName) {
      const mine = tiles.find((tile) => tile.self);
      if (mine) mine.name = meetName;
    }
    return tiles;
  }

  function poll() {
    if (!enabled) return;
    const now = Date.now();
    const tiles = scan();

    const self = tiles.find((tile) => tile.self && tile.name);
    selfFound = Boolean(self);
    const nextSelf = meetName || self?.name || null;
    if (nextSelf !== selfName) {
      selfName = nextSelf;
      rosterDirty = true;
    }

    const named = tiles.map((tile) => tile.name).filter(Boolean);
    if (named.join('\u0000') !== participants.join('\u0000')) {
      participants = named;
      rosterDirty = true;
    }

    const lit = new Set(
      tiles.filter((tile) => !tile.self && tile.speaking && tile.name).map((tile) => tile.name),
    );
    for (const name of lit) {
      lastLit.set(name, now);
      if (!speaking.has(name)) {
        speaking.add(name);
        open.set(name, now);
      }
    }
    for (const name of [...speaking]) {
      if (!lit.has(name) && now - (lastLit.get(name) || 0) >= RELEASE_MS) {
        close(name, now);
      }
    }

    if (now - lastFlush >= FLUSH_MS && (pending.length > 0 || rosterDirty)) {
      flush();
    }
    reportStatus(now);
  }

  /// Ends `name`'s interval at the last poll that saw it lit.
  function close(name, now) {
    speaking.delete(name);
    const startMs = open.get(name);
    const endMs = Math.min(now, (lastLit.get(name) || now) + POLL_MS);
    open.delete(name);
    lastLit.delete(name);
    if (startMs !== undefined && endMs > startMs) {
      pending.push({ name, startMs, endMs });
    }
  }

  /// Ends every open interval, before the tab goes or reporting stops.
  function closeAll() {
    const now = Date.now();
    for (const name of [...speaking]) close(name, now);
  }

  /// Puts a batch that never reached Hex back, keeping the newest.
  function requeue(events) {
    pending = [...events, ...pending].slice(-MAX_PENDING);
    rosterDirty = true;
  }

  function flush() {
    if (pending.length === 0 && !rosterDirty) return;
    const payload = {
      code: meetCode() || undefined,
      selfName: selfName || undefined,
      participants: [],
      events: pending,
    };
    // Every name this call has seen, including whoever spoke first.
    const names = new Set(participants);
    for (const event of pending) names.add(event.name);
    payload.participants = [...names];
    const sent = pending;
    pending = [];
    rosterDirty = false;
    lastFlush = Date.now();
    try {
      chrome.runtime.sendMessage({ type: 'hex-presence', payload }, (response) => {
        // The worker was asleep or Hex unreachable: the next batch retries.
        if (chrome.runtime.lastError || response?.status === 0) requeue(sent);
      });
    } catch {
      // The extension was reloaded under the page; this tab cannot report.
    }
  }

  function reportStatus(now) {
    if (now - lastStatus < STATUS_MS) return;
    lastStatus = now;
    const key = `${participants.length}:${speaking.size}:${selfName || ''}:${selfFound ? 1 : 0}`;
    const heartbeat = now - lastStatusAt >= STATUS_HEARTBEAT_MS;
    if (key === lastStatusKey && !heartbeat) return;
    lastStatusKey = key;
    lastStatusAt = now;
    try {
      // Session storage stays in memory: participant names never reach disk.
      chrome.storage.session.set({
        hexMeetStatus: {
          meeting: true,
          participants,
          speaking: [...speaking],
          selfName,
          selfFound,
          code: meetCode(),
          updatedAt: now,
        },
      });
    } catch {
      // Storage is unavailable until the extension is reloaded.
    }
  }

  function reset() {
    speaking = new Set();
    open = new Map();
    lastLit = new Map();
    pending = [];
    participants = [];
    selfName = null;
    selfFound = false;
    rosterDirty = true;
  }

  chrome.storage.local.get({ enabled: false, meetName: '' }, (config) => {
    enabled = Boolean(config.enabled);
    meetName = (config.meetName || '').trim();
    if (!enabled) offStatus();
  });

  function offStatus() {
    try {
      chrome.storage.session.set({ hexMeetStatus: { meeting: false, updatedAt: Date.now() } });
    } catch {
      // Storage is unavailable until the extension is reloaded.
    }
  }

  chrome.storage.onChanged.addListener((changes, area) => {
    if (area !== 'local') return;
    if (changes.meetName) {
      meetName = (changes.meetName.newValue || '').trim();
      rosterDirty = true;
    }
    if (!changes.enabled) return;
    enabled = Boolean(changes.enabled.newValue);
    if (!enabled) {
      closeAll();
      flush();
      reset();
      offStatus();
    }
  });

  function probeSettings(track) {
    const settings = track.getSettings ? track.getSettings() : {};
    return {
      width: settings.width,
      height: settings.height,
      frameRate: settings.frameRate,
      hasDeviceId: Boolean(settings.deviceId),
      hasGroupId: Boolean(settings.groupId),
    };
  }

  function mediaInfo(element) {
    const stream = element.srcObject;
    const tracks = stream && typeof stream.getTracks === 'function'
      ? [...stream.getTracks()].map((track) => ({
          kind: track.kind,
          // A device name can carry its owner's name.
          labelled: Boolean(track.label),
          remoteLabel: String(track.label || '').toLowerCase().includes('remote'),
          enabled: track.enabled,
          muted: track.muted,
          readyState: track.readyState,
          // Device ids would identify this camera: say only whether they exist.
          settings: probeSettings(track),
        }))
      : [];
    return {
      tag: element.tagName.toLowerCase(),
      muted: element.muted,
      paused: element.paused,
      transform: getComputedStyle(element).transform,
      tracks,
    };
  }

  /// What the extension sees, for finding Meet's structural self marker when
  /// `data-self-name` is absent.
  function probe() {
    const tiles = [...document.querySelectorAll('[data-participant-id]')].map((tile) => ({
      id: (tile.getAttribute('data-participant-id') || '').slice(0, 24),
      name: tileName(tile),
      speaking: isSpeaking(tile),
      attrs: [...tile.attributes].map((attr) => `${attr.name}=${(attr.value || '').slice(0, 80)}`),
      classes: String(tile.className).slice(0, 160),
      descendants: [...new Set([...tile.querySelectorAll('[class]')].flatMap((el) => [...el.classList]))]
        .filter(Boolean)
        .slice(0, 120),
      aria: tile.getAttribute('aria-label'),
      media: [...tile.querySelectorAll('video, audio')].map(mediaInfo),
    }));
    const markers = [...document.querySelectorAll('[data-self-name]')].map((element) => ({
      tag: element.tagName.toLowerCase(),
      value: (element.getAttribute('data-self-name') || '').slice(0, 80),
      inTile: Boolean(element.closest('[data-participant-id]')),
    }));
    return { page: location.href.split('?')[0], tiles, markers };
  }

  chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
    if (message?.type === 'hex-probe') {
      sendResponse(probe());
      return undefined;
    }
    if (message?.type === 'hex-status') {
      sendResponse({
        meeting: true,
        enabled,
        participants,
        speaking: [...speaking],
        selfName,
      });
    }
  });

  window.addEventListener('pagehide', () => {
    if (!enabled) return;
    closeAll();
    flush();
  });

  setInterval(poll, POLL_MS);
})();
