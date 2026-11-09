//! Participant names for a meeting, sent by the browser extension.
//!
//! The extension reads Google Meet's own DOM (who is lit as speaking) and
//! posts short, batched reports to the local API while a meeting records.
//! Nothing here hears individual audio: a report is a roster and a list of
//! `[start, end]` speaking intervals on the extension's wall clock. The
//! recorder writes them to `speakers.ndjson` beside the transcript and, when
//! the meeting is transcribed, attributes each sentence to the participant
//! whose intervals cover it. With no reports, the transcript keeps the
//! `You` / `Them` labels it always had.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::desktop::events::now_ms;

use super::{MeetingManifest, MeetingSource, TranscriptEntry};

/// The report file beside a meeting's transcript.
pub(super) const FILE: &str = "speakers.ndjson";
/// The panel that receives reports is capped well above a busy call's batch.
pub const MAX_INGEST_BYTES: usize = 256 * 1024;
/// A browser extension counts as connected while it checked in this recently
/// (the extension heartbeats every 30 s, and Chrome may clamp that to a minute).
const CONNECTED_WINDOW_MS: u64 = 90_000;
/// A client that stops for longer than this is forgotten.
const CLIENT_TTL_MS: u64 = 60 * 60 * 1_000;
/// Two Chrome profiles are two clients; this bounds a misbehaving one.
const MAX_CLIENTS: usize = 16;
/// A participant name longer than this is a misread tile, not a person.
const MAX_NAME_CHARS: usize = 80;
/// One report carries a few seconds of speech; this bounds a malformed one.
const MAX_REPORT_EVENTS: usize = 4_096;
/// A meeting keeps this many speaking intervals, about a full working day of
/// talk even when Meet's glow flickers between words.
const MAX_MEETING_EVENTS: usize = 50_000;
/// A call never shows more people than this; more is a malformed report.
const MAX_PARTICIPANTS: usize = 256;
/// Intervals of one name this close are one turn: the glow dims between words.
const MERGE_GAP_MS: u64 = 400;
/// How far back a new interval looks for its name's last one to extend.
const MERGE_LOOKBACK: usize = 64;
/// A name must cover this much of a sentence to own it outright.
const OWNER_COVERAGE: u64 = 60;
/// One name alone still owns the sentence at this much coverage, which is
/// well above a stray tile changing for a moment.
const SINGLE_COVERAGE: u64 = 25;
/// A name that covers less than this of a shared sentence is noise.
const SHARED_COVERAGE: u64 = 20;
/// Overlaps shorter than this are DOM sampling jitter.
const MIN_OVERLAP_MS: u64 = 120;
/// The largest number of names joined for one sentence.
const MAX_SHARED_NAMES: usize = 3;
/// The extension's token, stored once so it survives restarts.
const TOKEN_FILE: &str = "presence-token";

/// One participant's speaking interval, in epoch milliseconds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerEvent {
    pub name: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// What the extension has reported for one meeting.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    /// The local participant's display name, from Meet's own marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_name: Option<String>,
    /// Everyone the call showed, the local participant first.
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub events: Vec<SpeakerEvent>,
}

