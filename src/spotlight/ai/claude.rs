//! Claude backend — `POST /v1/messages` with `stream: true`.
//!
//! Rust has no official Anthropic SDK, so this speaks the HTTP API directly.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::config::AiEffort;

use super::{AiError, AiEvent, ApiKey, ChatMessage, sse};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const PROVIDER: &str = "Claude";
/// The error body is small; never read an unbounded amount of it.
const MAX_ERROR_BODY: u64 = 64 * 1024;

pub struct Request<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub effort: AiEffort,
    pub api_key_env: &'a str,
    pub api_key_file: Option<&'a Path>,
}

pub fn run(
    request: Request<'_>,
    history: &[ChatMessage],
    generation: u64,
    is_stale: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(AiEvent) -> bool,
) {
    let key = match resolve_key(request.api_key_file, request.api_key_env) {
        Ok(key) => key,
        Err(error) => {
            emit(AiEvent::Failed { generation, error });
            return;
        }
    };

    let body = request_body(&request, history);

    // `http_status_as_error(false)` is load-bearing: the default turns a 401 or
    // 429 into a bare `Error::StatusCode(u16)`, discarding the API's own error
    // message and the `retry-after` header — exactly what the UI needs to show.
    //
    // Only connect and response-header timeouts are set. `timeout_recv_body`
    // and `timeout_global` are total-duration budgets rather than idle
    // timeouts, and would cut a long answer off mid-sentence.
    let response = ureq::post(ENDPOINT)
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .header("content-type", "application/json")
        .header("x-api-key", key.expose())
        .header("anthropic-version", API_VERSION)
        .header("accept", "text/event-stream")
        // Suppress ureq's automatic gzip on a streaming response.
        .header("accept-encoding", "identity")
        .send(body);

    let response = match response {
        Ok(response) => response,
        Err(error) => {
            emit(AiEvent::Failed {
                generation,
                error: network_error(&error),
            });
            return;
        }
    };

    let status = response.status().as_u16();
    // Must be read before `into_parts` moves the body out.
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let (_parts, body) = response.into_parts();

    if !(200..300).contains(&status) {
        let text = body
            .into_with_config()
            .limit(MAX_ERROR_BODY)
            .read_to_string()
            .unwrap_or_default();
        emit(AiEvent::Failed {
            generation,
            error: classify_http_error(
                status,
                &text,
                retry_after,
                request.model,
                request.api_key_env,
            ),
        });
        return;
    }

    stream_body(
        BufReader::new(body.into_reader()),
        generation,
        is_stale,
        emit,
    );
}

fn stream_body(
    reader: impl BufRead,
    generation: u64,
    is_stale: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(AiEvent) -> bool,
) {
    let mut stop_reason = None;
    let mut refusal_category = None;
    let mut saw_terminal = false;

    for line in reader.lines() {
        if is_stale() {
            return;
        }
        let Ok(line) = line else {
            break;
        };

        match sse::parse_line(&line) {
            sse::SseEvent::ThinkingStart => {
                if !emit(AiEvent::Thinking { generation }) {
                    return;
                }
            }
            sse::SseEvent::TextDelta(text) => {
                if !text.is_empty() && !emit(AiEvent::Delta { generation, text }) {
                    return;
                }
            }
            sse::SseEvent::MessageDelta {
                stop_reason: reason,
                refusal_category: category,
            } => {
                stop_reason = reason.or(stop_reason);
                refusal_category = category.or(refusal_category);
            }
            sse::SseEvent::MessageStop => {
                saw_terminal = true;
                break;
            }
            sse::SseEvent::Error { kind, message } => {
                emit(AiEvent::Failed {
                    generation,
                    error: AiError::Http {
                        provider: PROVIDER.to_string(),
                        status: 200,
                        message: format!("{kind}: {message}"),
                    },
                });
                return;
            }
            _ => {}
        }
    }

    // A refusal arrives on `message_delta`, after any content blocks — so text
    // can stream and only then turn out to be refused. Report the refusal
    // rather than leaving a half-answer standing.
    if stop_reason.as_deref() == Some("refusal") {
        emit(AiEvent::Failed {
            generation,
            error: AiError::Refused {
                category: refusal_category,
            },
        });
        return;
    }

    if !saw_terminal {
        emit(AiEvent::Failed {
            generation,
            error: AiError::Protocol {
                provider: PROVIDER.to_string(),
                detail: "the stream ended unexpectedly".to_string(),
            },
        });
        return;
    }

    emit(AiEvent::Done {
        generation,
        stop_reason,
    });
}

