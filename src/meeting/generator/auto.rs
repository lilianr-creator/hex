//! Auto, the notes model when none is chosen: a rule over OpenCode's catalog
//! rather than a list of model names, so it follows new models without a Hex
//! release.
//!
//! 1. Candidates are active text models whose ID names a fast tier (flash,
//!    lite, mini, nano, haiku) with room for the longest excerpt. Free tiers,
//!    previews, and models made for code, vision, or audio are left out.
//! 2. Managed OpenCode (`opencode`, then `opencode-go`) comes before any
//!    other connection, so notes never spend another account's credits while
//!    OpenCode's own can answer.
//! 3. Within a provider, the newest release comes first, then the cheapest
//!    output.
//!
//! A generation tries the first [`CANDIDATES`] in turn within one deadline.

use std::cmp::Ordering;

use crate::dictation::processor::{ModelCatalog, ModelChoice};

/// How many ranked models one generation tries before it gives up.
pub(crate) const CANDIDATES: usize = 3;
/// Room for the longest excerpt, 48,000 characters that can each be a token
/// in Chinese or Japanese, and the answer.
const MIN_CONTEXT_TOKENS: u64 = 64_000;
const FAST_TIERS: &[&str] = &["flash", "lite", "mini", "nano", "haiku"];
const LEFT_OUT: &[&str] = &[
    "free",
    "preview",
    "exp",
    "experimental",
    "beta",
    "vision",
    "vl",
    "audio",
    "omni",
    "image",
    "tts",
    "realtime",
    "search",
    "embedding",
    "code",
    "coder",
    "codex",
];

/// The catalog's Auto candidates, best first.
pub(crate) fn ranked(catalog: &ModelCatalog) -> Vec<&ModelChoice> {
    let mut ranked: Vec<&ModelChoice> = catalog
        .models
        .iter()
        .filter(|choice| eligible(choice))
        .collect();
    ranked.sort_by(|a, b| {
        provider_rank(&a.provider)
            .cmp(&provider_rank(&b.provider))
            // Newest first; an undated model after every dated one.
            .then_with(|| b.released.cmp(&a.released))
            .then_with(|| cheaper_first(a.output_cost, b.output_cost))
            .then_with(|| a.key.cmp(&b.key))
    });
    ranked
}

fn eligible(choice: &ModelChoice) -> bool {
    let leaf = choice
        .key
        .rsplit('/')
        .next()
        .unwrap_or(&choice.key)
        .to_ascii_lowercase();
    let words: Vec<&str> = leaf
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    choice.text
        && choice.active
        && choice
            .context
            .is_none_or(|context| context >= MIN_CONTEXT_TOKENS)
        && words.iter().any(|word| FAST_TIERS.contains(word))
        && !words.iter().any(|word| LEFT_OUT.contains(word))
}

fn provider_rank(provider: &str) -> u8 {
    match provider {
        "opencode" => 0,
        "opencode-go" => 1,
        _ => 2,
    }
}

/// A priced model before an unpriced one.
fn cheaper_first(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 86_400_000;

    fn model(key: &str, released_day: Option<u64>, output_cost: Option<f64>) -> ModelChoice {
        ModelChoice {
            key: key.into(),
            name: key.into(),
            provider: key.split_once('/').unwrap().0.into(),
            variants: Vec::new(),
            text: true,
            context: Some(1_000_000),
            released: released_day.map(|day| day * DAY_MS),
            output_cost,
            active: true,
        }
    }

    fn keys(models: Vec<ModelChoice>) -> Vec<String> {
        let catalog = ModelCatalog {
            models,
            default_key: None,
            default_name: None,
            default_variant: None,
        };
        ranked(&catalog)
            .into_iter()
            .map(|choice| choice.key.clone())
            .collect()
    }

    #[test]
    fn auto_keeps_fast_tiers_and_leaves_out_free_previews_and_specialists() {
        let ranked = keys(vec![
            model("opencode/claude-haiku-5-5", Some(10), Some(0.5)),
            model("opencode/gemini-3.5-flash-lite", Some(9), Some(2.5)),
            model("opencode/gpt-5.4-nano", Some(8), Some(1.25)),
            model("opencode/claude-sonnet-5-5", Some(11), Some(15.0)),
            model("opencode/gemini-4-argon", Some(11), Some(10.0)),
            model("opencode/deepseek-v4.1-flash-free", Some(11), Some(0.0)),
            model("opencode/gemini-3-flash-preview", Some(11), Some(3.0)),
            model("opencode/deepseek-v4-flash-vision-exp", Some(11), Some(1.0)),
            model("opencode/gpt-5.1-codex-mini", Some(11), Some(2.0)),
            model("opencode/gpt-audio-mini", Some(11), Some(2.4)),
            model("opencode/glm-5.3-flashx", Some(11), Some(1.25)),
        ]);
        assert_eq!(
            ranked,
            [
                "opencode/claude-haiku-5-5",
                "opencode/gemini-3.5-flash-lite",
                "opencode/gpt-5.4-nano",
            ]
        );
    }

    #[test]
    fn auto_prefers_managed_opencode_over_newer_models_elsewhere() {
        let ranked = keys(vec![
            model(
                "openrouter/~deepseek/deepseek-flash-latest",
                Some(20),
                Some(0.5),
            ),
            model("kiro/deepseek-v4.1-flash", None, None),
            model("opencode-go/deepseek-v4.1-flash", Some(10), Some(0.6)),
            model("opencode/deepseek-v4.1-flash", Some(10), Some(1.2)),
        ]);
        assert_eq!(
            ranked,
            [
                "opencode/deepseek-v4.1-flash",
                "opencode-go/deepseek-v4.1-flash",
                "openrouter/~deepseek/deepseek-flash-latest",
                "kiro/deepseek-v4.1-flash",
            ]
        );
    }

    #[test]
    fn auto_puts_the_newest_first_then_the_cheapest() {
        let ranked = keys(vec![
            model("opencode/qwen3.8-flash", Some(5), Some(0.47)),
            model("opencode/glm-5.3-flash", Some(5), Some(0.5)),
            model("opencode/minimax-m3.1-flash", Some(7), Some(0.6)),
            model("opencode/mimo-flash", None, Some(0.1)),
        ]);
        assert_eq!(
            ranked,
            [
                "opencode/minimax-m3.1-flash",
                "opencode/qwen3.8-flash",
                "opencode/glm-5.3-flash",
                "opencode/mimo-flash",
            ]
        );
    }

    #[test]
    fn auto_skips_inactive_small_context_and_non_text_models() {
        let mut deprecated = model("opencode/gemini-2.5-flash", Some(9), Some(2.5));
        deprecated.active = false;
        let mut small = model("opencode/ministral-3b-mini", Some(9), Some(0.1));
        small.context = Some(32_000);
        let mut speech = model("opencode/flash-speech", Some(9), Some(0.1));
        speech.text = false;
        let unknown_context = ModelChoice {
            context: None,
            ..model("opencode/qwen3.8-flash", Some(8), Some(0.47))
        };
        assert_eq!(
            keys(vec![deprecated, small, speech, unknown_context]),
            ["opencode/qwen3.8-flash"]
        );
    }
}