impl Presence {
    /// Adds a report to this one, keeping the first-seen order and skipping
    /// events `seen` already holds, so a long meeting does not rescan every
    /// earlier interval per report.
    /// An interval that starts within `MERGE_GAP_MS` of the end of its
    /// name's latest one extends it instead of adding another.
    fn absorb(&mut self, report: Presence, seen: &mut HashSet<(String, u64, u64)>) {
        if let Some(name) = report.self_name {
            self.self_name = Some(name);
        }
        for name in report.participants {
            if self.participants.len() < MAX_PARTICIPANTS && !self.participants.contains(&name) {
                self.participants.push(name);
            }
        }
        for event in report.events {
            let key = (event.name.clone(), event.start_ms, event.end_ms);
            if !seen.insert(key) {
                continue;
            }
            let latest = self
                .events
                .iter_mut()
                .rev()
                .take(MERGE_LOOKBACK)
                .find(|known| known.name == event.name);
            if let Some(latest) = latest
                && event.start_ms >= latest.start_ms
                && event.start_ms <= latest.end_ms.saturating_add(MERGE_GAP_MS)
            {
                latest.end_ms = latest.end_ms.max(event.end_ms);
                continue;
            }
            if self.events.len() >= MAX_MEETING_EVENTS {
                continue;
            }
            self.events.push(event);
            if self.events.len() == MAX_MEETING_EVENTS {
                tracing::warn!(
                    max = MAX_MEETING_EVENTS,
                    "meeting holds the most speaking intervals; later ones are ignored"
                );
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.self_name.is_none() && self.participants.is_empty() && self.events.is_empty()
    }
}

/// A report as the extension sends it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    #[serde(default)]
    self_name: Option<String>,
    #[serde(default)]
    participants: Vec<String>,
    #[serde(default)]
    events: Vec<SpeakerEvent>,
    /// The Meet code from the extension's URL, so a report from another
    /// Chrome profile's tab can be ignored.
    #[serde(default)]
    code: Option<String>,
}

impl Report {
    /// The cleaned report: names trimmed, shortened, and deduplicated, and
    /// only intervals that run forward.
    fn accept(self) -> Presence {
        let self_name = self.self_name.as_deref().and_then(clean_name);
        let mut presence = Presence {
            self_name: self_name.clone(),
            participants: Vec::new(),
            events: Vec::new(),
        };
        let mut names = HashSet::new();
        if let Some(name) = &self_name {
            names.insert(name.clone());
            presence.participants.push(name.clone());
        }
        for name in self
            .participants
            .iter()
            .take(MAX_PARTICIPANTS)
            .filter_map(|name| clean_name(name))
        {
            if names.insert(name.clone()) {
                presence.participants.push(name);
            }
        }
        let mut seen = HashSet::new();
        for event in self.events.into_iter().take(MAX_REPORT_EVENTS) {
            let Some(name) = clean_name(&event.name) else {
                continue;
            };
            if event.end_ms <= event.start_ms
                || !seen.insert((name.clone(), event.start_ms, event.end_ms))
            {
                continue;
            }
            if presence.participants.len() < MAX_PARTICIPANTS && names.insert(name.clone()) {
                presence.participants.push(name.clone());
            }
            presence.events.push(SpeakerEvent {
                name,
                start_ms: event.start_ms,
                end_ms: event.end_ms,
            });
        }
        presence
    }
}

/// A tile name as reported, or `None` when it is blank or implausibly long.
fn clean_name(name: &str) -> Option<String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then(|| name.chars().take(MAX_NAME_CHARS).collect())
}

/// The meeting currently recording, kept reachable by the local API.
struct Active {
    meeting_id: String,
    directory: PathBuf,
    presence: Presence,
    /// Events already stored, so a retried report adds nothing twice.
    seen: HashSet<(String, u64, u64)>,
    /// The Meet code this recording is for: the detected call's, else the
    /// first one a report with people in it names.
    code: Option<String>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

fn active() -> MutexGuard<'static, Option<Active>> {
    ACTIVE.lock().unwrap_or_else(|error| error.into_inner())
}

/// Extension installs that talked to this Hex, so two Chrome profiles show up
/// as two connections rather than one.
#[derive(Default)]
struct Bridge {
    /// Client id and its last authenticated request, epoch ms, oldest first.
    clients: Vec<(String, u64)>,
}

static BRIDGE: Mutex<Bridge> = Mutex::new(Bridge {
    clients: Vec::new(),
});

fn bridge() -> MutexGuard<'static, Bridge> {
    BRIDGE.lock().unwrap_or_else(|error| error.into_inner())
}

/// What Meetings › Chrome extension shows, and what a check-in answers.
#[derive(Clone, Debug, Default)]
pub struct BridgeStatus {
    /// Client ids that checked in within the connection window.
    pub clients: Vec<String>,
}

impl BridgeStatus {
    pub fn connected(&self) -> bool {
        !self.clients.is_empty()
    }
}

/// Records that an extension holding the presence token just called.
pub fn touch(client_id: Option<&str>) {
    let now = now_ms();
    let id = client_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("unknown")
        .chars()
        .take(64)
        .collect::<String>();
    let mut bridge = bridge();
    bridge
        .clients
        .retain(|(_, seen)| now.saturating_sub(*seen) < CLIENT_TTL_MS);
    if let Some((_, seen)) = bridge.clients.iter_mut().find(|(known, _)| *known == id) {
        *seen = now;
        return;
    }
    if bridge.clients.len() >= MAX_CLIENTS
        && let Some(oldest) = bridge
            .clients
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, seen))| *seen)
            .map(|(index, _)| index)
    {
        bridge.clients.remove(oldest);
    }
    bridge.clients.push((id, now));
}

