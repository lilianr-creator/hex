//! A finished meeting's notes in the pane: what the host hands over, and
//! the summary and list sections above the transcript.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::rune_ui) struct NotesView {
    pub(in crate::rune_ui) summary: String,
    pub(in crate::rune_ui) key_points: Vec<String>,
    pub(in crate::rune_ui) decisions: Vec<String>,
    pub(in crate::rune_ui) action_items: Vec<ActionItemView>,
    pub(in crate::rune_ui) open_questions: Vec<String>,
}

impl NotesView {
    pub(in crate::rune_ui) fn plain_text(&self) -> String {
        let mut parts = vec![self.summary.clone()];
        parts.extend(self.key_points.iter().cloned());
        parts.extend(self.decisions.iter().cloned());
        parts.extend(
            self.action_items
                .iter()
                .map(|item| item.line(tr("Unassigned"), tr("due:"))),
        );
        parts.extend(self.open_questions.iter().cloned());
        parts
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub(in crate::rune_ui) type ActionItemView = crate::meeting::ActionItem;

/// The notes above a meeting's transcript: the summary, then each list
/// section that has items.
pub(super) fn notes_block(p: Paint, notes: &NotesView) -> El {
    let summary = notes.summary.trim();
    let mut children = vec![
        section_label(p, "summary-label", tr("Summary")),
        text(if summary.is_empty() {
            tr("Nothing to summarize.")
        } else {
            summary
        })
        .key("summary")
        .font(p.k.ty.body)
        .color(if summary.is_empty() {
            p.faint()
        } else {
            p.text()
        })
        .wrap(),
    ];
    push_notes_list(
        &mut children,
        p,
        "key-points",
        tr("Key points"),
        &notes.key_points,
    );
    push_notes_list(
        &mut children,
        p,
        "decisions",
        tr("Decisions"),
        &notes.decisions,
    );
    if !notes.action_items.is_empty() {
        let lines = notes
            .action_items
            .iter()
            .map(|item| format!("• {}", item.line(tr("Unassigned"), tr("due:"))))
            .collect::<Vec<_>>()
            .join("\n");
        children.push(section_label(p, "next-steps-label", tr("Next steps")));
        children.push(
            text(lines)
                .key("next-steps")
                .font(DESCRIPTION)
                .color(p.muted())
                .wrap(),
        );
    }
    push_notes_list(
        &mut children,
        p,
        "open-questions",
        tr("Open questions"),
        &notes.open_questions,
    );
    col().key("notes").gap(10.0).children(children)
}

fn push_notes_list(
    children: &mut Vec<El>,
    p: Paint,
    key: &'static str,
    label: &'static str,
    items: &[String],
) {
    if items.is_empty() {
        return;
    }
    let lines = items
        .iter()
        .map(|item| format!("• {}", item.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    children.push(section_label(p, &format!("{key}-label"), label));
    children.push(
        text(lines)
            .key(key)
            .font(DESCRIPTION)
            .color(p.muted())
            .wrap(),
    );
}
