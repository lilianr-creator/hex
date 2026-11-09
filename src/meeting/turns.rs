//! Transcript turns: one speaker's text over a span, and how consecutive
//! entries grow into turns.

use serde::{Deserialize, Serialize};

/// Entries of one source this close together are the same turn.
const TRANSCRIPT_TURN_GAP_MS: u64 = 3_000;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct TranscriptEntry {
    pub source: MeetingSource,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// Who said it, when the browser extension reported the call's speakers.
    /// Absent leaves the source's `You` / `Them` label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

impl TranscriptEntry {
    /// The name to show for this turn: the reported participant, else
    /// `You` or `Them`.
    pub fn label(&self) -> &str {
        self.speaker
            .as_deref()
            .unwrap_or_else(|| self.source.label())
    }
}

/// Joins consecutive entries of one source and one speaker, then closes a
/// turn at a pause longer than [`TRANSCRIPT_TURN_GAP_MS`].
pub fn coalesce_transcript(
    entries: impl IntoIterator<Item = TranscriptEntry>,
) -> Vec<TranscriptEntry> {
    let mut turns: Vec<TranscriptEntry> = Vec::new();
    for entry in entries {
        if let Some(turn) = turns.last_mut()
            && turn.source == entry.source
            && turn.speaker == entry.speaker
            && entry.start_ms.saturating_sub(turn.end_ms) <= TRANSCRIPT_TURN_GAP_MS
        {
            if !turn.text.ends_with(char::is_whitespace) {
                turn.text.push(' ');
            }
            turn.text.push_str(entry.text.trim());
            turn.end_ms = turn.end_ms.max(entry.end_ms);
        } else {
            turns.push(entry);
        }
    }
    turns
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeetingSource {
    System,
    Microphone,
}

impl MeetingSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Them",
            Self::Microphone => "You",
        }
    }
}