/// The clients that checked in recently enough to count as connected.
pub fn bridge_status(now: u64) -> BridgeStatus {
    let mut bridge = bridge();
    bridge
        .clients
        .retain(|(_, seen)| now.saturating_sub(*seen) < CLIENT_TTL_MS);
    status_at(&bridge.clients, now)
}

fn status_at(clients: &[(String, u64)], now: u64) -> BridgeStatus {
    BridgeStatus {
        clients: clients
            .iter()
            .filter(|(_, seen)| now.saturating_sub(*seen) <= CONNECTED_WINDOW_MS)
            .map(|(id, _)| id.clone())
            .collect(),
    }
}

/// The answer to the extension's `GET /meeting/presence` check-in. Reaching
/// it at all means the token was accepted.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusReply {
    pub connected: bool,
    pub recording: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meeting_code: Option<String>,
    pub participants: usize,
    pub events: usize,
    pub extensions: usize,
}

pub fn status() -> StatusReply {
    let bridge = bridge_status(now_ms());
    let active = active();
    StatusReply {
        connected: true,
        recording: active.is_some(),
        meeting_id: active.as_ref().map(|active| active.meeting_id.clone()),
        meeting_code: active.as_ref().and_then(|active| active.code.clone()),
        participants: active
            .as_ref()
            .map_or(0, |active| active.presence.participants.len()),
        events: active
            .as_ref()
            .map_or(0, |active| active.presence.events.len()),
        extensions: bridge.clients.len(),
    }
}

/// Makes `directory` the destination for reports until dropped.
pub(super) struct Registration(String);

impl Drop for Registration {
    fn drop(&mut self) {
        let mut active = active();
        if active
            .as_ref()
            .is_some_and(|active| active.meeting_id == self.0)
        {
            *active = None;
        }
    }
}

pub(super) fn register(meeting_id: &str, directory: &Path, code: Option<String>) -> Registration {
    *active() = Some(Active {
        meeting_id: meeting_id.to_string(),
        directory: directory.to_path_buf(),
        presence: Presence::default(),
        seen: HashSet::new(),
        code,
    });
    Registration(meeting_id.to_string())
}

/// The Meet code, `abc-defg-hij`, in a Meet page address or a Meet tab's
/// own title, if there is one. Codes are lowercase, so a title word such as
/// `End-User-API` never reads as one.
pub(crate) fn meet_code(text: &str) -> Option<String> {
    text.split(|character: char| !(character.is_ascii_alphabetic() || character == '-'))
        .find(|token| is_meet_code(token))
        .map(str::to_owned)
}

fn is_meet_code(token: &str) -> bool {
    let parts = token.split('-').collect::<Vec<_>>();
    parts.len() == 3
        && [3, 4, 3].into_iter().zip(&parts).all(|(length, part)| {
            part.len() == length && part.bytes().all(|byte| byte.is_ascii_lowercase())
        })
}

/// Why a report could not be taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestError {
    /// No meeting records right now.
    NotRecording,
    /// The body is not a report this Hex understands.
    Invalid,
    /// The report comes from another Meet than the one recording, or names
    /// none while the recording knows its own.
    OtherMeeting,
    /// The report could not be saved beside the meeting.
    Storage,
}

/// What the extension gets back, so its popup can show the call is live.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestReply {
    pub meeting_id: String,
    pub participants: usize,
    pub events: usize,
}

