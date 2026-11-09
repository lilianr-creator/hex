//! The language a meeting's chunks run in: Meetings › Language when the
//! model can take it per run, else the dictation selection's, snapshotted in
//! the manifest so a meeting finished later runs in the same one.

/// Meetings › Language while Labs › Meeting language is on; `None` follows
/// dictation.
pub(super) fn requested_language() -> Option<String> {
    crate::labs::meeting_language()
        .then(crate::app_settings::meeting_language)
        .flatten()
}

/// The language a meeting records in: the user's Meetings › Language when
/// the selected model can take that hint, otherwise the dictation selection's
/// own language. The second value says whether the language was pinned.
pub(super) fn pinned_language(
    model: &crate::transcription::models::ModelDefinition,
    selection: &crate::transcription::models::TranscriptionSelection,
    requested: Option<&str>,
) -> (String, bool) {
    let Some(requested) = requested else {
        return (selection.language.clone(), false);
    };
    if model.takes_meeting_language(requested) {
        (
            requested.to_string(),
            requested != crate::transcription::models::AUTO_LANGUAGE,
        )
    } else {
        (selection.language.clone(), false)
    }
}

/// Reuses a finished recording's snapshotted language when the model that
/// finishes it can take that code per run. Apple Speech keeps the locale it
/// is installed for, so a pending meeting never waits on another locale.
pub(super) fn apply_manifest_language(
    selection: &mut crate::transcription::models::TranscriptionSelection,
    language: Option<&str>,
) {
    let model = crate::transcription::models::definition(selection.model);
    if let Some(language) = language
        && crate::transcription::models::is_known_language(language)
        && model.supports_language(language)
        && !matches!(
            model.runtime,
            crate::transcription::models::ModelRuntime::AppleSpeech
        )
    {
        selection.language = language.to_string();
    }
}
