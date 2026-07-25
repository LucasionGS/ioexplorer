//! Line parser for Claude's server-sent-event stream.
//!
//! Pure over `&str` — no I/O, no allocation beyond the returned event — so the
//! whole protocol surface is unit-testable without a network or a main loop.
//!
//! Unknown event and content-block types deliberately return [`SseEvent::Ignored`]
//! rather than an error: a content-block type added to the API in future must not
//! break an in-flight stream.

/// One decoded `data:` payload from `POST /v1/messages` with `stream: true`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SseEvent {
    MessageStart,
    /// A `thinking` content block opened. Under the default
    /// `thinking.display: "omitted"` its deltas carry no text, so this is the
    /// only reliable "reasoning, no output yet" signal.
    ThinkingStart,
    TextStart,
    TextDelta(String),
    /// Text is empty whenever `display` is `"omitted"` (the default). Kept
    /// distinct from [`SseEvent::TextDelta`] so it can never reach the transcript.
    ThinkingDelta(String),
    BlockStop,
    /// Carries the terminal `stop_reason` and, on a refusal, its category.
    MessageDelta {
        stop_reason: Option<String>,
        refusal_category: Option<String>,
    },
    MessageStop,
    /// An `error` event inside the stream body (distinct from an HTTP error).
    Error {
        kind: String,
        message: String,
    },
    Ping,
    /// A recognised but non-actionable line: `event:` lines, comments, blanks,
    /// and any type this parser does not model.
    Ignored,
}

/// Parses exactly one line of the SSE stream. Never panics.
pub fn parse_line(line: &str) -> SseEvent {
    let line = line.trim_end_matches(['\r', '\n']);

    // `event:` lines duplicate the `type` field inside the payload, and a
    // leading `:` is an SSE comment. Only `data:` carries anything we need.
    let Some(payload) = line.strip_prefix("data:") else {
        return SseEvent::Ignored;
    };

    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload.trim()) else {
        // A truncated line is not fatal — the terminal event or the reader's
        // own error handles a genuinely broken stream.
        return SseEvent::Ignored;
    };

    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("message_start") => SseEvent::MessageStart,
        Some("content_block_start") => match block_type(&value) {
            Some("thinking") => SseEvent::ThinkingStart,
            Some("text") => SseEvent::TextStart,
            _ => SseEvent::Ignored,
        },
        Some("content_block_delta") => parse_delta(&value),
        Some("content_block_stop") => SseEvent::BlockStop,
        Some("message_delta") => SseEvent::MessageDelta {
            stop_reason: string_at(&value, &["delta", "stop_reason"]),
            // `stop_details` is null on most refusals — never branch on it.
            refusal_category: string_at(&value, &["delta", "stop_details", "category"])
                .or_else(|| string_at(&value, &["stop_details", "category"])),
        },
        Some("message_stop") => SseEvent::MessageStop,
        Some("ping") => SseEvent::Ping,
        Some("error") => SseEvent::Error {
            kind: string_at(&value, &["error", "type"]).unwrap_or_else(|| "error".to_string()),
            message: string_at(&value, &["error", "message"])
                .unwrap_or_else(|| "unknown error".to_string()),
        },
        _ => SseEvent::Ignored,
    }
}

fn parse_delta(value: &serde_json::Value) -> SseEvent {
    match value
        .get("delta")
        .and_then(|delta| delta.get("type"))
        .and_then(serde_json::Value::as_str)
    {
        Some("text_delta") => {
            SseEvent::TextDelta(string_at(value, &["delta", "text"]).unwrap_or_default())
        }
        Some("thinking_delta") => {
            SseEvent::ThinkingDelta(string_at(value, &["delta", "thinking"]).unwrap_or_default())
        }
        // `signature_delta`, `input_json_delta` (phase 2) and anything newer.
        _ => SseEvent::Ignored,
    }
}

fn block_type(value: &serde_json::Value) -> Option<&str> {
    value
        .get("content_block")
        .and_then(|block| block.get("type"))
        .and_then(serde_json::Value::as_str)
}