/// Takes one report from the browser extension. The file write happens
/// outside the lock, so a slow disk never holds up a check-in or the end of
/// the recording.
pub fn ingest(body: &[u8]) -> Result<IngestReply, IngestError> {
    let report: Report = serde_json::from_slice(body).map_err(|_| IngestError::Invalid)?;
    let code = report
        .code
        .as_deref()
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .map(str::to_ascii_lowercase);
    let presence = report.accept();
    let line = serde_json::to_vec(&presence).map_err(|_| IngestError::Invalid)?;
    let (meeting_id, directory) = {
        let mut active = active();
        let active = active.as_mut().ok_or(IngestError::NotRecording)?;
        match (active.code.as_deref(), code.as_deref()) {
            (Some(expected), Some(reported)) if expected != reported => {
                return Err(IngestError::OtherMeeting);
            }
            (Some(_), None) => return Err(IngestError::OtherMeeting),
            // A recording started by hand or from a calendar event without
            // a Meet link belongs to the first Meet with people in it.
            (None, Some(reported)) if !presence.is_empty() => {
                active.code = Some(reported.to_owned());
            }
            _ => {}
        }
        (active.meeting_id.clone(), active.directory.clone())
    };
    append(&directory.join(FILE), &line).map_err(|error| {
        tracing::warn!(%error, "could not record participant names");
        IngestError::Storage
    })?;
    let mut active = active();
    let active = active
        .as_mut()
        .filter(|active| active.meeting_id == meeting_id)
        .ok_or(IngestError::NotRecording)?;
    active.presence.absorb(presence, &mut active.seen);
    Ok(IngestReply {
        meeting_id,
        participants: active.presence.participants.len(),
        events: active.presence.events.len(),
    })
}

fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(line)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

/// The reports for a meeting this process recorded, if any.
pub(super) fn take(meeting_id: &str) -> Option<Presence> {
    let mut active = active();
    let matches = active
        .as_ref()
        .is_some_and(|active| active.meeting_id == meeting_id);
    if !matches {
        return None;
    }
    let taken = active.take().map(|active| active.presence);
    taken.filter(|presence| !presence.is_empty())
}

/// The reports saved beside a meeting, for one whose process is gone.
pub(super) fn load(directory: &Path) -> Option<Presence> {
    let text = fs::read_to_string(directory.join(FILE)).ok()?;
    let mut presence = Presence::default();
    let mut seen = HashSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if let Ok(report) = serde_json::from_str::<Presence>(line) {
            presence.absorb(report, &mut seen);
        }
    }
    (!presence.is_empty()).then_some(presence)
}

/// Epoch milliseconds at which the meeting's transcript timeline starts:
/// the earliest first packet of its tracks, whose arrival stamps that zero.
pub(super) fn origin_epoch_ms(manifest: &MeetingManifest) -> Option<u64> {
    [&manifest.system, &manifest.microphone]
        .into_iter()
        .filter_map(|track| Some((track.first_arrival_us?, track.first_arrival_epoch_ms?)))
        .min_by_key(|(arrival_us, _)| *arrival_us)
        .map(|(_, epoch_ms)| epoch_ms)
}

/// Names every entry whose speaker the reports make clear, and returns how
/// many entries gained one. The microphone is the local participant.
pub(super) fn attribute(
    entries: &mut [TranscriptEntry],
    presence: &Presence,
    origin_epoch_ms: u64,
) -> usize {
    let events = presence
        .events
        .iter()
        .filter_map(|event| {
            // An interval that began before recording still covers its start.
            let start_ms = event.start_ms.saturating_sub(origin_epoch_ms);
            let end_ms = event.end_ms.checked_sub(origin_epoch_ms)?;
            (end_ms > start_ms).then(|| SpeakerEvent {
                name: event.name.clone(),
                start_ms,
                end_ms,
            })
        })
        .collect::<Vec<_>>();
    let mut named = 0;
    for entry in entries {
        entry.speaker = match entry.source {
            MeetingSource::Microphone => presence.self_name.clone(),
            MeetingSource::System => speaker_for(entry.start_ms, entry.end_ms, &events),
        };
        if entry.speaker.is_some() {
            named += 1;
        }
    }
    named
}

