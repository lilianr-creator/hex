//! The notes as Markdown: the block between Hex's markers in the exported
//! copy, the same sections for copying, and the in-place updates that touch
//! only Hex's heading and block.

use super::*;

/// The notes block of the Markdown copy, between Hex's markers, with a line
/// that says who wrote it.
pub(super) fn render_notes_block(notes: &MeetingNotes) -> String {
    format!(
        "{NOTES_START}\n{}\n\n_{}_\n{NOTES_END}\n",
        notes_body(notes),
        notes_footer(notes.written_by.as_ref()),
    )
}

/// The line under the notes, naming the writer that actually wrote them.
fn notes_footer(writer: Option<&NotesWriter>) -> String {
    const DETAILS: &str = "the transcript below has the details.";
    match writer {
        Some(NotesWriter::AppleIntelligence) => {
            format!("Written on this Mac by Apple Intelligence; {DETAILS}")
        }
        Some(NotesWriter::OpenCode { model: Some(model) }) => {
            format!(
                "Written by {} through OpenCode; {DETAILS}",
                markdown_text(model)
            )
        }
        Some(NotesWriter::OpenCode { model: None }) => {
            format!("Written by OpenCode's default model; {DETAILS}")
        }
        None => "The transcript below has the details.".into(),
    }
}

/// The notes as Markdown sections, without markers or footer: `## Summary`,
/// then Key points, Decisions, Next steps, and Open questions when present.
/// Model text is made inert, so a participant named `<!--` or a line that
/// reads like Hex's own marker cannot hide or cut the rest of the file.
pub(crate) fn notes_body(notes: &MeetingNotes) -> String {
    let summary = inert(notes.summary.trim());
    let mut block = String::from("## Summary\n\n");
    if summary.is_empty() {
        block.push_str("_Nothing to summarize._");
    } else {
        block.push_str(&summary);
    }
    block.push_str("\n\n");
    push_notes_section(&mut block, "Key points", &notes.key_points);
    push_notes_section(&mut block, "Decisions", &notes.decisions);
    if !notes.action_items.is_empty() {
        block.push_str("### Next steps\n\n");
        for item in &notes.action_items {
            block.push_str("- ");
            match item
                .owner
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
            {
                Some(owner) => {
                    block.push_str(&format!("**{}** — ", markdown_text(owner.trim())));
                }
                None => block.push_str("**Unassigned** — "),
            }
            block.push_str(&inert(item.task.trim()));
            if let Some(due) = item.due.as_deref().filter(|due| !due.trim().is_empty()) {
                block.push_str(&format!(" (due: {})", inert(due.trim())));
            }
            if let Some(at) = item.at.as_deref().filter(|at| !at.trim().is_empty()) {
                block.push_str(&format!(" [{}]", inert(at.trim())));
            }
            block.push('\n');
        }
        block.push('\n');
    }
    push_notes_section(&mut block, "Open questions", &notes.open_questions);
    block.trim_end().to_owned()
}

pub(super) fn push_notes_section(block: &mut String, title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    block.push_str(&format!("### {title}\n\n"));
    for line in lines {
        block.push_str("- ");
        block.push_str(&inert(line));
        block.push('\n');
    }
    block.push('\n');
}

/// Model text with `<` escaped, which Markdown still shows as `<` but never
/// reads as an HTML comment or tag.
pub(super) fn inert(text: &str) -> String {
    text.replace('<', "&lt;")
}

/// Text from outside Hex, such as a name a participant chose in Meet, with
/// every character Markdown reads as markup escaped.
pub(crate) fn markdown_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\\' | '*' | '_' | '[' | ']' | '`' | '#' | '|' | '!' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '\n' | '\r' => escaped.push(' '),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Rewrites only Hex's parts of the Markdown: the heading when it still
/// reads `previous_title`, and the notes block between its markers (inserted
/// above `## Transcript` the first time). Everything else is preserved.
pub(super) fn apply_notes(
    markdown: &str,
    previous_title: &str,
    title: &str,
    block: &str,
) -> String {
    let mut output = String::with_capacity(markdown.len() + block.len());
    let mut lines = markdown.split_inclusive('\n').peekable();
    let mut heading_seen = false;
    let mut inserted = false;
    // Whole lines only: a marker inside a transcript turn is not Hex's.
    let bare_lines = || markdown.lines().map(|line| line.trim_end_matches('\r'));
    let has_markers =
        bare_lines().any(|line| line == NOTES_START) && bare_lines().any(|line| line == NOTES_END);
    while let Some(line) = lines.next() {
        let bare = line.trim_end_matches(['\n', '\r']);
        if !heading_seen && bare.starts_with("# ") {
            heading_seen = true;
            if bare[2..].trim() == previous_title.trim() {
                output.push_str(&format!("# {}\n", title.trim()));
                continue;
            }
        }
        if !inserted && has_markers && bare == NOTES_START {
            for skipped in lines.by_ref() {
                if skipped.trim_end_matches(['\n', '\r']) == NOTES_END {
                    break;
                }
            }
            output.push_str(block);
            inserted = true;
            continue;
        }
        if !inserted && !has_markers && bare == "## Transcript" {
            output.push_str(block);
            output.push('\n');
            inserted = true;
        }
        output.push_str(line);
    }
    if !inserted {
        if !output.is_empty() && !output.ends_with("\n\n") {
            output.push_str(if output.ends_with('\n') { "\n" } else { "\n\n" });
        }
        output.push_str(block);
    }
    output
}

/// Replaces the first heading only while it still reads `previous_title`.
pub(super) fn apply_title(markdown: &str, previous_title: &str, title: &str) -> String {
    let mut output = String::with_capacity(markdown.len() + title.len());
    let mut heading_seen = false;
    for line in markdown.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if !heading_seen && bare.starts_with("# ") {
            heading_seen = true;
            if bare[2..].trim() == previous_title.trim() {
                output.push_str(&format!("# {}\n", title.trim()));
                continue;
            }
        }
        output.push_str(line);
    }
    output
}