/// Reads a nested string, returning `None` for a missing or non-string node.
fn string_at(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut node = value;
    for key in path {
        node = node.get(key)?;
    }
    node.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_event_lines_comments_and_blanks() {
        assert_eq!(parse_line("event: content_block_delta"), SseEvent::Ignored);
        assert_eq!(parse_line(": heartbeat"), SseEvent::Ignored);
        assert_eq!(parse_line(""), SseEvent::Ignored);
        assert_eq!(parse_line("   "), SseEvent::Ignored);
    }

    #[test]
    fn parses_message_lifecycle_events() {
        assert_eq!(
            parse_line(r#"data: {"type":"message_start","message":{"id":"msg_1"}}"#),
            SseEvent::MessageStart
        );
        assert_eq!(
            parse_line(r#"data: {"type":"content_block_stop","index":0}"#),
            SseEvent::BlockStop
        );
        assert_eq!(
            parse_line(r#"data: {"type":"message_stop"}"#),
            SseEvent::MessageStop
        );
        assert_eq!(parse_line(r#"data: {"type":"ping"}"#), SseEvent::Ping);
    }

    #[test]
    fn parses_text_blocks_and_deltas() {
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#
            ),
            SseEvent::TextStart
        );
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#
            ),
            SseEvent::TextDelta("Hello".to_string())
        );
    }

    #[test]
    fn thinking_deltas_are_never_text() {
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#
            ),
            SseEvent::ThinkingStart
        );
        // The Opus 5 default is `display: "omitted"`, so the text is empty.
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#
            ),
            SseEvent::ThinkingDelta(String::new())
        );
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing"}}"#
            ),
            SseEvent::ThinkingDelta("weighing".to_string())
        );
    }

    #[test]
    fn signature_deltas_are_ignored() {
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#
            ),
            SseEvent::Ignored
        );
    }

    #[test]
    fn extracts_stop_reason_from_message_delta() {
        assert_eq!(
            parse_line(r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#),
            SseEvent::MessageDelta {
                stop_reason: Some("end_turn".to_string()),
                refusal_category: None,
            }
        );
    }

    #[test]
    fn handles_a_refusal_with_null_stop_details() {
        assert_eq!(
            parse_line(
                r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":null}}"#
            ),
            SseEvent::MessageDelta {
                stop_reason: Some("refusal".to_string()),
                refusal_category: None,
            }
        );
    }

    #[test]
    fn extracts_a_refusal_category_when_present() {
        assert_eq!(
            parse_line(
                r#"data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber"}}}"#
            ),
            SseEvent::MessageDelta {
                stop_reason: Some("refusal".to_string()),
                refusal_category: Some("cyber".to_string()),
            }
        );
    }

    #[test]
    fn parses_in_stream_error_events() {
        assert_eq!(
            parse_line(
                r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            SseEvent::Error {
                kind: "overloaded_error".to_string(),
                message: "Overloaded".to_string(),
            }
        );
    }

    #[test]
    fn unknown_types_are_ignored_not_fatal() {
        assert_eq!(
            parse_line(r#"data: {"type":"some_future_event","payload":{}}"#),
            SseEvent::Ignored
        );
        assert_eq!(
            parse_line(
                r#"data: {"type":"content_block_start","content_block":{"type":"tool_use","id":"t1"}}"#
            ),
            SseEvent::Ignored
        );
    }

    #[test]
    fn truncated_json_is_ignored_not_fatal() {
        assert_eq!(
            parse_line(r#"data: {"type":"content_block_delta","delta":{"type":"tex"#),
            SseEvent::Ignored
        );
        assert_eq!(parse_line("data: not json at all"), SseEvent::Ignored);
    }

    #[test]
    fn tolerates_crlf_and_a_missing_space_after_data() {
        assert_eq!(
            parse_line("data:{\"type\":\"message_stop\"}\r"),
            SseEvent::MessageStop
        );
    }
}
