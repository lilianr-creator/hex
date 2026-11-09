//! `/meeting/presence`, the Google Meet extension's route. It takes the
//! stable presence token, not the local API's per-launch one: GET checks in
//! (even with no meeting recording), POST reports who is speaking in the
//! meeting that records now.

use super::*;

pub(super) const PATH: &str = "/meeting/presence";
/// A report is small; a client that sends it slower than this is gone.
const BODY_TIMEOUT: Duration = Duration::from_secs(2);

/// Answers a presence request once its token is checked, or hands a report
/// to [`ingest`] to read its body.
pub(super) fn route(request: HttpRequest, context: &HttpContext) -> RequestAction {
    // Labs › Meet participant names off: the route isn't there.
    if !crate::labs::meet_participants() {
        return RequestAction::Respond(HttpResponse::code(404, "not-found"));
    }
    let token = request
        .authorization
        .as_deref()
        .and_then(|header| header.strip_prefix("Bearer "));
    let allowed = match (context.presence_token.as_deref(), token) {
        (Some(stored), Some(token)) => crate::meeting::speakers::token_matches(stored, token),
        _ => false,
    };
    if !allowed {
        context
            .auth_limiter
            .observe(&context.events, &request.method, PATH);
        return RequestAction::Respond(HttpResponse::empty(401));
    }
    crate::meeting::speakers::touch(client(&request.path));
    if request.method == "GET" {
        return RequestAction::Respond(HttpResponse::json(
            200,
            &crate::meeting::speakers::status(),
        ));
    }
    if request.method != "POST" {
        return RequestAction::Respond(HttpResponse::code(405, "method-not-allowed"));
    }
    if !request.has_content_type("application/json") {
        return RequestAction::Respond(HttpResponse::code(415, "unsupported-content-type"));
    }
    let Some(content_length) = request.content_length else {
        return RequestAction::Respond(HttpResponse::code(411, "length-required"));
    };
    if content_length > crate::meeting::speakers::MAX_INGEST_BYTES {
        return RequestAction::Respond(HttpResponse::code(413, "presence-too-large"));
    }
    RequestAction::Presence {
        content_length,
        body_prefix: request.body_prefix,
    }
}

/// The `client` query value the extension sends, so two Chrome profiles count
/// as two connections. Values are opaque ids, so no decoding is needed.
fn client(path: &str) -> Option<&str> {
    let (_, query) = path.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "client").then_some(value)
    })
}

/// `POST /meeting/presence`: the browser extension's roster and speaking
/// intervals for the meeting that records now.
pub(super) fn ingest(
    stream: &mut TcpStream,
    content_length: usize,
    mut body: Vec<u8>,
    shutdown: &AtomicBool,
) -> HttpResponse {
    if let Err(error) = read_body_bounded(
        stream,
        content_length,
        &mut body,
        shutdown,
        crate::meeting::speakers::MAX_INGEST_BYTES,
        BODY_TIMEOUT,
    ) {
        return body_read_response(error);
    }
    match crate::meeting::speakers::ingest(&body) {
        Ok(reply) => HttpResponse::json(200, &reply),
        Err(crate::meeting::speakers::IngestError::NotRecording) => {
            HttpResponse::code(409, "presence-not-recording")
        }
        Err(crate::meeting::speakers::IngestError::Invalid) => {
            HttpResponse::code(400, "invalid-presence")
        }
        Err(crate::meeting::speakers::IngestError::OtherMeeting) => {
            HttpResponse::code(409, "presence-other-meeting")
        }
        Err(crate::meeting::speakers::IngestError::Storage) => {
            HttpResponse::code(503, "presence-unavailable")
        }
    }
}
