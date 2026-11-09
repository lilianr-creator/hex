//! Cleaning what a notes model answers: lenient field reading, titles,
//! summaries in each format, list items, and action items, each capped.

use serde::{Deserialize, Deserializer};

use super::*;

/// A string, a number written as text, or nothing for `null` and anything
/// else.
pub(super) fn loose_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(
        match Option::<serde_json::Value>::deserialize(deserializer)? {
            Some(serde_json::Value::String(text)) => Some(text),
            Some(serde_json::Value::Number(number)) => Some(number.to_string()),
            _ => None,
        },
    )
}

/// A string, or empty for `null`.
pub(crate) fn text_or_null<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(loose_text(deserializer)?.unwrap_or_default())
}

/// A list, or empty for `null`.
pub(crate) fn list_or_null<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

pub(crate) fn summary_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Summary {
        Text(String),
        Points(Vec<String>),
    }
    Ok(match Option::<Summary>::deserialize(deserializer)? {
        Some(Summary::Text(text)) => text,
        Some(Summary::Points(points)) => points
            .iter()
            .map(|point| point.trim())
            .filter(|point| !point.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        None => String::new(),
    })
}

pub(crate) fn clean_title(title: &str) -> String {
    let words = title
        .trim()
        .trim_matches(['"', '\'', '“', '”', '#', '*'])
        .split_whitespace()
        .take(MAX_TITLE_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    words.trim_end_matches(['.', ':', ';', ',']).to_string()
}

/// Up to `MAX_SUMMARY_PARAGRAPHS` paragraphs, each with its words collapsed,
/// and `MAX_SUMMARY_CHARS` in all.
pub(crate) fn clean_summary(
    summary: &str,
    format: crate::app_settings::MeetingNotesFormat,
) -> String {
    if format == crate::app_settings::MeetingNotesFormat::Short {
        return clean_short_summary(summary);
    }
    let text = summary
        .split("\n\n")
        .map(|paragraph| {
            paragraph
                .lines()
                .map(without_bullet)
                .flat_map(str::split_whitespace)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|paragraph| !paragraph.is_empty())
        .take(MAX_SUMMARY_PARAGRAPHS)
        .collect::<Vec<_>>()
        .join("\n\n");
    // Guided generation sometimes leaks a closing brace from its JSON.
    let text = text
        .trim_start_matches(['{', '['])
        .trim_end_matches(['}', ']'])
        .trim();
    truncate_at_word(text, MAX_SUMMARY_CHARS)
}

/// One paragraph of at most two sentences and `MAX_SHORT_SUMMARY_CHARS`.
pub(super) fn clean_short_summary(summary: &str) -> String {
    let text = summary
        .lines()
        .map(without_bullet)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    let text = text
        .trim_start_matches(['{', '['])
        .trim_end_matches(['}', ']'])
        .trim();
    let mut end = text.len();
    let mut sentences = 0;
    for (index, character) in text.char_indices() {
        if matches!(character, '.' | '!' | '?')
            && text[index + 1..].starts_with(|next: char| next.is_whitespace())
        {
            sentences += 1;
            if sentences == MAX_SHORT_SUMMARY_SENTENCES {
                end = index + 1;
                break;
            }
        }
    }
    truncate_at_word(&text[..end], MAX_SHORT_SUMMARY_CHARS)
}

/// Trimmed, non-empty items, at most `max`, each cleaned and capped.
pub(super) fn clean_list(items: Vec<String>, max: usize) -> Vec<String> {
    items
        .into_iter()
        .map(|item| clean_item(&item))
        .filter(|item| !item.is_empty())
        .take(max)
        .collect()
}

pub(super) fn clean_item(item: &str) -> String {
    let text = without_bullet(item)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_at_word(&text, MAX_ITEM_CHARS)
}

/// A line without the list marker a model sometimes starts it with. Only a
/// marker followed by a space counts, so "-15 %" and "**Alice**" keep theirs.
pub(super) fn without_bullet(line: &str) -> &str {
    let line = line.trim();
    ["- ", "* ", "• ", "•"]
        .into_iter()
        .find_map(|marker| line.strip_prefix(marker))
        .map_or(line, str::trim_start)
}

/// `text` within `max` bytes, cut at the last space before the limit (or at
/// a character boundary when there is none, as in Chinese or Japanese).
pub(super) fn truncate_at_word(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let head = &text[..text.floor_char_boundary(max)];
    let cut = head.rfind(' ').unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end_matches([',', ';', ':']))
}

pub(super) fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| clean_item(&value))
        .filter(|value| !value.is_empty())
}

pub(super) fn clean_action_items(
    items: Vec<crate::meeting::ActionItem>,
) -> Vec<crate::meeting::ActionItem> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter_map(|item| {
            let task = clean_item(&item.task);
            if task.is_empty() {
                return None;
            }
            let owner = clean_optional(item.owner);
            let key = (owner.clone(), task.clone());
            if !seen.insert(key) {
                return None;
            }
            Some(crate::meeting::ActionItem {
                owner,
                task,
                due: clean_optional(item.due),
                at: clean_optional(item.at),
                quote: clean_optional(item.quote),
            })
        })
        .take(MAX_ACTIONS)
        .collect()
}
