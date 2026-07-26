//! Line parser for Ollama's newline-delimited JSON stream.
//!
//! Pure over `&str`, same contract as [`super::sse`]: unknown shapes are
//! [`NdjsonEvent::Ignored`] rather than errors.

/// One decoded line from `POST {endpoint}/api/chat` with `stream: true`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NdjsonEvent {
    Delta(String),
    /// Tool calls, which Ollama returns whole in the message object rather than
    /// streaming incrementally the way Claude does — so there is nothing to
    /// accumulate and no `input_json_delta` equivalent.
    ToolCalls(Vec<ToolCallJson>),
    /// The final object. Any trailing content it carries is reported as a
    /// [`NdjsonEvent::Delta`] first, so callers never lose the last fragment.
    Done,
    Error(String),
    Ignored,
}

/// One `tool_calls` entry, before it becomes a provider-neutral call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallJson {
    pub name: String,
    /// Ollama sends `arguments` already parsed, not as a JSON string.
    pub arguments: String,
}

/// Parses exactly one NDJSON line. Never panics.
///
/// Returns up to two events because Ollama's terminal object may still carry
/// content: `{"message":{"content":"!"},"done":true}`.
pub fn parse_line(line: &str) -> Vec<NdjsonEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![NdjsonEvent::Ignored];
    };

    // Ollama reports failures as a bare `{"error": "..."}` with HTTP 200.
    if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
        return vec![NdjsonEvent::Error(error.to_string())];
    }

    let mut events = Vec::new();
    if let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        && !content.is_empty()
    {
        events.push(NdjsonEvent::Delta(content.to_string()));
    }

    if let Some(calls) = parse_tool_calls(&value) {
        events.push(NdjsonEvent::ToolCalls(calls));
    }

    if value
        .get("done")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        events.push(NdjsonEvent::Done);
    }

    if events.is_empty() {
        events.push(NdjsonEvent::Ignored);
    }
    events
}

/// Reads `message.tool_calls`, dropping entries with no function name — a call
/// that cannot be dispatched is worse than no call at all.
fn parse_tool_calls(value: &serde_json::Value) -> Option<Vec<ToolCallJson>> {
    let raw = value.get("message")?.get("tool_calls")?.as_array()?;

    let calls = raw
        .iter()
        .filter_map(|call| {
            let function = call.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            if name.is_empty() {
                return None;
            }
            // `arguments` is a JSON object here, unlike Claude's streamed string.
            let arguments = function
                .get("arguments")
                .map(serde_json::Value::to_string)
                .unwrap_or_else(|| "{}".to_string());
            Some(ToolCallJson { name, arguments })
        })
        .collect::<Vec<_>>();

    match calls.is_empty() {
        true => None,
        false => Some(calls),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_calls_from_the_message_object() {
        let events = parse_line(
            r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"calculate","arguments":{"expression":"2+2"}}}]},"done":false}"#,
        );

        assert_eq!(
            events,
            vec![NdjsonEvent::ToolCalls(vec![ToolCallJson {
                name: "calculate".to_string(),
                arguments: r#"{"expression":"2+2"}"#.to_string(),
            }])]
        );
    }

    /// Unlike Claude, Ollama sends the whole call at once — there is no
    /// fragment accumulation, and a missing `arguments` means no arguments.
    #[test]
    fn a_tool_call_without_arguments_becomes_an_empty_object() {
        let events = parse_line(
            r#"{"message":{"tool_calls":[{"function":{"name":"list_apps"}}]},"done":true}"#,
        );

        assert_eq!(
            events,
            vec![
                NdjsonEvent::ToolCalls(vec![ToolCallJson {
                    name: "list_apps".to_string(),
                    arguments: "{}".to_string(),
                }]),
                NdjsonEvent::Done,
            ]
        );
    }

    #[test]
    fn a_tool_call_with_no_name_is_dropped() {
        let events = parse_line(
            r#"{"message":{"content":"hi","tool_calls":[{"function":{"arguments":{}}}]},"done":false}"#,
        );

        assert_eq!(events, vec![NdjsonEvent::Delta("hi".to_string())]);
    }

    #[test]
    fn parses_a_content_delta() {
        assert_eq!(
            parse_line(
                r#"{"model":"llama3.2","message":{"role":"assistant","content":"Hi"},"done":false}"#
            ),
            vec![NdjsonEvent::Delta("Hi".to_string())]
        );
    }

    #[test]
    fn reports_trailing_content_before_done() {
        assert_eq!(
            parse_line(r#"{"message":{"role":"assistant","content":"!"},"done":true}"#),
            vec![NdjsonEvent::Delta("!".to_string()), NdjsonEvent::Done]
        );
    }

    #[test]
    fn a_bare_done_object_is_just_done() {
        assert_eq!(
            parse_line(r#"{"message":{"role":"assistant","content":""},"done":true}"#),
            vec![NdjsonEvent::Done]
        );
    }

    #[test]
    fn parses_an_error_object() {
        assert_eq!(
            parse_line(r#"{"error":"model 'llama3.2' not found"}"#),
            vec![NdjsonEvent::Error("model 'llama3.2' not found".to_string())]
        );
    }

    #[test]
    fn blank_lines_produce_nothing() {
        assert!(parse_line("").is_empty());
        assert!(parse_line("   ").is_empty());
    }

    #[test]
    fn malformed_json_is_ignored_not_fatal() {
        assert_eq!(parse_line("{not json"), vec![NdjsonEvent::Ignored]);
        assert_eq!(parse_line("plain text"), vec![NdjsonEvent::Ignored]);
    }

    #[test]
    fn unrecognised_shapes_are_ignored() {
        assert_eq!(
            parse_line(r#"{"model":"llama3.2","created_at":"2026-07-25T00:00:00Z"}"#),
            vec![NdjsonEvent::Ignored]
        );
    }
}
