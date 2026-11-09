//! Meeting titles and notes from the notes writer (`generator`: Apple
//! Intelligence, or OpenCode when turned on). A provisional title is written
//! while the meeting records; after final publication one call over a bounded
//! excerpt writes the final title, a multi-paragraph summary in the meeting's
//! language, key points, decisions, action items, and open questions.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

mod clean;
mod render;

pub(crate) use clean::*;
pub(crate) use render::*;

use super::generator::{self, GeneratedNotes, GenerationError, NotesWriter};
use super::{
    MeetingManifest, MeetingStatus, TranscriptEntry, TranscriptReader, coalesce_transcript,
    format_duration, read_manifest, root, sync_directory, write_manifest,
};
use crate::desktop::events::now_ms;

/// Room left in the context for instructions, the schema, and the response.
const RESERVED_TOKENS: usize = 1_000;
const MIN_USABLE_TOKENS: usize = 1_000;
/// Conservative for English speech, which averages about four per token.
const CHARS_PER_TOKEN: usize = 3;
const FALLBACK_CONTEXT_TOKENS: usize = 4_096;
/// Prompt size bounds latency more than the context does: about 3,000
/// tokens of transcript keeps one call near four seconds on an M2 Max.
const FINAL_EXCERPT_CHARS: usize = 12_000;
/// A remote model writing Standard or Detailed notes reads more of a long
/// meeting, so actions from its middle are not skipped.
const DETAILED_EXCERPT_CHARS: usize = 48_000;
const TITLE_EXCERPT_CHARS: usize = 5_000;
/// Long turns are split so a monologue can still be sampled.
const LINE_CHARS: usize = 600;
const GAP_MARKER: &str = "[…]";
const TOTAL_DEADLINE: Duration = Duration::from_secs(180);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const TITLE_TIMEOUT: Duration = Duration::from_secs(30);
const DICTATION_WAIT: Duration = Duration::from_secs(30);
const MAX_TITLE_WORDS: usize = 8;
const MAX_SUMMARY_PARAGRAPHS: usize = 8;
const MAX_SUMMARY_CHARS: usize = 4_000;
const MAX_SHORT_SUMMARY_SENTENCES: usize = 2;
const MAX_SHORT_SUMMARY_CHARS: usize = 400;
const MAX_KEY_POINTS: usize = 8;
const MAX_DECISIONS: usize = 5;
const MAX_ACTIONS: usize = 12;
const MAX_OPEN_QUESTIONS: usize = 5;
const MAX_ITEM_CHARS: usize = 300;

/// Provisional titles: the first once the opening has enough speech, one
/// refresh ten minutes in, and at most three attempts in all.
const TITLE_SPEECH_MS: u64 = 90_000;
const TITLE_WORDS: usize = 600;
const MIN_TITLE_WORDS: usize = 60;
const TITLE_REFRESH_MS: u64 = 10 * 60_000;
const TITLE_RETRY_MS: u64 = 60_000;
const MAX_TITLE_ATTEMPTS: u8 = 3;
const TITLE_CHECK_INTERVAL: Duration = Duration::from_secs(10);

pub(super) const NOTES_START: &str = "<!-- hex:notes -->";
pub(super) const NOTES_END: &str = "<!-- /hex:notes -->";

const SHORT_NOTES_INSTRUCTIONS: &str = "You title and summarize meeting transcripts. Each line \
starts with a timestamp and a speaker label: a name is that participant, You is the person who \
recorded the meeting, and Them is one or more participants whose name was not clear. A long \
meeting is excerpted, and […] marks skipped parts. Write one short, specific title for the whole \
meeting and a summary of at most two short sentences on the main topics and outcomes. Leave \
key_points, decisions, action_items, and open_questions empty. Use only what the transcript says; \
never invent names, dates, or numbers. Write the title and summary in the same language as the \
transcript.";

const STANDARD_NOTES_INSTRUCTIONS: &str = "You write meeting minutes. Each line starts with a \
timestamp and a speaker label: a name is that participant, You is the person who recorded the \
meeting, and Them is one or more participants whose name was not clear. A long meeting is \
excerpted, and […] marks skipped parts. Use only what the transcript says; never invent names, \
dates, numbers, owners, or deadlines. Write one short, specific title for the whole meeting. \
Write the summary as three to five short paragraphs that cover the context, the main topics, the \
decisions, and the outcomes. List the key points and the decisions separately. Leave action_items \
and open_questions empty. Write the title, summary, points, and decisions in the same language as \
the transcript.";

const DETAILED_NOTES_INSTRUCTIONS: &str = "You write meeting minutes. Each line starts with a \
timestamp and a speaker label: a name is that participant, You is the person who recorded the \
meeting, and Them is one or more participants whose name was not clear. A long meeting is \
excerpted, and […] marks skipped parts. Use only what the transcript says; never invent names, \
dates, numbers, owners, or deadlines. Write one short, specific title for the whole meeting. \
Write the summary as three to six short paragraphs that cover the context, the main topics, the \
decisions, and the outcomes; do not repeat action items in it. List the key points, the \
decisions, and open questions separately. List every task that someone explicitly agreed to do \
as an action item, with owner, due date, transcript timestamp, and a short supporting quote when \
the transcript gives them. Set owner to the participant who committed, using their label, and \
otherwise leave it null. Write the title, summary, points, decisions, action items, and questions \
in the same language as the transcript.";

fn notes_instructions(format: crate::app_settings::MeetingNotesFormat) -> &'static str {
    use crate::app_settings::MeetingNotesFormat;
    match format {
        MeetingNotesFormat::Short => SHORT_NOTES_INSTRUCTIONS,
        MeetingNotesFormat::Standard => STANDARD_NOTES_INSTRUCTIONS,
        MeetingNotesFormat::Detailed => DETAILED_NOTES_INSTRUCTIONS,
    }
}

const TITLE_INSTRUCTIONS: &str = "You name meetings from the first part of their transcript. \
Each line starts with a timestamp and a speaker label: a name is that participant, You is \
the person who recorded the meeting, and Them is one or more participants whose name was not \
clear. Write one short, specific title naming \
the main topic. Use only what the transcript says.";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MeetingNotes {
    pub title: String,
    /// The overview, as one or more paragraphs in the meeting's language.
    /// Older manifests stored a list of points, and action items and
    /// decisions, which are read as joined text or ignored.
    #[serde(default, deserialize_with = "summary_text")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_points: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action_items: Vec<ActionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub generated_at_ms: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    /// Transcript characters in the prompt; fewer than `transcript_chars`
    /// when a long meeting was excerpted.
    #[serde(default)]
    pub excerpt_chars: usize,
    #[serde(default)]
    pub transcript_chars: usize,
    /// The writer, and for OpenCode its `provider/model`. Notes written
    /// before Hex recorded it have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_by: Option<NotesWriter>,
}

impl MeetingNotes {
    pub fn excerpted(&self) -> bool {
        self.excerpt_chars < self.transcript_chars
    }

    /// The notes as plain text, for search and copy fallbacks.
    pub fn plain_text(&self) -> String {
        let mut parts = vec![self.title.trim().to_owned(), self.summary.trim().to_owned()];
        parts.extend(self.key_points.iter().map(|point| point.trim().to_owned()));
        parts.extend(
            self.decisions
                .iter()
                .map(|decision| decision.trim().to_owned()),
        );
        parts.extend(self.action_items.iter().map(ActionItem::plain_text));
        parts.extend(
            self.open_questions
                .iter()
                .map(|question| question.trim().to_owned()),
        );
        parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One follow-up task from a meeting. `owner` is only set when the
/// transcript names one; `at` and `quote` point back into the transcript.
///
/// Models answer loosely: `null` for a field they were told to leave empty,
/// a number for a timestamp. Every field reads leniently, and an item
/// without a task is dropped when the notes are cleaned.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActionItem {
    #[serde(default, deserialize_with = "loose_text")]
    pub owner: Option<String>,
    #[serde(default, deserialize_with = "text_or_null")]
    pub task: String,
    #[serde(default, deserialize_with = "loose_text")]
    pub due: Option<String>,
    #[serde(default, deserialize_with = "loose_text")]
    pub at: Option<String>,
    #[serde(default, deserialize_with = "loose_text")]
    pub quote: Option<String>,
}

impl ActionItem {
    /// `owner: task (due: …) [at]`, with `unassigned` for an item nobody
    /// took, in the caller's words (the pane translates them).
    pub fn line(&self, unassigned: &str, due_label: &str) -> String {
        let owner = self
            .owner
            .as_deref()
            .map(str::trim)
            .filter(|owner| !owner.is_empty())
            .unwrap_or(unassigned);
        let mut text = format!("{owner}: {}", self.task.trim());
        if let Some(due) = self.due.as_deref().filter(|due| !due.trim().is_empty()) {
            text.push_str(&format!(" ({due_label} {})", due.trim()));
        }
        if let Some(at) = self.at.as_deref().filter(|at| !at.trim().is_empty()) {
            text.push_str(&format!(" [{}]", at.trim()));
        }
        text
    }

