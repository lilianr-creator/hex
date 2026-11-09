# Hex Meet Participants (POC)

A minimal Chrome/Chromium extension that tells Hex who is speaking in Google
Meet. It reads Meet's own participant tiles (names and the speaking glow) and
posts short reports to Hex's local API while a meeting records.

It is **read-only** for the page and for Hex:

- no audio capture, no `tabCapture`, no `AudioWorklet`, no injected page script;
- no network access except `http://127.0.0.1:*`, and an endpoint that is not
  `http://127.0.0.1:<port>` is refused, so the token never leaves the Mac;
- nothing is sent anywhere else;
- on disk it stores only a local endpoint, a token, the on/off flag, your
  name in Meet if you set it, and Hex's last answer; the current call's
  participant names stay in memory (`chrome.storage.session`).

The native host and its installer are macOS only.

## Install (unpacked, for the POC)

1. Start Hex.
2. Install the native messaging host, so the extension pairs itself:

   ```sh
   hex/scripts/install-presence-host.sh --browser chrome
   ```

   It writes Chrome's host manifest and points it at the binary it finds
   (`/Applications/Hex.app/Contents/MacOS/hex`, else a `cargo build` in the
   repo).
   Pass `--binary PATH` when Hex lives elsewhere, or `--all` for Chrome,
   Chromium, Brave, and Edge.

3. **Restart Chrome** so it reads the host manifest.
4. Open `chrome://extensions`, turn on **Developer mode**, and **Load
   unpacked** this `gmeet-presence` folder.
5. Open the popup, tick **Report speakers to Hex**, and press **Pair with
   Hex**. It fills the endpoint and token from Hex (`Pair with Hex` needs
   Chrome restarted once after step 2). The popup then shows **Hex: connected
   · not recording** — even before a meeting — and switches to **recording**
   while Hex records. **Test connection** checks again.
6. Optional: fill **Your name in Hex** to label your microphone turns (for
   example `John`). It never decides who a remote speaker is, so two people
   who share a name are never merged.

If the native host is not installed, the endpoint and token fields remain a
manual fallback under **Manual setup** (collapsed once the native host has
paired): `hex meetings presence` prints both. The extension's ID is
fixed by `key` in `manifest.json`; removing and re-adding the unpacked
extension resets its stored settings, but the native host pairs it again.

Two Chrome profiles are fine: each extension install has its own client id and
Hex counts two connections. Only one Meet at a time is expected; the profile
that is not in a Meet only checks in. Reports carry the Meet code from the
URL, so an idle Meet tab in another profile cannot feed the recording one.

Meet does not always mark which tile is yours. The extension uses
`data-self-name` when the page has it; otherwise it finds the tile whose camera
preview is mirrored and whose track is this machine's camera (it has a
`groupId`, which a received track has not). A single tile is not assumed to
be yours: with Hide self view or the speaker layout it is the other person.
When nothing structural marks your tile, Meet's own `data-self-name` value
picks it only if exactly one tile has that name, so two participants with the
same name are never merged.

Meet's speaking glow dims between words. A tile must stay dark for 600 ms
before its speaking interval ends, so one sentence is one interval, and an
interval still open when the tab closes or reporting stops is sent. A batch
that cannot reach Hex is kept (up to 2,000 intervals) and sent with the next.

If none of that matches in a multi-person call, the popup says so; press
**Copy DOM probe** and the JSON of what the extension sees can be used to find
the structural marker for that Meet version (it holds participant names and
Meet's markup, but no camera or device ids). Until then your turns stay `You`
(never a guess).

The port is remembered between launches and the token is created once, so this
step is done only once per Hex install. If Hex had to take another port, the
extension asks the native host again when the old one stops answering (at
most every five minutes).

Meetings › Settings › Chrome extension shows the same state inside Hex:
**Connected** (with the number of extensions when several are connected) or
**Not connected**, in which case run `hex meetings presence` and paste the
setup into the extension.


## Toolbar icon

The icon says what the extension is doing without opening the popup:

| Icon | Meaning |
|---|---|
| Slate hexagon | Off, or no Meet tab reporting |
| Blue hexagon, `•` | On a Meet page, not in a call yet |
| Green hexagon, participant count | In a call, reporting that many people |
| Red `REC` | Hex is recording this Meet (never on another Meet, or while Hex is idle) |

## Use

Join a Google Meet. Hex records as usual. During the call the popup shows the
names it reads; after **Stop & Transcribe**, the transcript and the Markdown
export label turns with those names, and the notes can assign next steps to
people.

Without the extension, or on a call where it is off, the transcript keeps
`You` / `Them`.

## Limit

Meet's speaking glow uses obfuscated CSS class names that change over time.
When they change, the extension reads names but no longer sees who is speaking,
so turns fall back to `Them` instead of guessing. Update `SPEAKING_CLASSES` in
`speakers.js` with the class currently on the active speaker's tile (inspect
the tile in DevTools while someone talks).

Two people talking at once may be reported as `Alice + Bob`. One Meet tab at a
time is assumed.

## Manual checks

The extension has no automated tests. After a change, reload it (the version in
`chrome://extensions` says which build runs) and check against a live Meet:

- a two-person call: the other person's turns carry their name, yours `You` or
  your name;
- Hide self view on, then the speaker layout: your turns are never named after
  the other person;
- two participants with the same display name: both stay remote speakers;
- a second Chrome profile with an idle Meet tab while the first records: the
  recording's participants do not change, and the idle tab never shows `REC`;
- quit Hex during a call, start it again: the popup reconnects by itself.