/// Finds the API key: the key file first, then the environment variable.
///
/// Resolved here, at query time on the worker thread, rather than at startup:
/// the daemon usually inherits a minimal compositor environment, so failing at
/// startup would disable the provider for the whole session with no visible
/// cause, and a key file could not be created after the fact.
fn resolve_key(file: Option<&Path>, var: &str) -> Result<ApiKey, AiError> {
    if let Some(path) = file {
        let path = expand_tilde(path);
        return match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let key = contents.trim().to_string();
                if key.is_empty() {
                    return Err(AiError::KeyFileUnreadable {
                        path: path.display().to_string(),
                        detail: "the file is empty".to_string(),
                    });
                }
                Ok(ApiKey(key))
            }
            Err(error) => Err(AiError::KeyFileUnreadable {
                path: path.display().to_string(),
                detail: error.to_string(),
            }),
        };
    }

    std::env::var(var)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .map(ApiKey)
        .ok_or_else(|| AiError::MissingKey {
            var: var.to_string(),
        })
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join(rest))
        .unwrap_or_else(|| path.to_path_buf())
}

fn request_body(request: &Request<'_>, history: &[ChatMessage]) -> String {
    let messages: Vec<serde_json::Value> = history
        .iter()
        .map(|message| serde_json::json!({ "role": message.api_role(), "content": message.text }))
        .collect();

    // Deliberately absent: `temperature`/`top_p`/`top_k` (removed on Opus 5,
    // they return 400) and `thinking` (on by default on Opus 5; disabling it
    // 400s above `high` effort and leaks reasoning into the visible reply).
    let mut body = serde_json::json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "stream": true,
        "messages": messages,
    });

    // `effort` is the supported latency lever — but only on models that have
    // it. Haiku and the 4.5-and-older line reject it outright.
    if let Some(effort) = effort_for(request.model, request.effort) {
        body["output_config"] = serde_json::json!({ "effort": effort });
    }

    body.to_string()
}

/// The `effort` value to send for a model, or `None` when it must be omitted.
///
/// Allow-by-default so a model released after this code still works: only the
/// families known to reject `effort` are excluded.
fn effort_for(model: &str, effort: AiEffort) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();

    // Haiku (any generation) and Sonnet/Opus 4.5-and-older return 400 for
    // `effort` — the parameter did not exist yet.
    let rejects_effort = model.contains("haiku")
        || model.contains("sonnet-4-5")
        || model.contains("sonnet-4-0")
        || model.contains("opus-4-1")
        || model.contains("opus-4-0")
        || model.contains("claude-3")
        || model.contains("claude-2");
    if rejects_effort {
        return None;
    }

    // Opus 4.5 has `effort` but only through `high`; `xhigh`/`max` 400 there.
    if model.contains("opus-4-5") {
        return Some(match effort {
            AiEffort::Low => "low",
            AiEffort::Medium => "medium",
            _ => "high",
        });
    }

    Some(effort.as_str())
}

fn network_error(error: &ureq::Error) -> AiError {
    AiError::Network {
        endpoint: "api.anthropic.com".to_string(),
        detail: match error {
            ureq::Error::HostNotFound => "host not found; check your connection".to_string(),
            ureq::Error::ConnectionFailed => "connection failed".to_string(),
            ureq::Error::Timeout(_) => "the request timed out".to_string(),
            other => other.to_string(),
        },
    }
}

/// Maps an HTTP failure onto a message that names the next step.
///
/// Pure over its inputs so every branch is unit-testable.
fn classify_http_error(
    status: u16,
    body: &str,
    retry_after: Option<u64>,
    model: &str,
    key_var: &str,
) -> AiError {
    let message = api_error_message(body);

    match status {
        401 | 403 => AiError::Unauthorized {
            provider: PROVIDER.to_string(),
            var: key_var.to_string(),
        },
        404 => AiError::ModelUnavailable {
            model: model.to_string(),
            hint: Some("check `model` in [[spotlight.ai]]".to_string()),
        },
        429 => AiError::RateLimited { retry_after },
        // A 400 names the offending parameter; surface it verbatim, since that
        // is exactly what is needed while tuning `effort` or `max_tokens`.
        _ => AiError::Http {
            provider: PROVIDER.to_string(),
            status,
            message: message.unwrap_or_else(|| "no details returned".to_string()),
        },
    }
}