    fn plain_text(&self) -> String {
        self.line("Unassigned", "due:")
    }
}

/// Characters of transcript one prompt may carry for a context window.
pub(super) fn excerpt_budget(context_tokens: usize, cap: usize) -> usize {
    let tokens = if context_tokens == 0 {
        FALLBACK_CONTEXT_TOKENS
    } else {
        context_tokens
    };
    (tokens
        .saturating_sub(RESERVED_TOKENS)
        .max(MIN_USABLE_TOKENS)
        * CHARS_PER_TOKEN)
        .min(cap)
}

/// Coalesced turns as `[mm:ss] You: text` lines, split at word boundaries
/// so no line exceeds `LINE_CHARS`.
fn transcript_lines(entries: &[TranscriptEntry]) -> Vec<String> {
    let mut lines = Vec::new();
    for turn in coalesce_transcript(entries.iter().cloned()) {
        let prefix = format!("[{}] {}: ", format_duration(turn.start_ms), turn.label());
        let mut line = prefix.clone();
        for word in turn.text.split_whitespace() {
            if line.len() > prefix.len() && line.len() + 1 + word.len() > LINE_CHARS {
                lines.push(std::mem::replace(&mut line, prefix.clone()));
            }
            if line.len() > prefix.len() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if line.len() > prefix.len() {
            lines.push(line);
        }
    }
    lines
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Excerpt {
    pub text: String,
    pub transcript_chars: usize,
}

/// The whole transcript when it fits in `max_chars`; otherwise the opening,
/// evenly spaced passages from the middle, and the ending, with `[…]` where
/// lines were skipped.
pub(super) fn excerpt(entries: &[TranscriptEntry], max_chars: usize) -> Excerpt {
    let lines = transcript_lines(entries);
    let size = |range: std::ops::Range<usize>| -> usize {
        lines[range.clone()].iter().map(String::len).sum::<usize>() + range.len().saturating_sub(1)
    };
    let transcript_chars = size(0..lines.len());
    if transcript_chars <= max_chars {
        return Excerpt {
            text: lines.join("\n"),
            transcript_chars,
        };
    }
    // Each window may cost a gap marker and two newlines.
    let gap = GAP_MARKER.len() + 2;
    let windows = (max_chars / 2_000).clamp(1, 6);
    let usable = max_chars.saturating_sub((windows + 1) * gap);
    let take_forward = |start: usize, end: usize, budget: usize| {
        let mut stop = start;
        let mut used = 0;
        while stop < end && used + lines[stop].len() < budget {
            used += lines[stop].len() + 1;
            stop += 1;
        }
        stop
    };
    let head_end = take_forward(0, lines.len(), usable * 3 / 10);
    let mut tail_start = lines.len();
    let mut used = 0;
    while tail_start > head_end && used + lines[tail_start - 1].len() < usable / 5 {
        used += lines[tail_start - 1].len() + 1;
        tail_start -= 1;
    }
    let mut ranges = Vec::with_capacity(windows + 2);
    ranges.push(0..head_end);
    let middle = tail_start.saturating_sub(head_end);
    let window_budget = (usable - usable * 3 / 10 - usable / 5) / windows;
    for index in 0..windows {
        let center = head_end + (2 * index + 1) * middle / (2 * windows);
        let start = center.max(ranges.last().map_or(0, |range| range.end));
        let end = take_forward(start, tail_start, window_budget);
        if end > start {
            ranges.push(start..end);
        }
    }
    ranges.push(tail_start..lines.len());

    let mut text = String::new();
    let mut previous_end = 0;
    for range in ranges.into_iter().filter(|range| !range.is_empty()) {
        if !text.is_empty() {
            text.push('\n');
        }
        if range.start > previous_end {
            text.push_str(GAP_MARKER);
            text.push('\n');
        }
        text.push_str(&lines[range.clone()].join("\n"));
        previous_end = range.end;
    }
    if previous_end < lines.len() {
        text.push('\n');
        text.push_str(GAP_MARKER);
    }
    Excerpt {
        text,
        transcript_chars,
    }
}

pub(super) struct Summary {
    pub notes: GeneratedNotes,
    pub excerpt_chars: usize,
    pub transcript_chars: usize,
    pub calls: usize,
}

/// One call over a bounded excerpt. A prompt that still exceeds the context
/// is retried once with half the excerpt; every call receives the time left
/// before `deadline`.
#[cfg(any(debug_assertions, test))]
pub(super) fn summarize_entries(
    entries: &[TranscriptEntry],
    max_chars: usize,
    deadline: Instant,
    generate: impl FnMut(&str, &str, Duration) -> Result<GeneratedNotes, GenerationError>,
) -> Result<Summary> {
    summarize_with_calendar(entries, None, &[], max_chars, deadline, generate)
}

/// The meeting language, when it was fixed. `auto` and `None` ask the model
/// to use the transcript's own language.
fn language_preamble(language: Option<&str>) -> String {
    match language {
        Some(code) if code != crate::transcription::models::AUTO_LANGUAGE => format!(
            "Meeting language: {} ({}). Write every field in this language.\n\n",
            crate::transcription::models::language_name(code),
            code
        ),
        // The instructions already ask for the transcript's language; an
        // empty preamble keeps older prompts and tests unchanged.
        _ => String::new(),
    }
}

/// The most attendee names a notes prompt lists.
const MAX_PROMPT_ATTENDEES: usize = 12;

/// The calendar event's title and attendees, ahead of the transcript.
fn calendar_preamble(calendar: Option<&super::MeetingCalendar>) -> String {
    let Some(calendar) = calendar else {
        return String::new();
    };
    let mut preamble = String::new();
    let title = calendar.title.trim();
    if !title.is_empty() {
        let title = title.chars().take(200).collect::<String>();
        preamble.push_str(&format!("Calendar event: {title}\n"));
    }
    let attendees = calendar
        .attendees
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .take(MAX_PROMPT_ATTENDEES)
        .map(|name| name.chars().take(80).collect::<String>())
        .collect::<Vec<_>>();
    if !attendees.is_empty() {
        preamble.push_str(&format!("Attendees: {}\n", attendees.join(", ")));
    }
    if !preamble.is_empty() {
        preamble.push('\n');
    }
    preamble
}

/// [`summarize_entries`] with the vocabulary's `terms` (Labs › Vocabulary
/// in meeting notes; empty leaves them out) and the calendar event as
/// context. Both count against the excerpt budget.
#[cfg(any(debug_assertions, test))]
pub(super) fn summarize_with_calendar(
    entries: &[TranscriptEntry],
    calendar: Option<&super::MeetingCalendar>,
    terms: &[String],
    max_chars: usize,
    deadline: Instant,
    generate: impl FnMut(&str, &str, Duration) -> Result<GeneratedNotes, GenerationError>,
) -> Result<Summary> {
    summarize_with_calendar_in(
        entries,
        calendar,
        terms,
        None,
        crate::app_settings::MeetingNotesFormat::Detailed,
        max_chars,
        deadline,
        generate,
    )
}

/// [`summarize_with_calendar`] with the meeting's fixed language and the
/// chosen amount of detail.
#[allow(clippy::too_many_arguments)]
pub(super) fn summarize_with_calendar_in(
    entries: &[TranscriptEntry],
    calendar: Option<&super::MeetingCalendar>,
    terms: &[String],
    language: Option<&str>,
    format: crate::app_settings::MeetingNotesFormat,
    max_chars: usize,
    deadline: Instant,
    mut generate: impl FnMut(&str, &str, Duration) -> Result<GeneratedNotes, GenerationError>,
) -> Result<Summary> {
    let preamble = format!(
        "{}{}{}",
        super::chapters::vocabulary_line(terms),
        language_preamble(language),
        calendar_preamble(calendar)
    );
    let mut max_chars = max_chars.saturating_sub(preamble.len()).max(1);
    let mut calls = 0;
    loop {
        let excerpt = excerpt(entries, max_chars);
        if excerpt.text.is_empty() {
            return Err(eyre!("the transcript has no speech to summarize"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(eyre!("the notes writer did not finish in time"));
        }
        calls += 1;
        let prompt = format!("{preamble}Meeting transcript:\n\n{}", excerpt.text);
        match generate(
            notes_instructions(format),
            &prompt,
            remaining.min(CALL_TIMEOUT),
        ) {
            Ok(notes) => {
                use crate::app_settings::MeetingNotesFormat;
                let (key_points, decisions, action_items, open_questions) = match format {
                    MeetingNotesFormat::Short => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
                    MeetingNotesFormat::Standard => (
                        clean_list(notes.key_points, MAX_KEY_POINTS),
                        clean_list(notes.decisions, MAX_DECISIONS),
                        Vec::new(),
                        Vec::new(),
                    ),
                    MeetingNotesFormat::Detailed => (
                        clean_list(notes.key_points, MAX_KEY_POINTS),
                        clean_list(notes.decisions, MAX_DECISIONS),
                        clean_action_items(notes.action_items),
                        clean_list(notes.open_questions, MAX_OPEN_QUESTIONS),
                    ),
                };
                return Ok(Summary {
                    notes: GeneratedNotes {
                        title: clean_title(&notes.title),
                        summary: clean_summary(&notes.summary, format),
                        key_points,
                        decisions,
                        action_items,
                        open_questions,
                        written_by: notes.written_by,
                    },
                    excerpt_chars: excerpt.text.len(),
                    transcript_chars: excerpt.transcript_chars,
                    calls,
                });
            }
            Err(GenerationError::ContextWindow) if calls == 1 => max_chars /= 2,
            Err(error) => return Err(eyre!("{error}")),
        }
    }
}

impl MeetingNotes {
    fn from_summary(summary: Summary, elapsed: Duration) -> Self {
        Self {
            title: summary.notes.title,
            summary: summary.notes.summary,
            key_points: summary.notes.key_points,
            decisions: summary.notes.decisions,
            action_items: summary.notes.action_items,
            open_questions: summary.notes.open_questions,
            generated_at_ms: now_ms(),
            elapsed_ms: elapsed.as_millis() as u64,
            excerpt_chars: summary.excerpt_chars,
            transcript_chars: summary.transcript_chars,
            written_by: summary.notes.written_by,
        }
    }
}

/// The title shown for a meeting. A meeting with a default app-and-time
/// title shows its generated title, else its provisional title; a named or
/// renamed meeting keeps its name. Chapters extend any title but one the
/// user typed.
pub fn display_title(manifest: &MeetingManifest) -> String {
    let base = base_title(manifest);
    if manifest.chapters.is_empty() || manifest.title_source() == super::TitleSource::User {
        return base;
    }
    // While recording, the last chapter runs until now.
    let end_ms = manifest.duration_ms.or_else(|| {
        manifest
            .status
            .is_active()
            .then(|| now_ms().saturating_sub(manifest.created_at_ms))
    });
    super::chapters::titled(&base, &manifest.chapters, end_ms)
}

fn base_title(manifest: &MeetingManifest) -> String {
    if manifest.title_source() != super::TitleSource::Default {
        return manifest.title.clone();
    }
    manifest
        .notes
        .as_ref()
        .map(|notes| notes.title.trim())
        .filter(|title| !title.is_empty())
        .or_else(|| {
            manifest
                .provisional_title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
        })
        .map_or_else(|| manifest.title.clone(), str::to_owned)
}

/// How far a recording has come, for deciding when to name it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Progress {
    /// Summed duration of transcribed speech from both sides.
    pub speech_ms: u64,
    pub words: usize,
    /// The end of the latest transcribed speech.
    pub timeline_ms: u64,
}

impl Progress {
    pub fn of(entries: &[TranscriptEntry]) -> Self {
        entries
            .iter()
            .fold(Self::default(), |progress, entry| Self {
                speech_ms: progress.speech_ms + entry.end_ms.saturating_sub(entry.start_ms),
                words: progress.words + entry.text.split_whitespace().count(),
                timeline_ms: progress.timeline_ms.max(entry.end_ms),
            })
    }
}

/// When to ask for a provisional title: once about 90 seconds of speech
/// (or 600 words) exist, then once more ten minutes in. Failures retry a
/// minute later, with at most three attempts in all.
#[derive(Debug, Default)]
pub(super) struct TitlePlan {
    named: u8,
    attempts: u8,
    retry_at_ms: u64,
}

impl TitlePlan {
    pub fn due(&self, progress: Progress) -> bool {
        if self.attempts >= MAX_TITLE_ATTEMPTS || progress.timeline_ms < self.retry_at_ms {
            return false;
        }
        match self.named {
            0 => {
                progress.words >= MIN_TITLE_WORDS
                    && (progress.speech_ms >= TITLE_SPEECH_MS || progress.words >= TITLE_WORDS)
            }
            1 => progress.timeline_ms >= TITLE_REFRESH_MS,
            _ => false,
        }
    }

    pub fn record(&mut self, named: bool, progress: Progress) {
        self.attempts += 1;
        if named {
            self.named += 1;
        } else {
            self.retry_at_ms = progress.timeline_ms + TITLE_RETRY_MS;
        }
    }

    pub fn finished(&self) -> bool {
        self.named >= 2 || self.attempts >= MAX_TITLE_ATTEMPTS
    }
}

/// Names a recording meeting in the background. The recorder takes each new
/// title and records it in its manifest; dropping this stops the worker and
/// discards a title still being generated.
pub(super) struct ProvisionalTitles {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<String>>>,
}

impl ProvisionalTitles {
    pub fn start(id: String) -> Self {
        let titles = Self {
            stop: Arc::new(AtomicBool::new(false)),
            latest: Arc::new(Mutex::new(None)),
        };
        let stop = titles.stop.clone();
        let latest = titles.latest.clone();
        if let Err(error) = thread::Builder::new()
            .name("meeting-title".into())
            .spawn(move || {
                crate::transcription::background_priority::deprioritize_background_thread();
                name_while_recording(&id, &stop, &latest);
            })
        {
            tracing::warn!(%error, "could not start provisional meeting titles");
        }
        titles
    }

    pub fn take(&self) -> Option<String> {
        self.latest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

impl Drop for ProvisionalTitles {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn name_while_recording(id: &str, stop: &AtomicBool, latest: &Mutex<Option<String>>) {
    let mut reader = TranscriptReader::default();
    let mut plan = TitlePlan::default();
    let mut next_check = Instant::now() + TITLE_CHECK_INTERVAL;
    while !stop.load(Ordering::Relaxed) && !plan.finished() {
        if Instant::now() < next_check {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        next_check = Instant::now() + TITLE_CHECK_INTERVAL;
        let entries = match reader.read(id) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!(%error, meeting = id, "could not read the transcript for a title");
                continue;
            }
        };
        let progress = Progress::of(&entries);
        if !plan.due(progress) || !notes_wanted() {
            continue;
        }
        if !wait_for_dictation(stop) {
            return;
        }
        let started = Instant::now();
        let result = provisional_title(
            &entries,
            excerpt_budget(generator::context_tokens(), TITLE_EXCERPT_CHARS),
            TITLE_TIMEOUT,
            generator::generate_title,
        );
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match result {
            Ok(title) => {
                tracing::info!(
                    meeting = id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    words = progress.words,
                    "generated provisional meeting title"
                );
                *latest.lock().unwrap_or_else(|error| error.into_inner()) = Some(title);
                plan.record(true, progress);
            }
            Err(error) => {
                tracing::warn!(%error, meeting = id, "could not generate a provisional meeting title");
                plan.record(false, progress);
            }
        }
    }
}

/// One title-only call over the transcript so far.
pub(super) fn provisional_title(
    entries: &[TranscriptEntry],
    max_chars: usize,
    timeout: Duration,
    generate: impl FnOnce(&str, &str, Duration) -> Result<String, GenerationError>,
) -> Result<String> {
    let excerpt = excerpt(entries, max_chars);
    if excerpt.text.is_empty() {
        return Err(eyre!("the transcript has no speech yet"));
    }
    let prompt = format!("Meeting transcript so far:\n\n{}", excerpt.text);
    let title =
        clean_title(&generate(TITLE_INSTRUCTIONS, &prompt, timeout).map_err(|e| eyre!("{e}"))?);
    if title.is_empty() {
        return Err(eyre!("the notes writer returned an empty title"));
    }
    Ok(title)
}

/// Renames the exported file's heading in place, under the same rule as
/// generated titles.
pub(super) fn retitle_markdown_file(
    path: &Path,
    written: &mut Option<String>,
    previous_title: &str,
    title: &str,
) -> Result<()> {
    rewrite_markdown_file(path, written, |markdown| {
        apply_title(markdown, previous_title, title)
    })
}

/// Applies the notes to the exported file in place; the caller renames it
/// to the new title when Labs › Descriptive meeting file names is on.
fn update_markdown_file(
    path: &Path,
    written: &mut Option<String>,
    previous_title: &str,
    title: &str,
    block: &str,
) -> Result<()> {
    rewrite_markdown_file(path, written, |markdown| {
        apply_notes(markdown, previous_title, title, block)
    })
}

/// Applies the notes to the exported file and, with Labs › Descriptive
/// meeting file names on, renames it to the new title. A missing file is
/// left to the manifest alone.
fn apply_notes_to_export(
    manifest: &mut MeetingManifest,
    previous_title: &str,
    title: &str,
    descriptive_filenames: bool,
) -> Result<()> {
    let Some(path) = manifest.export_path.clone().filter(|path| path.exists()) else {
        return Ok(());
    };
    let block = render_notes_block(manifest.notes.as_ref().expect("notes were set"));
    update_markdown_file(
        &path,
        &mut manifest.export_sha256,
        previous_title,
        title,
        &block,
    )?;
    super::export_name::rename_to_title(manifest, title, descriptive_filenames);
    Ok(())
}

/// The file is replaced atomically, and only if nothing changed it meanwhile.
/// `written` is the SHA-256 of the copy Hex last wrote; it follows the
/// rewrite only while the file was still that copy, so a file the user
/// edited never becomes Hex's to expire.
fn rewrite_markdown_file(
    path: &Path,
    written: &mut Option<String>,
    rewrite: impl FnOnce(&str) -> String,
) -> Result<()> {
    let original =
        fs::read_to_string(path).wrap_err_with(|| format!("could not read {}", path.display()))?;
    let updated = rewrite(&original);
    if updated == original {
        return Ok(());
    }
    let unchanged = written.as_deref() == Some(super::text_sha256(original.as_bytes()).as_str());
    let folder = path
        .parent()
        .ok_or_else(|| eyre!("{} has no folder", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| eyre!("{} has no name", path.display()))?
        .to_string_lossy();
    let temporary = folder.join(format!(".{name}.notes.tmp"));
    fs::write(&temporary, &updated)?;
    if let Ok(metadata) = fs::metadata(path) {
        let _ = fs::set_permissions(&temporary, metadata.permissions());
    }
    crate::durable::sync_file(&temporary)?;
    if fs::read_to_string(path).ok().as_deref() != Some(original.as_str()) {
        let _ = fs::remove_file(&temporary);
        return Err(eyre!("the Markdown changed while it was being updated"));
    }
    fs::rename(&temporary, path)?;
    if unchanged {
        *written = Some(super::text_sha256(updated.as_bytes()));
    }
    Ok(sync_directory(folder)?)
}

static IN_PROGRESS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

struct InProgress(String);

impl InProgress {
    fn claim(id: &str) -> Option<Self> {
        IN_PROGRESS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.to_string())
            .then(|| Self(id.to_string()))
    }
}

impl Drop for InProgress {
    fn drop(&mut self) {
        IN_PROGRESS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.0);
    }
}

pub fn notes_in_progress(id: &str) -> bool {
    IN_PROGRESS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(id)
}

/// Whether finished meetings should be summarized automatically.
pub fn notes_wanted() -> bool {
    generator::automatic()
}

/// Yields to dictation for at most `DICTATION_WAIT`. Returns false when
/// `stop` was set meanwhile.
pub(super) fn wait_for_dictation(stop: &AtomicBool) -> bool {
    let started = Instant::now();
    while crate::transcription::engine::dictation_busy() && started.elapsed() < DICTATION_WAIT {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !stop.load(Ordering::Relaxed)
}

/// Generates notes for one completed meeting with the real model and records
/// them. Blocks; call from a background thread. Failures are recorded in
/// `notes_error` and leave the Markdown untouched.
#[cfg(debug_assertions)]
pub fn summarize(id: &str) -> Result<MeetingManifest> {
    let claim = InProgress::claim(id).ok_or_else(|| eyre!("notes are already being written"))?;
    summarize_claimed(id, claim)
}

/// Generates and records notes while holding this meeting's claim.
fn summarize_claimed(id: &str, _claim: InProgress) -> Result<MeetingManifest> {
    let directory = root()?.join(id);
    let mut manifest = read_manifest(&directory)?;
    if manifest.status != MeetingStatus::Complete {
        return Err(eyre!("only completed meetings can be summarized"));
    }
    // A writer that isn't there (OpenCode not running, Apple Intelligence
    // off) is the meeting's notes error, so the pane says why.
    if let Err(reason) = generator::available() {
        tracing::warn!(%reason, meeting = id, "could not generate meeting notes");
        manifest.notes_error = Some(reason.clone());
        write_manifest(&directory, &manifest)?;
        return Err(eyre!("{reason}"));
    }
    let entries = super::read_ndjson(&directory.join("transcript.ndjson"))?;
    let never = AtomicBool::new(false);
    if crate::labs::live_chapters() {
        super::chapters::refine_meeting(&directory, &entries, &never);
    }
    // Read at each generation, so Regenerate summary uses the current list.
    let terms = if crate::labs::meeting_notes_vocabulary() {
        crate::dictation::vocabulary::terms()
    } else {
        Vec::new()
    };
    let started = Instant::now();
    let (format, cap) = notes_plan(
        generator::opencode_writes(),
        crate::app_settings::meeting_notes_format(),
    );
    let result = summarize_with_calendar_in(
        &entries,
        manifest.calendar.as_ref(),
        &terms,
        manifest.language.as_deref(),
        format,
        excerpt_budget(generator::context_tokens(), cap),
        started + TOTAL_DEADLINE,
        |instructions, prompt, timeout| {
            wait_for_dictation(&never);
            generator::generate_notes(instructions, prompt, timeout)
        },
    );
    // Re-read so concurrent manifest updates are not lost.
    let mut manifest = read_manifest(&directory)?;
    match result {
        Ok(summary) => {
            tracing::info!(
                meeting = id,
                excerpt_chars = summary.excerpt_chars,
                transcript_chars = summary.transcript_chars,
                calls = summary.calls,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "generated meeting notes"
            );
            let previous_title = display_title(&manifest);
            let notes = MeetingNotes::from_summary(summary, started.elapsed());
            manifest.notes = Some(notes);
            manifest.notes_error = None;
            let title = display_title(&manifest);
            if let Err(error) = apply_notes_to_export(
                &mut manifest,
                &previous_title,
                &title,
                crate::labs::meeting_descriptive_filenames(),
            ) {
                tracing::warn!(%error, meeting = id, "could not add notes to meeting Markdown");
                manifest.notes_error = Some(format!("Notes not added to Markdown: {error}"));
            }
            write_manifest(&directory, &manifest)?;
            Ok(manifest)
        }
        Err(error) => {
            tracing::warn!(%error, meeting = id, "could not generate meeting notes");
            manifest.notes_error = Some(error.to_string());
            write_manifest(&directory, &manifest)?;
            Err(error)
        }
    }
}

/// The format and excerpt cap the writer can serve. Apple Intelligence's
/// schema holds a title and at most two sentences, and its latency grows
/// with the prompt, so it always writes Short notes over the smaller
/// excerpt; a remote model reads more of a long meeting for Standard and
/// Detailed notes.
fn notes_plan(
    opencode: bool,
    requested: crate::app_settings::MeetingNotesFormat,
) -> (crate::app_settings::MeetingNotesFormat, usize) {
    use crate::app_settings::MeetingNotesFormat;
    match (opencode, requested) {
        (true, MeetingNotesFormat::Short) | (false, _) => {
            (MeetingNotesFormat::Short, FINAL_EXCERPT_CHARS)
        }
        (true, format) => (format, DETAILED_EXCERPT_CHARS),
    }
}

/// Parses the turns of an exported meeting Markdown file (`**You** · 01:02`
/// followed by the turn's text).
#[cfg(any(debug_assertions, test))]
pub(super) fn entries_from_markdown(markdown: &str) -> Vec<TranscriptEntry> {
    let mut entries: Vec<TranscriptEntry> = Vec::new();
    let mut in_turn = false;
    for line in markdown.lines() {
        let header = line
            .strip_prefix("**")
            .and_then(|rest| rest.split_once("** · "))
            .and_then(|(label, clock)| {
                let source = match label {
                    "You" => super::MeetingSource::Microphone,
                    "Them" => super::MeetingSource::System,
                    _ => super::MeetingSource::System,
                };
                let (minutes, seconds) = clock.trim().split_once(':')?;
                let start_ms =
                    (minutes.parse::<u64>().ok()? * 60 + seconds.parse::<u64>().ok()?) * 1_000;
                let speaker = (source == super::MeetingSource::System && label != "Them")
                    .then(|| label.to_string());
                Some((source, start_ms, speaker))
            });
        if let Some((source, start_ms, speaker)) = header {
            entries.push(TranscriptEntry {
                source,
                start_ms,
                end_ms: start_ms,
                text: String::new(),
                speaker,
            });
            in_turn = true;
        } else if line.starts_with('#') || line.starts_with(NOTES_START) {
            in_turn = false;
        } else if in_turn
            && !line.trim().is_empty()
            && let Some(entry) = entries.last_mut()
        {
            if !entry.text.is_empty() {
                entry.text.push(' ');
            }
            entry.text.push_str(line.trim());
        }
    }
    entries.retain(|entry| !entry.text.is_empty());
    // Markdown has start times only: a turn lasts until the next one, or
    // about 150 words a minute for the last.
    let starts = entries
        .iter()
        .skip(1)
        .map(|entry| entry.start_ms)
        .collect::<Vec<_>>();
    for (index, entry) in entries.iter_mut().enumerate() {
        let spoken = entry.start_ms + entry.text.split_whitespace().count() as u64 * 400;
        entry.end_ms = starts
            .get(index)
            .map_or(spoken, |next| (*next).max(entry.start_ms));
    }
    entries
}

/// Runs the real model on a transcript file without recording anything:
/// first the provisional title at the point a recording would ask for it,
/// then the final title and summary. Debug CLI: `hex meeting summarize <path>`.
#[cfg(debug_assertions)]
pub fn summarize_file(path: &Path, excerpt_chars: Option<usize>) -> Result<String> {
    generator::available().map_err(|reason| eyre!("{reason}"))?;
    let text =
        fs::read_to_string(path).wrap_err_with(|| format!("could not read {}", path.display()))?;
    let entries = entries_from_markdown(&text);
    let context = generator::context_tokens();
    let mut report = String::new();

    let plan = TitlePlan::default();
    let opening = (1..=entries.len())
        .find(|count| plan.due(Progress::of(&entries[..*count])))
        .unwrap_or(entries.len());
    let progress = Progress::of(&entries[..opening]);
    let started = Instant::now();
    let title = provisional_title(
        &entries[..opening],
        excerpt_budget(context, TITLE_EXCERPT_CHARS),
        TITLE_TIMEOUT,
        generator::generate_title,
    );
    report.push_str(&format!(
        "provisional title at {} ({} words, {}s of speech): {} · {:.1}s\n",
        format_duration(progress.timeline_ms),
        progress.words,
        progress.speech_ms / 1_000,
        match &title {
            Ok(title) => title.clone(),
            Err(error) => format!("failed: {error}"),
        },
        started.elapsed().as_secs_f32()
    ));

    let max_chars = excerpt_chars.unwrap_or_else(|| excerpt_budget(context, FINAL_EXCERPT_CHARS));
    let started = Instant::now();
    let summary = summarize_entries(
        &entries,
        max_chars,
        started + TOTAL_DEADLINE,
        generator::generate_notes,
    )?;
    let elapsed = started.elapsed();
    let calls = summary.calls;
    let notes = MeetingNotes::from_summary(summary, elapsed);
    report.push_str(&format!(
        "\n# {}\n\n{}\ncontext {context} tokens · transcript {} turns, {} chars · excerpt {} chars (budget {max_chars}) · calls {calls}\nfinal title + summary {:.1}s\n",
        notes.title,
        render_notes_block(&notes),
        entries.len(),
        notes.transcript_chars,
        notes.excerpt_chars,
        elapsed.as_secs_f32()
    ));
    Ok(report)
}

/// Summarizes on a utility-QoS thread and reports the outcome. Returns false
/// when notes for this meeting are already being written.
pub fn spawn_summary(
    id: String,
    done: impl FnOnce(Result<MeetingManifest>) + Send + 'static,
) -> bool {
    let Some(claim) = InProgress::claim(&id) else {
        return false;
    };
    thread::Builder::new()
        .name("meeting-notes".into())
        .spawn(move || {
            crate::transcription::background_priority::deprioritize_background_thread();
            done(summarize_claimed(&id, claim));
        })
        .map_err(|error| tracing::error!(%error, "could not start meeting notes"))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::MeetingSource;

    fn entry(source: MeetingSource, start_ms: u64, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            source,
            start_ms,
            end_ms: start_ms + 1_000,
            text: text.into(),
            speaker: None,
        }
    }

    fn conversation(turns: usize) -> Vec<TranscriptEntry> {
        (0..turns)
            .map(|index| {
                entry(
                    if index % 2 == 0 {
                        MeetingSource::Microphone
                    } else {
                        MeetingSource::System
                    },
                    index as u64 * 10_000,
                    &format!("Turn {index} talks about the launch plan and the docs."),
                )
            })
            .collect()
    }

    #[test]
    fn transcript_lines_carry_participant_names() {
        let mut entries = conversation(2);
        entries[0].speaker = Some("Camille".into());
        entries[1].speaker = Some("Alice".into());
        let lines = transcript_lines(&entries);
        assert!(
            lines.iter().any(|line| line.contains("] Camille: ")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("] Alice: ")),
            "{lines:?}"
        );
    }

    fn generated(title: &str, summary: &str) -> GeneratedNotes {
        GeneratedNotes {
            title: title.into(),
            summary: summary.into(),
            ..Default::default()
        }
    }

    #[test]
    fn excerpt_budget_scales_with_the_context_window_up_to_the_cap() {
        assert_eq!(excerpt_budget(4_096, usize::MAX), 3_096 * 3);
        assert_eq!(
            excerpt_budget(0, usize::MAX),
            excerpt_budget(4_096, usize::MAX)
        );
        assert_eq!(excerpt_budget(1_000, usize::MAX), 3_000);
        assert_eq!(
            excerpt_budget(8_192, FINAL_EXCERPT_CHARS),
            FINAL_EXCERPT_CHARS
        );
    }

    #[test]
    fn a_short_transcript_is_sent_whole_in_one_call() {
        let mut prompts = Vec::new();
        let summary = summarize_entries(
            &conversation(4),
            FINAL_EXCERPT_CHARS,
            Instant::now() + Duration::from_secs(60),
            |instructions, prompt, _| {
                prompts.push((instructions.to_string(), prompt.to_string()));
                Ok(generated(
                    "\"Launch plan review.\"",
                    "- They reviewed the launch plan.  Docs move to Friday. Then more. And more.",
                ))
            },
        )
        .unwrap();
        assert_eq!(summary.calls, 1);
        assert_eq!(
            prompts[0].0,
            notes_instructions(crate::app_settings::MeetingNotesFormat::Detailed)
        );
        assert!(
            prompts[0]
                .1
                .starts_with("Meeting transcript:\n\n[00:00] You: Turn 0")
        );
        assert!(
            prompts[0]
                .1
                .ends_with("[00:30] Them: Turn 3 talks about the launch plan and the docs.")
        );
        assert_eq!(summary.notes.title, "Launch plan review");
        assert_eq!(
            summary.notes.summary,
            "They reviewed the launch plan. Docs move to Friday. Then more. And more."
        );
        assert_eq!(summary.excerpt_chars, summary.transcript_chars);
    }

    #[test]
    fn a_calendar_meeting_gives_its_title_and_attendees_as_context() {
        let calendar = crate::meeting::MeetingCalendar {
            title: "Q4 launch sync".into(),
            attendees: vec!["Ana Ruiz".into(), " ".into(), "Ben Ito".into()],
        };
        let mut prompts = Vec::new();
        summarize_with_calendar(
            &conversation(2),
            Some(&calendar),
            &[],
            FINAL_EXCERPT_CHARS,
            Instant::now() + Duration::from_secs(60),
            |_, prompt, _| {
                prompts.push(prompt.to_string());
                Ok(generated("Launch", "They met."))
            },
        )
        .unwrap();
        assert!(prompts[0].starts_with(
            "Calendar event: Q4 launch sync\nAttendees: Ana Ruiz, Ben Ito\n\nMeeting transcript:\n\n[00:00] You: Turn 0"
        ));
    }

    #[test]
    fn vocabulary_terms_lead_the_notes_prompt_and_share_its_budget() {
        let calendar = crate::meeting::MeetingCalendar {
            title: "Q4 launch sync".into(),
            attendees: vec!["Ana Ruiz".into()],
        };
        let terms = vec!["Kubernetes".to_string(), "Ana Ruiz".to_string()];
        let mut prompts = Vec::new();
        summarize_with_calendar(
            &conversation(2),
            Some(&calendar),
            &terms,
            FINAL_EXCERPT_CHARS,
            Instant::now() + Duration::from_secs(60),
            |_, prompt, _| {
                prompts.push(prompt.to_string());
                Ok(generated("Launch", "They met."))
            },
        )
        .unwrap();
        assert!(prompts[0].starts_with(
            "Names and terms that may appear: Kubernetes, Ana Ruiz\n\nCalendar event: Q4 launch sync\nAttendees: Ana Ruiz\n\nMeeting transcript:\n\n[00:00] You: Turn 0"
        ));

        // No terms: the prompt is the one Hex sent before.
        let mut without = Vec::new();
        summarize_with_calendar(
            &conversation(2),
            None,
            &[],
            FINAL_EXCERPT_CHARS,
            Instant::now() + Duration::from_secs(60),
            |_, prompt, _| {
                without.push(prompt.to_string());
                Ok(generated("Launch", "They met."))
            },
        )
        .unwrap();
        assert!(without[0].starts_with("Meeting transcript:\n\n[00:00] You: Turn 0"));

        // The vocabulary line comes out of the transcript's budget.
        let entries = conversation(400);
        let budget = 6_000;
        let line = crate::meeting::chapters::vocabulary_line(&terms).len();
        let mut excerpts = Vec::new();
        for terms in [&[][..], &terms[..]] {
            let summary = summarize_with_calendar(
                &entries,
                None,
                terms,
                budget,
                Instant::now() + Duration::from_secs(60),
                |_, _, _| Ok(generated("Launch", "They met.")),
            )
            .unwrap();
            excerpts.push(summary.excerpt_chars);
        }
        assert!(excerpts[0] <= budget, "{excerpts:?}");
        assert!(excerpts[1] <= budget - line, "{excerpts:?}");
    }

    #[test]
    fn long_meetings_are_excerpted_from_the_opening_middle_and_ending() {
        let entries = conversation(400);
        let excerpt = excerpt(&entries, 12_000);
        assert!(excerpt.text.len() < excerpt.transcript_chars);
        assert!(excerpt.text.len() <= 12_000, "{}", excerpt.text.len());
        let lines = excerpt.text.lines().collect::<Vec<_>>();
        assert!(
            lines
                .iter()
                .all(|line| *line == GAP_MARKER || line.starts_with('['))
        );
        let turns = lines
            .iter()
            .filter_map(|line| line.split_whitespace().nth(3)?.parse::<usize>().ok())
            .collect::<Vec<_>>();
        assert_eq!(turns.first(), Some(&0));
        assert_eq!(turns.last(), Some(&399));
        assert!(turns.windows(2).all(|pair| pair[0] < pair[1]));
        // Evenly spaced passages from the middle, separated by gaps.
        assert!(turns.iter().any(|turn| (100..200).contains(turn)));
        assert!(turns.iter().any(|turn| (200..300).contains(turn)));
        assert!(lines.iter().filter(|line| **line == GAP_MARKER).count() >= 3);

        let whole = super::excerpt(&entries[..3], 4_000);
        assert_eq!(whole.text.len(), whole.transcript_chars);
        assert_eq!(whole.text.lines().count(), 3);
    }

    #[test]
    fn long_turns_are_split_into_lines_that_can_be_sampled() {
        let entries = vec![
            entry(MeetingSource::Microphone, 0, "Hello there."),
            entry(MeetingSource::System, 5_000, &"word ".repeat(400)),
        ];
        let lines = transcript_lines(&entries);
        assert_eq!(lines[0], "[00:00] You: Hello there.");
        assert!(lines.len() > 2);
        assert!(lines.iter().all(|line| line.len() <= LINE_CHARS));
        assert!(
            lines[1..]
                .iter()
                .all(|line| line.starts_with("[00:05] Them: word"))
        );
        let words = lines
            .iter()
            .flat_map(|line| line.split_whitespace())
            .filter(|word| *word == "word")
            .count();
        assert_eq!(words, 400);
    }

    #[test]
    fn context_overflow_halves_the_excerpt_once_and_failures_stop() {
        let mut sizes = Vec::new();
        let summary = summarize_entries(
            &conversation(400),
            8_000,
            Instant::now() + Duration::from_secs(60),
            |_, prompt, _| {
                sizes.push(prompt.len());
                if sizes.len() == 1 {
                    Err(GenerationError::ContextWindow)
                } else {
                    Ok(generated("Launch", "Short."))
                }
            },
        )
        .unwrap();
        assert_eq!(summary.calls, 2);
        assert!(sizes[1] < sizes[0] * 2 / 3, "{sizes:?}");

        let mut calls = 0;
        let error = summarize_entries(
            &conversation(4),
            8_000,
            Instant::now() + Duration::from_secs(60),
            |_, _, _| {
                calls += 1;
                Err(GenerationError::ContextWindow)
            },
        )
        .err()
        .unwrap();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("context"), "{error}");

        let error = summarize_entries(
            &conversation(4),
            8_000,
            Instant::now() + Duration::from_secs(60),
            |_, _, _| Err(GenerationError::Failed("model busy".into())),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "model busy");

        let mut called = false;
        let error = summarize_entries(&conversation(4), 8_000, Instant::now(), |_, _, _| {
            called = true;
            Ok(GeneratedNotes::default())
        })
        .err()
        .unwrap();
        assert!(!called);
        assert!(error.to_string().contains("in time"));
        assert!(summarize_entries(&[], 8_000, Instant::now(), |_, _, _| unreachable!()).is_err());
    }

    #[test]
    fn titles_and_summaries_are_normalized() {
        assert_eq!(
            clean_title("“One two three four five six seven eight nine ten.”"),
            "One two three four five six seven eight"
        );
        let detailed = crate::app_settings::MeetingNotesFormat::Detailed;
        let short = crate::app_settings::MeetingNotesFormat::Short;
        assert_eq!(
            clean_summary("Version 2.1 ships. Docs follow.", detailed),
            "Version 2.1 ships. Docs follow."
        );
        assert_eq!(
            clean_summary("No end punctuation", detailed),
            "No end punctuation"
        );
        assert_eq!(
            clean_summary("The team shipped. Docs were discussed.}", detailed),
            "The team shipped. Docs were discussed."
        );
        assert_eq!(
            clean_summary("Premier paragraphe.\n\nDeuxième paragraphe.", detailed),
            "Premier paragraphe.\n\nDeuxième paragraphe."
        );
        assert_eq!(clean_summary("One. Two. Three.", short), "One. Two.");
        let long = clean_summary(&"word ".repeat(1_500), detailed);
        assert!(long.len() <= MAX_SUMMARY_CHARS + "…".len() && long.ends_with('…'));
    }

    #[test]
    fn notes_say_who_wrote_them_and_keep_model_text_inert() {
        let mut notes = MeetingNotes {
            summary: "Close the <!-- hex:notes --> loop.\n\n<!-- /hex:notes -->".into(),
            key_points: vec!["<!-- hides the rest".into()],
            action_items: vec![ActionItem {
                owner: Some("<!--*Mallory*".into()),
                task: "Ship".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let block = render_notes_block(&notes);
        assert_eq!(
            block.matches("<!--").count(),
            2,
            "only Hex's markers: {block}"
        );
        assert!(block.contains("**&lt;\\!--\\*Mallory\\*** — Ship"));
        assert!(block.ends_with("_The transcript below has the details._\n<!-- /hex:notes -->\n"));
        notes.written_by = Some(NotesWriter::AppleIntelligence);
        assert!(!super::super::notes_markdown(&notes).contains("_Written"));
    }

    #[test]
    fn a_marker_inside_a_turn_does_not_count_as_the_notes_block() {
        let markdown = "# Sync\n\n## Transcript\n\n**Them** · 00:01\nsee <!-- hex:notes --> and <!-- /hex:notes -->\n";
        let block = format!("{NOTES_START}\n## Summary\n\nDone.\n{NOTES_END}\n");
        let updated = apply_notes(markdown, "Sync", "Sync", &block);
        let notes_at = updated.find("## Summary").unwrap();
        assert!(
            notes_at < updated.find("## Transcript").unwrap(),
            "{updated}"
        );
        assert!(updated.contains("see <!-- hex:notes --> and <!-- /hex:notes -->"));
    }

    #[test]
    fn apple_intelligence_writes_short_notes_over_the_small_excerpt() {
        use crate::app_settings::MeetingNotesFormat::{Detailed, Short, Standard};
        assert_eq!(notes_plan(false, Detailed), (Short, FINAL_EXCERPT_CHARS));
        assert_eq!(notes_plan(false, Standard), (Short, FINAL_EXCERPT_CHARS));
        assert_eq!(notes_plan(true, Short), (Short, FINAL_EXCERPT_CHARS));
        assert_eq!(
            notes_plan(true, Detailed),
            (Detailed, DETAILED_EXCERPT_CHARS)
        );
    }

    #[test]
    fn text_without_spaces_is_cut_on_a_character_boundary() {
        let detailed = crate::app_settings::MeetingNotesFormat::Detailed;
        let short = crate::app_settings::MeetingNotesFormat::Short;
        // Japanese and Chinese have no spaces; three bytes per character
        // never land on the byte limits.
        let japanese = "会議の結論".repeat(2_000);
        for summary in [
            clean_summary(&japanese, detailed),
            clean_summary(&japanese, short),
            clean_item(&japanese),
        ] {
            assert!(summary.ends_with('…'));
            assert!(summary.starts_with("会議の結論"));
        }
        assert!(clean_item(&japanese).len() <= MAX_ITEM_CHARS + "…".len());
    }

    #[test]
    fn only_a_leading_list_marker_is_removed() {
        let detailed = crate::app_settings::MeetingNotesFormat::Detailed;
        assert_eq!(clean_item("- Ship it"), "Ship it");
        assert_eq!(clean_item("•Ship it"), "Ship it");
        assert_eq!(clean_item("-15 % on the budget"), "-15 % on the budget");
        assert_eq!(clean_item("**Alice** owns it"), "**Alice** owns it");
        assert_eq!(
            clean_summary("- First point\n- Costs fell -15 %.", detailed),
            "First point Costs fell -15 %."
        );
    }

    #[test]
    fn provisional_titles_use_the_title_prompt_over_the_transcript_so_far() {
        let title = provisional_title(
            &conversation(6),
            5_000,
            Duration::from_secs(5),
            |instructions, prompt, _| {
                assert_eq!(instructions, TITLE_INSTRUCTIONS);
                assert!(prompt.starts_with("Meeting transcript so far:\n\n[00:00] You: Turn 0"));
                Ok("**Launch plan.**".into())
            },
        )
        .unwrap();
        assert_eq!(title, "Launch plan");
        assert!(
            provisional_title(
                &conversation(2),
                5_000,
                Duration::from_secs(5),
                |_, _, _| Ok(" ".into())
            )
            .is_err()
        );
    }

    #[test]
    fn titles_are_requested_after_the_opening_then_once_at_ten_minutes() {
        let progress = |speech_s: u64, words: usize, timeline_s: u64| Progress {
            speech_ms: speech_s * 1_000,
            words,
            timeline_ms: timeline_s * 1_000,
        };
        let mut plan = TitlePlan::default();
        assert!(!plan.due(progress(60, 150, 70)));
        assert!(!plan.due(progress(120, 30, 130)), "too few words");
        assert!(plan.due(progress(90, 200, 100)));
        assert!(plan.due(progress(40, 600, 50)), "600 words suffice");

        plan.record(false, progress(90, 200, 100));
        assert!(!plan.due(progress(120, 260, 150)), "retries a minute later");
        assert!(plan.due(progress(140, 300, 160)));
        plan.record(true, progress(140, 300, 160));
        assert!(!plan.due(progress(500, 2_000, 599)));
        assert!(plan.due(progress(500, 2_000, 600)));
        plan.record(true, progress(500, 2_000, 600));
        assert!(plan.finished());
        assert!(!plan.due(progress(3_000, 9_000, 3_600)));

        let mut failing = TitlePlan::default();
        for minute in 0..3 {
            failing.record(false, progress(100, 300, 100 + minute * 60));
        }
        assert!(failing.finished());

        let entries = conversation(3);
        assert_eq!(
            Progress::of(&entries),
            Progress {
                speech_ms: 3_000,
                words: 30,
                timeline_ms: 21_000
            }
        );
    }

    fn meeting_notes() -> MeetingNotes {
        MeetingNotes {
            title: "Launch plan review".into(),
            summary: "They agreed to ship v2 on Friday.".into(),
            ..Default::default()
        }
    }

    #[test]
    fn detailed_notes_keep_paragraphs_and_unassigned_actions() {
        let mut notes = meeting_notes();
        notes.summary = "Premier paragraphe.\n\nDeuxième paragraphe.".into();
        notes.key_points = vec!["Un point clé".into()];
        notes.action_items = vec![crate::meeting::ActionItem {
            owner: None,
            task: "Faire X".into(),
            due: Some("demain".into()),
            at: Some("01:00".into()),
            quote: None,
        }];
        let block = render_notes_block(&notes);
        assert!(block.contains("Premier paragraphe.\n\nDeuxième paragraphe."));
        assert!(block.contains("### Key points\n\n- Un point clé"));
        assert!(block.contains("### Next steps"));
        assert!(block.contains("**Unassigned** — Faire X (due: demain) [01:00]"));
    }

    #[test]
    fn notes_are_written_in_the_meeting_language() {
        let mut prompts = Vec::new();
        summarize_with_calendar_in(
            &conversation(2),
            None,
            &[],
            Some("fr"),
            crate::app_settings::MeetingNotesFormat::Detailed,
            8_000,
            Instant::now() + Duration::from_secs(30),
            |_, prompt, _| {
                prompts.push(prompt.to_string());
                Ok(generated("Titre", "Résumé.\n\nDeuxième paragraphe."))
            },
        )
        .unwrap();
        assert!(
            prompts[0]
                .starts_with("Meeting language: French (fr). Write every field in this language."),
            "{}",
            prompts[0]
        );
    }

    #[test]
    fn notes_formats_choose_their_fields() {
        use crate::app_settings::MeetingNotesFormat;
        let detailed_notes = || GeneratedNotes {
            title: "Titre".into(),
            summary: "Un. Deux. Trois. Quatre.".into(),
            key_points: vec!["Point".into()],
            decisions: vec!["Décision".into()],
            action_items: vec![crate::meeting::ActionItem {
                owner: Some("You".into()),
                task: "Faire X".into(),
                ..Default::default()
            }],
            open_questions: vec!["Question".into()],
            written_by: None,
        };
        let run = |format| {
            summarize_with_calendar_in(
                &conversation(2),
                None,
                &[],
                Some("fr"),
                format,
                8_000,
                Instant::now() + Duration::from_secs(30),
                |_, _, _| Ok(detailed_notes()),
            )
            .unwrap()
            .notes
        };

        let detailed = run(MeetingNotesFormat::Detailed);
        assert_eq!(detailed.key_points, ["Point"]);
        assert_eq!(detailed.decisions, ["Décision"]);
        assert_eq!(detailed.action_items.len(), 1);
        assert_eq!(detailed.open_questions, ["Question"]);

        let standard = run(MeetingNotesFormat::Standard);
        assert_eq!(standard.key_points, ["Point"]);
        assert_eq!(standard.decisions, ["Décision"]);
        assert!(standard.action_items.is_empty() && standard.open_questions.is_empty());

        let short = run(MeetingNotesFormat::Short);
        assert_eq!(short.summary, "Un. Deux.");
        assert!(
            short.key_points.is_empty()
                && short.decisions.is_empty()
                && short.action_items.is_empty()
                && short.open_questions.is_empty()
        );
    }

    const EXPORT: &str = "# Zoom call — 2:30 PM\n\n- **Date:** Thursday, October 15, 2026, 2:30 PM\n\n## Transcript\n\n**You** · 00:01\nHi.\n\n**Them** · 00:04\nHello.\n\n";

    #[test]
    fn notes_are_inserted_above_the_transcript_and_regenerate_idempotently() {
        let block = render_notes_block(&meeting_notes());
        assert!(block.contains("## Summary\n\nThey agreed to ship v2 on Friday.\n"));
        assert!(!block.contains("Action items") && !block.contains("Decisions"));

        let first = apply_notes(EXPORT, "Zoom call — 2:30 PM", "Launch plan review", &block);
        assert!(first.starts_with("# Launch plan review\n\n- **Date:**"));
        let notes_at = first.find(NOTES_START).unwrap();
        assert!(notes_at < first.find("## Transcript").unwrap());
        assert!(first.ends_with("**Them** · 00:04\nHello.\n\n"));

        let again = apply_notes(&first, "Launch plan review", "Launch plan review", &block);
        assert_eq!(again, first);

        let mut changed = meeting_notes();
        changed.title = "Release sync".into();
        changed.summary = "They moved v2 to next week.".into();
        let regenerated = apply_notes(
            &first,
            "Launch plan review",
            "Release sync",
            &render_notes_block(&changed),
        );
        assert!(regenerated.starts_with("# Release sync\n"));
        assert!(regenerated.contains("They moved v2 to next week.\n"));
        assert!(!regenerated.contains("Friday"));
        assert_eq!(regenerated.matches(NOTES_START).count(), 1);
        assert_eq!(regenerated.matches("## Transcript").count(), 1);
    }

    #[test]
    fn the_notes_footer_names_the_writer_that_wrote_them() {
        let mut notes = meeting_notes();
        assert!(!render_notes_block(&notes).contains("Apple Intelligence"));

        notes.written_by = Some(NotesWriter::AppleIntelligence);
        assert!(render_notes_block(&notes).contains("Written on this Mac by Apple Intelligence"));

        notes.written_by = Some(NotesWriter::OpenCode {
            model: Some("openai/gpt-5".into()),
        });
        let block = render_notes_block(&notes);
        assert!(block.contains("Written by openai/gpt-5 through OpenCode"));
        assert!(!block.contains("Apple Intelligence") && !block.contains("this Mac"));

        // The manifest keeps the writer and model, and older notes still read.
        let json = serde_json::to_value(&notes).unwrap();
        assert_eq!(
            json["written_by"],
            serde_json::json!({"writer": "opencode", "model": "openai/gpt-5"})
        );
        let read: MeetingNotes = serde_json::from_value(json).unwrap();
        assert_eq!(read.written_by, notes.written_by);
        let older: MeetingNotes =
            serde_json::from_value(serde_json::json!({"title": "Launch"})).unwrap();
        assert_eq!(older.written_by, None);
    }

    #[test]
    fn user_edits_outside_the_notes_block_are_preserved() {
        let block = render_notes_block(&meeting_notes());
        let edited = EXPORT
            .replace("# Zoom call — 2:30 PM", "# My own title")
            .replace("Hello.", "Hello, edited.");
        let updated = apply_notes(&edited, "Zoom call — 2:30 PM", "Launch plan review", &block);
        assert!(updated.starts_with("# My own title\n"));
        assert!(updated.contains("Hello, edited."));
        assert!(updated.contains(NOTES_START));

        let without_transcript = "# Notes\n\nSome text";
        let appended = apply_notes(without_transcript, "Other", "Title", &block);
        assert!(appended.starts_with("# Notes\n\nSome text\n\n<!-- hex:notes -->"));
    }

    fn manifest() -> MeetingManifest {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "1",
            "title": "placeholder",
            "status": "complete",
            "created_at_ms": 1_760_000_000_000_u64,
            "ended_at_ms": null,
            "duration_ms": 1_000,
            "system": {"file": "system.wav", "samples": 0, "dropped_packets": 0},
            "microphone": {"file": "microphone.wav", "samples": 0, "dropped_packets": 0},
            "error": null
        }))
        .unwrap()
    }

    #[test]
    fn generated_and_provisional_titles_replace_only_default_titles() {
        let mut manifest = manifest();
        assert!(manifest.notes.is_none() && manifest.provisional_title.is_none());
        manifest.app = Some("Zoom".into());
        manifest.title = super::super::default_title(Some("Zoom"), manifest.created_at_ms);
        assert_eq!(display_title(&manifest), manifest.title);
        manifest.provisional_title = Some("Launch planning".into());
        assert_eq!(display_title(&manifest), "Launch planning");
        manifest.notes = Some(meeting_notes());
        assert_eq!(display_title(&manifest), "Launch plan review");
        manifest.title = "Design sync".into();
        assert_eq!(display_title(&manifest), "Design sync");
    }

    #[test]
    fn older_notes_with_points_action_items_and_decisions_still_read() {
        let notes: MeetingNotes = serde_json::from_value(serde_json::json!({
            "title": "Scheduled sessions",
            "summary": ["Dax proposed scheduled sessions.", " ", "They agreed to try it."],
            "action_items": [{"task": "Write the spec", "owner": "You"}],
            "decisions": ["Ship next week."],
            "generated_at_ms": 1,
            "elapsed_ms": 2,
            "chunks": 1,
            "total_chunks": 3
        }))
        .unwrap();
        assert_eq!(
            notes.summary,
            "Dax proposed scheduled sessions. They agreed to try it."
        );
        assert_eq!(notes.action_items.len(), 1);
        assert_eq!(notes.action_items[0].task, "Write the spec");
        assert_eq!(notes.action_items[0].owner.as_deref(), Some("You"));
        assert_eq!(notes.decisions, ["Ship next week."]);
        assert!(!notes.excerpted());
        let written = serde_json::to_value(&notes).unwrap();
        assert!(written.get("action_items").is_some());
    }

    #[test]
    fn retitling_replaces_only_a_matching_first_heading() {
        let markdown = "# Zoom call — 2:30 PM\n\nText\n# Zoom call — 2:30 PM\n";
        assert_eq!(
            apply_title(markdown, "Zoom call — 2:30 PM", "Launch review"),
            "# Launch review\n\nText\n# Zoom call — 2:30 PM\n"
        );
        assert_eq!(apply_title(markdown, "Other", "Launch review"), markdown);
    }

    #[test]
    fn exported_markdown_parses_back_into_turns() {
        let block = render_notes_block(&meeting_notes());
        let markdown = apply_notes(EXPORT, "Zoom call — 2:30 PM", "Launch", &block);
        let entries = entries_from_markdown(&markdown);
        assert_eq!(
            entries,
            [
                TranscriptEntry {
                    source: MeetingSource::Microphone,
                    start_ms: 1_000,
                    end_ms: 4_000,
                    text: "Hi.".into(),
                    speaker: None,
                },
                TranscriptEntry {
                    source: MeetingSource::System,
                    start_ms: 4_000,
                    end_ms: 4_400,
                    text: "Hello.".into(),
                    speaker: None,
                },
            ]
        );
    }

    #[test]
    fn notes_files_are_rewritten_in_place_only_when_unchanged() {
        let folder = std::env::temp_dir().join(format!(
            "hex-meeting-notes-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("2026-10-15 1430 Zoom call — 2-30 PM.md");
        fs::write(&path, EXPORT).unwrap();
        let block = render_notes_block(&meeting_notes());
        update_markdown_file(
            &path,
            &mut None,
            "Zoom call — 2:30 PM",
            "Launch plan review",
            &block,
        )
        .unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# Launch plan review\n"));
        assert!(written.contains(NOTES_START));
        assert_eq!(fs::read_dir(&folder).unwrap().count(), 1);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn notes_rename_the_export_only_with_descriptive_file_names() {
        let folder = std::env::temp_dir().join(format!(
            "hex-meeting-notes-name-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&folder).unwrap();
        let stamp = super::super::transcript::LocalTime::from_ms(1_760_000_000_000).file_stamp();

        let path = folder.join(format!("{stamp} Zoom call — 2-30 PM.md"));
        fs::write(&path, EXPORT).unwrap();
        let mut changed = manifest();
        changed.export_path = Some(path.clone());
        changed.export_sha256 = Some(super::super::text_sha256(EXPORT.as_bytes()));
        changed.notes = Some(meeting_notes());
        apply_notes_to_export(
            &mut changed,
            "Zoom call — 2:30 PM",
            "Launch plan review",
            true,
        )
        .unwrap();
        let renamed = changed.export_path.clone().unwrap();
        assert_ne!(renamed, path);
        assert!(!path.exists());
        assert_eq!(
            renamed.file_name().unwrap().to_string_lossy(),
            format!("{stamp} Launch plan review.md")
        );
        let written = fs::read_to_string(&renamed).unwrap();
        assert!(written.starts_with("# Launch plan review\n"));
        assert!(written.contains(NOTES_START));
        // The rename keeps the copy claimed for retention at its new path.
        assert_eq!(super::super::file_sha256(&renamed), changed.export_sha256);

        // Off, the notes rewrite the file in place and its name stays.
        let kept = folder.join(format!("{stamp} Old name.md"));
        fs::write(&kept, EXPORT).unwrap();
        let mut unchanged = manifest();
        unchanged.export_path = Some(kept.clone());
        unchanged.notes = Some(meeting_notes());
        apply_notes_to_export(
            &mut unchanged,
            "Zoom call — 2:30 PM",
            "Launch plan review",
            false,
        )
        .unwrap();
        assert_eq!(unchanged.export_path, Some(kept.clone()));
        assert!(kept.exists());
        assert!(fs::read_to_string(&kept).unwrap().contains(NOTES_START));
        fs::remove_dir_all(folder).unwrap();
    }
}