/// The participant whose talking covers a sentence, `None` when the call's
/// tiles do not make it clear.
fn speaker_for(start_ms: u64, end_ms: u64, events: &[SpeakerEvent]) -> Option<String> {
    let duration = end_ms.saturating_sub(start_ms);
    if duration == 0 {
        return None;
    }
    let mut coverage: BTreeMap<&str, Vec<(u64, u64)>> = BTreeMap::new();
    for event in events {
        let start = event.start_ms.max(start_ms);
        let end = event.end_ms.min(end_ms);
        if end <= start {
            continue;
        }
        coverage
            .entry(event.name.as_str())
            .or_default()
            .push((start, end));
    }
    let mut overlaps: Vec<(&str, u64)> = coverage
        .into_iter()
        .filter_map(|(name, spans)| {
            let covered = merged_ms(spans);
            (covered >= MIN_OVERLAP_MS).then_some((name, covered))
        })
        .collect();
    if overlaps.is_empty() {
        return None;
    }
    overlaps.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    let (best, best_ms) = overlaps[0];
    if best_ms * 100 >= duration * OWNER_COVERAGE {
        return Some(best.to_string());
    }
    if overlaps.len() == 1 {
        return (best_ms * 100 >= duration * SINGLE_COVERAGE).then(|| best.to_string());
    }
    let shared = overlaps
        .iter()
        .filter(|(_, covered)| *covered * 100 >= duration * SHARED_COVERAGE)
        .map(|(name, _)| *name)
        .take(MAX_SHARED_NAMES)
        .collect::<Vec<_>>();
    (shared.len() > 1).then(|| shared.join(" + "))
}