fn api_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>() -> Request<'a> {
        Request {
            model: "claude-opus-5",
            max_tokens: 8192,
            effort: AiEffort::Low,
            api_key_env: "ANTHROPIC_API_KEY",
            api_key_file: None,
        }
    }

    fn body_json(history: &[ChatMessage]) -> serde_json::Value {
        serde_json::from_str(&request_body(&request(), history)).expect("valid json")
    }

    #[test]
    fn builds_a_streaming_request_body() {
        let body = body_json(&[ChatMessage::user("hello")]);

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["stream"], true);
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
    }

    #[test]
    fn omits_effort_on_models_that_reject_it() {
        // Haiku has no `effort` parameter at all — sending it is a 400.
        for model in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5",
            "claude-opus-4-1",
            "claude-3-haiku-20240307",
        ] {
            assert_eq!(effort_for(model, AiEffort::Low), None, "{model}");
        }
    }

    #[test]
    fn sends_effort_on_models_that_support_it() {
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-sonnet-4-6",
            "claude-fable-5",
        ] {
            assert_eq!(
                effort_for(model, AiEffort::Medium),
                Some("medium"),
                "{model}"
            );
        }
    }

    #[test]
    fn clamps_effort_to_high_on_opus_4_5() {
        // Opus 4.5 has effort but only through `high`; xhigh/max are a 400.
        assert_eq!(effort_for("claude-opus-4-5", AiEffort::Xhigh), Some("high"));
        assert_eq!(effort_for("claude-opus-4-5", AiEffort::Max), Some("high"));
        assert_eq!(effort_for("claude-opus-4-5", AiEffort::Low), Some("low"));
    }

    #[test]
    fn an_unknown_future_model_still_gets_effort() {
        assert_eq!(effort_for("claude-opus-6", AiEffort::High), Some("high"));
    }

    #[test]
    fn a_haiku_request_body_carries_no_output_config() {
        let body: serde_json::Value = serde_json::from_str(&request_body(
            &Request {
                model: "claude-haiku-4-5",
                max_tokens: 4096,
                effort: AiEffort::Low,
                api_key_env: "K",
                api_key_file: None,
            },
            &[ChatMessage::user("hi")],
        ))
        .expect("valid json");

        assert!(body.get("output_config").is_none());
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn omits_parameters_that_are_rejected_on_opus_5() {
        let body = body_json(&[ChatMessage::user("hi")]);

        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
        assert!(
            body.get("thinking").is_none(),
            "thinking stays on by default"
        );
    }

    #[test]
    fn sends_the_full_conversation_in_order() {
        let body = body_json(&[
            ChatMessage::user("first"),
            ChatMessage::assistant("reply"),
            ChatMessage::user("second"),
        ]);

        assert_eq!(body["messages"].as_array().expect("array").len(), 3);
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "second");
    }

    #[test]
    fn unauthorized_names_the_environment_variable() {
        let error = classify_http_error(401, "{}", None, "claude-opus-5", "MY_KEY");

        assert!(matches!(error, AiError::Unauthorized { ref var, .. } if var == "MY_KEY"));
        assert!(error.to_string().contains("MY_KEY"));
    }

    #[test]
    fn rate_limited_carries_retry_after() {
        let error = classify_http_error(429, "{}", Some(30), "claude-opus-5", "K");

        assert!(matches!(
            error,
            AiError::RateLimited {
                retry_after: Some(30)
            }
        ));
        assert!(error.to_string().contains("30s"));
    }

    #[test]
    fn rate_limited_without_a_header_still_renders() {
        let error = classify_http_error(429, "{}", None, "claude-opus-5", "K");

        assert!(matches!(error, AiError::RateLimited { retry_after: None }));
        assert!(error.to_string().contains("Rate limited"));
    }

    #[test]
    fn a_400_surfaces_the_api_message_verbatim() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking.type: disabled is not supported at effort xhigh"}}"#;

        let error = classify_http_error(400, body, None, "claude-opus-5", "K");

        assert!(
            error
                .to_string()
                .contains("disabled is not supported at effort xhigh")
        );
    }

    #[test]
    fn a_404_names_the_config_key() {
        let error = classify_http_error(404, "{}", None, "claude-nope", "K");

        assert!(error.to_string().contains("claude-nope"));
        assert!(error.to_string().contains("[[spotlight.ai]]"));
    }

    #[test]
    fn a_500_reports_the_status() {
        let error = classify_http_error(529, "{}", None, "claude-opus-5", "K");

        assert!(matches!(error, AiError::Http { status: 529, .. }));
    }

    #[test]
    fn reads_the_key_from_a_key_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("key");
        // Trailing newline is what `echo > file` leaves behind.
        std::fs::write(&path, "sk-ant-from-file\n").expect("write key");

        let key = resolve_key(Some(&path), "IOEXPLORER_UNSET").expect("key file");

        assert_eq!(key.expose(), "sk-ant-from-file");
    }

    #[test]
    fn a_key_file_wins_over_the_environment() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("key");
        std::fs::write(&path, "from-file").expect("write key");

        // PATH is always set; if the env were consulted this would not match.
        let key = resolve_key(Some(&path), "PATH").expect("key file");

        assert_eq!(key.expose(), "from-file");
    }

    #[test]
    fn a_missing_key_file_names_the_path() {
        let error = resolve_key(Some(Path::new("/nope/missing-key")), "X").expect_err("missing");

        assert!(matches!(error, AiError::KeyFileUnreadable { .. }));
        assert!(error.to_string().contains("/nope/missing-key"));
    }

    #[test]
    fn an_empty_key_file_is_reported_rather_than_sent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("key");
        std::fs::write(&path, "   \n").expect("write key");

        let error = resolve_key(Some(&path), "X").expect_err("empty");

        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn the_missing_key_message_says_where_the_key_can_go() {
        let error = resolve_key(None, "IOEXPLORER_TEST_KEY_THAT_IS_NOT_SET").expect_err("unset");
        let text = error.to_string();

        // The message has to steer people away from putting it in config.toml,
        // which is the mistake the field naming invites.
        assert!(text.contains("api_key_file"));
        assert!(text.contains("IOEXPLORER_TEST_KEY_THAT_IS_NOT_SET"));
        assert!(text.contains("config.toml"));
    }

    #[test]
    fn a_missing_key_is_reported_with_the_variable_name() {
        let error = resolve_key(None, "IOEXPLORER_TEST_KEY_THAT_IS_NOT_SET").expect_err("unset");

        assert!(
            matches!(error, AiError::MissingKey { ref var } if var == "IOEXPLORER_TEST_KEY_THAT_IS_NOT_SET")
        );
        assert!(error.to_string().contains("restart"));
    }

    /// Drives the stream loop over a canned SSE transcript — no network.
    fn collect(transcript: &str) -> Vec<AiEvent> {
        let mut events = Vec::new();
        let never_stale = || false;
        let mut emit = |event: AiEvent| {
            events.push(event);
            true
        };
        stream_body(
            BufReader::new(transcript.as_bytes()),
            1,
            &never_stale,
            &mut emit,
        );
        events
    }

    #[test]
    fn streams_text_then_reports_done() {
        let events = collect(concat!(
            "data: {\"type\":\"message_start\",\"message\":{}}\n",
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ));

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                AiEvent::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(matches!(
            events.last(),
            Some(AiEvent::Done { stop_reason: Some(reason), .. }) if reason == "end_turn"
        ));
    }

    #[test]
    fn reports_thinking_without_adding_it_to_the_transcript() {
        let events = collect(concat!(
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"thinking\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ));

        assert!(events.iter().any(|e| matches!(e, AiEvent::Thinking { .. })));
        assert!(
            !events.iter().any(|e| matches!(e, AiEvent::Delta { .. })),
            "thinking must never reach the transcript"
        );
    }

    #[test]
    fn a_refusal_after_partial_text_fails_rather_than_half_answering() {
        let events = collect(concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Sure, \"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"category\":\"cyber\"}}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ));

        assert!(matches!(
            events.last(),
            Some(AiEvent::Failed { error: AiError::Refused { category: Some(c) }, .. }) if c == "cyber"
        ));
    }

    #[test]
    fn a_truncated_stream_is_reported_as_a_protocol_error() {
        let events = collect(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n",
        );

        assert!(matches!(
            events.last(),
            Some(AiEvent::Failed {
                error: AiError::Protocol { .. },
                ..
            })
        ));
    }

    #[test]
    fn an_in_stream_error_event_is_terminal() {
        let events = collect(
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n",
        );

        assert!(matches!(
            events.last(),
            Some(AiEvent::Failed {
                error: AiError::Http { .. },
                ..
            })
        ));
    }

    #[test]
    fn max_tokens_is_reported_so_the_ui_can_warn() {
        let events = collect(concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"cut\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        ));

        assert!(matches!(
            events.last(),
            Some(AiEvent::Done { stop_reason: Some(reason), .. }) if reason == "max_tokens"
        ));
    }
}