/// Total time covered by possibly overlapping spans.
fn merged_ms(mut spans: Vec<(u64, u64)>) -> u64 {
    spans.sort_unstable();
    let mut total = 0;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in spans {
        match current {
            Some((open_start, open_end)) if start <= open_end => {
                current = Some((open_start, open_end.max(end)));
            }
            Some((open_start, open_end)) => {
                total += open_end - open_start;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    total + current.map_or(0, |(start, end)| end - start)
}

/// The stable token the extension sends to the local API. It lives beside
/// the local API's discovery file, is readable only by the owner, and
/// survives restarts so the extension is configured once.
///
/// The token file beside a given discovery file, so a test or another
/// instance keeps its own.
pub fn token_beside(discovery_path: &Path) -> PathBuf {
    discovery_path.with_file_name(TOKEN_FILE)
}

/// Reads the token in `path`, if it is a well-formed one.
pub fn read_token(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let token = text.trim();
    valid_presence_token(token).then(|| token.to_string())
}

/// Reads, or creates, the token in `path`.
pub fn token_at(path: &Path) -> std::io::Result<String> {
    if let Some(token) = read_token(path) {
        return Ok(token);
    }
    let token = crate::local_api::generate_token().map_err(std::io::Error::other)?;
    let directory = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(token.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    crate::durable::sync_directory(directory)?;
    Ok(token)
}

fn valid_presence_token(token: &str) -> bool {
    token.len() == 68
        && token.starts_with("hex_")
        && token[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether `token` is the stored one, without an early exit on a near miss.
pub fn token_matches(stored: &str, token: &str) -> bool {
    constant_time_eq(stored.as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that use the process-wide active meeting.
    static ACTIVE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn event(name: &str, start_ms: u64, end_ms: u64) -> SpeakerEvent {
        SpeakerEvent {
            name: name.into(),
            start_ms,
            end_ms,
        }
    }

    fn entry(source: MeetingSource, start_ms: u64, end_ms: u64) -> TranscriptEntry {
        TranscriptEntry {
            source,
            start_ms,
            end_ms,
            text: "Sentence.".into(),
            speaker: None,
        }
    }

    #[test]
    fn a_single_clear_voice_names_the_sentence() {
        let events = [event("Alice", 1_000, 4_000)];
        assert_eq!(speaker_for(1_200, 3_800, &events).as_deref(), Some("Alice"));
    }

    #[test]
    fn a_brief_overlap_belongs_to_the_voice_that_covers_the_sentence() {
        let events = [event("Alice", 0, 10_000), event("Bob", 4_900, 5_100)];
        assert_eq!(
            speaker_for(5_000, 5_000 + 2_000, &events).as_deref(),
            Some("Alice")
        );
    }

    #[test]
    fn two_voices_sharing_a_sentence_are_joined() {
        let events = [event("Alice", 0, 2_000), event("Bob", 2_000, 4_000)];
        assert_eq!(
            speaker_for(0, 4_000, &events).as_deref(),
            Some("Alice + Bob")
        );
    }

    #[test]
    fn no_clear_voice_leaves_the_sentence_unnamed() {
        assert_eq!(speaker_for(0, 4_000, &[]), None);
        // One voice for a sliver of the sentence is not enough to own it.
        let events = [event("Alice", 0, 300)];
        assert_eq!(speaker_for(1_000, 5_000, &events), None);
    }

    #[test]
    fn overlapping_events_for_one_name_are_not_double_counted() {
        let events = [event("Alice", 0, 3_000), event("Alice", 1_000, 4_000)];
        assert_eq!(merged_ms(vec![(0, 3_000), (1_000, 4_000)]), 4_000);
        assert_eq!(speaker_for(0, 4_000, &events).as_deref(), Some("Alice"));
    }

    #[test]
    fn events_are_aligned_to_the_transcript_timeline() {
        let mut entries = [
            entry(MeetingSource::System, 5_000, 7_000),
            entry(MeetingSource::Microphone, 8_000, 9_000),
        ];
        let presence = Presence {
            self_name: Some("Camille".into()),
            participants: vec!["Camille".into(), "Alice".into()],
            events: vec![event("Alice", 1_015_000, 1_017_000)],
        };
        let named = attribute(&mut entries, &presence, 1_010_000);
        assert_eq!(named, 2);
        assert_eq!(entries[0].speaker.as_deref(), Some("Alice"));
        assert_eq!(entries[1].speaker.as_deref(), Some("Camille"));
    }

    #[test]
    fn reports_are_cleaned_before_they_are_stored() {
        let report: Report = serde_json::from_str(
            r#"{
                "selfName": "  Camille  ",
                "participants": ["Alice", " Alice ", "", "Bob"],
                "events": [
                    {"name": "Alice", "startMs": 10, "endMs": 20},
                    {"name": "Alice", "startMs": 10, "endMs": 20},
                    {"name": "Bob", "startMs": 30, "endMs": 30}
                ]
            }"#,
        )
        .unwrap();
        let presence = report.accept();
        assert_eq!(presence.self_name.as_deref(), Some("Camille"));
        assert_eq!(presence.participants, ["Camille", "Alice", "Bob"]);
        assert_eq!(presence.events, [event("Alice", 10, 20)]);
    }

    #[test]
    fn a_meet_address_or_tab_title_names_its_meet() {
        assert_eq!(
            meet_code("https://meet.google.com/rjz-xffn-gsc?authuser=0"),
            Some("rjz-xffn-gsc".into())
        );
        assert_eq!(
            meet_code("rjz-xffn-gsc – Enregistrement de la caméra - Google Chrome – russo"),
            Some("rjz-xffn-gsc".into())
        );
        assert_eq!(meet_code("Meeting — 4:02 PM"), None);
        assert_eq!(meet_code("Zoom call — 2:30 PM"), None);
        assert_eq!(meet_code("abcd-efg-hij"), None);
        assert_eq!(meet_code("abc-defg-hij-extra"), None);
        assert_eq!(meet_code("Review End-User-API"), None);
    }

    #[test]
    fn a_report_from_another_meet_is_refused() {
        let _serial = ACTIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "hex-presence-code-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let registration = register("meeting-code", &directory, Some("abc-defg-hij".into()));
        let report = |code: &str| format!(r#"{{"code":"{code}","participants":["Alice"]}}"#);

        assert_eq!(
            ingest(report("zzz-zzzz-zzz").as_bytes()).unwrap_err(),
            IngestError::OtherMeeting
        );
        assert!(ingest(report("abc-defg-hij").as_bytes()).is_ok());
        drop(registration);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_recording_without_a_code_takes_the_first_meet_with_people() {
        let _serial = ACTIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "hex-presence-adopt-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&directory).unwrap();
        let registration = register("meeting-adopt", &directory, None);
        // An idle tab with nobody in it claims nothing.
        assert!(ingest(br#"{"code":"zzz-zzzz-zzz"}"#).is_ok());
        assert_eq!(status().meeting_code, None);
        assert!(ingest(br#"{"code":"abc-defg-hij","participants":["Alice"]}"#).is_ok());
        assert_eq!(status().meeting_code.as_deref(), Some("abc-defg-hij"));
        assert_eq!(
            ingest(br#"{"code":"zzz-zzzz-zzz","participants":["Mallory"]}"#).unwrap_err(),
            IngestError::OtherMeeting
        );
        assert_eq!(
            ingest(br#"{"participants":["Mallory"]}"#).unwrap_err(),
            IngestError::OtherMeeting,
            "a report without a code is refused once the recording knows its own"
        );
        drop(registration);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_report_that_cannot_be_saved_is_a_storage_error() {
        let _serial = ACTIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "hex-presence-missing-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let registration = register("meeting-missing", &directory, None);
        assert_eq!(
            ingest(br#"{"participants":["Alice"]}"#).unwrap_err(),
            IngestError::Storage
        );
        drop(registration);
    }

    #[test]
    fn a_flickering_glow_is_one_turn_and_reports_are_bounded() {
        let mut presence = Presence::default();
        let mut seen = HashSet::new();
        let flicker = (0..10_000u64)
            .map(|index| event("Alice", index * 1_000, index * 1_000 + 800))
            .collect::<Vec<_>>();
        presence.absorb(
            Presence {
                events: flicker,
                ..Presence::default()
            },
            &mut seen,
        );
        assert_eq!(presence.events, [event("Alice", 0, 9_999_800)]);

        let crowd = Report {
            participants: (0..1_000).map(|index| format!("Guest {index}")).collect(),
            ..Report::default()
        };
        assert_eq!(crowd.accept().participants.len(), MAX_PARTICIPANTS);
    }

    #[test]
    fn a_report_without_meeting_is_refused() {
        let _serial = ACTIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            ingest(br#"{"participants":["Alice"]}"#).unwrap_err(),
            IngestError::NotRecording
        );
        assert_eq!(ingest(b"not json").unwrap_err(), IngestError::Invalid);
    }

    #[test]
    fn a_report_is_stored_once_and_read_back_from_the_meeting_folder() {
        let _serial = ACTIVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "hex-presence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let registration = register("meeting-1", &directory, None);
        let report = br#"{
            "selfName": "Camille",
            "participants": ["Camille", "Alice"],
            "events": [{"name": "Alice", "startMs": 1000, "endMs": 2000}]
        }"#;
        let first = ingest(report).unwrap();
        assert_eq!((first.participants, first.events), (2, 1));
        // A retried report adds nothing twice.
        let retried = ingest(report).unwrap();
        assert_eq!((retried.participants, retried.events), (2, 1));

        let presence = take("meeting-1").unwrap();
        assert_eq!(presence.self_name.as_deref(), Some("Camille"));
        assert_eq!(presence.participants, ["Camille", "Alice"]);
        assert_eq!(presence.events, [event("Alice", 1_000, 2_000)]);
        drop(registration);

        // A process that is gone leaves the same reports on disk.
        let loaded = load(&directory).unwrap();
        assert_eq!(loaded, presence);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn two_extension_profiles_count_as_two_connections() {
        let now = 1_000_000;
        let clients = [
            ("profile-a".to_string(), now - 10_000),
            ("profile-b".to_string(), now - 20_000),
        ];
        let status = status_at(&clients, now);
        assert!(status.connected());
        assert_eq!(status.clients, ["profile-a", "profile-b"]);
    }

    #[test]
    fn a_client_that_stops_checking_in_does_not_count() {
        let now = 1_000_000;
        let seen = now - CONNECTED_WINDOW_MS - 1;
        let clients = [("profile-a".to_string(), seen)];
        let status = status_at(&clients, now);
        assert!(!status.connected());
        assert!(status.clients.is_empty());
    }

    #[test]
    fn touching_distinct_clients_keeps_both() {
        touch(Some("profile-a-test"));
        touch(Some("profile-b-test"));
        let status = bridge_status(now_ms());
        assert!(status.clients.iter().any(|id| id == "profile-a-test"));
        assert!(status.clients.iter().any(|id| id == "profile-b-test"));
    }

    #[test]
    fn a_token_is_stable_and_survives_a_reread() {
        let directory = std::env::temp_dir().join(format!(
            "hex-presence-token-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let path = directory.join(TOKEN_FILE);
        let first = token_at(&path).unwrap();
        let second = token_at(&path).unwrap();
        assert_eq!(first, second);
        assert!(valid_presence_token(&first));
        assert!(constant_time_eq(first.as_bytes(), second.as_bytes()));
        assert!(!constant_time_eq(first.as_bytes(), b"hex_00"));
        let _ = fs::remove_dir_all(&directory);
    }
}
