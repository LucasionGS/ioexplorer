//! Ollama backend — `POST {endpoint}/api/chat` with `stream: true`.
//!
//! No authentication; the endpoint is expected to be a local daemon.

use std::{
    io::{BufRead, BufReader},
    time::Duration,
};

use super::{
    AiError, AiEvent, ChatMessage, ndjson,
    tools::{ToolCall, ToolDef},
};

const PROVIDER: &str = "Ollama";
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// Cap on the model list quoted back when a model is missing.
const MAX_LISTED_MODELS: usize = 8;

pub struct Request<'a> {
    pub model: &'a str,
    pub endpoint: &'a str,
    pub tools: &'a [ToolDef],
    pub system: Option<&'a str>,
}

pub fn run(
    request: Request<'_>,
    history: &[ChatMessage],
    generation: u64,
    is_stale: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(AiEvent) -> bool,
) {
    let Request {
        model,
        endpoint,
        tools,
        system,
    } = request;
    let body = request_body(model, history, tools, system);

    let response = ureq::post(format!("{endpoint}/api/chat"))
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .header("content-type", "application/json")
        .header("accept-encoding", "identity")
        .send(body);

    let response = match response {
        Ok(response) => response,
        Err(error) => {
            emit(AiEvent::Failed {
                generation,
                error: network_error(&error, endpoint),
            });
            return;
        }
    };

    let status = response.status().as_u16();
    let (_parts, body) = response.into_parts();

    if !(200..300).contains(&status) {
        let text = body
            .into_with_config()
            .limit(MAX_ERROR_BODY)
            .read_to_string()
            .unwrap_or_default();
        let mut error = classify_http_error(status, &text, model);
        // A missing model is the common first-run failure; saying which models
        // *are* installed turns a dead end into a one-line fix.
        if matches!(error, AiError::ModelUnavailable { .. })
            && let Some(installed) = installed_models(endpoint)
        {
            error = AiError::ModelUnavailable {
                model: model.to_string(),
                hint: Some(format!(
                    "run `ollama pull {model}`. Installed: {}",
                    installed.join(", ")
                )),
            };
        }
        emit(AiEvent::Failed { generation, error });
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
    let mut saw_done = false;

    for line in reader.lines() {
        if is_stale() {
            return;
        }
        let Ok(line) = line else {
            break;
        };

        for event in ndjson::parse_line(&line) {
            match event {
                ndjson::NdjsonEvent::Delta(text) => {
                    if !emit(AiEvent::Delta { generation, text }) {
                        return;
                    }
                }
                ndjson::NdjsonEvent::ToolCalls(raw) => {
                    let calls = raw
                        .into_iter()
                        .enumerate()
                        .map(|(index, call)| ToolCall {
                            // Ollama issues no call ids, but the result pairing
                            // needs one — so synthesize a per-turn unique id.
                            id: format!("ollama_{generation}_{index}"),
                            name: call.name,
                            input: serde_json::from_str(&call.arguments)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        })
                        .collect::<Vec<_>>();

                    if !calls.is_empty() {
                        emit(AiEvent::ToolRequested {
                            generation,
                            text: String::new(),
                            calls,
                        });
                        return;
                    }
                }
                ndjson::NdjsonEvent::Done => saw_done = true,
                ndjson::NdjsonEvent::Error(message) => {
                    emit(AiEvent::Failed {
                        generation,
                        error: AiError::Http {
                            provider: PROVIDER.to_string(),
                            status: 200,
                            message,
                        },
                    });
                    return;
                }
                ndjson::NdjsonEvent::Ignored => {}
            }
        }

        if saw_done {
            break;
        }
    }

    if !saw_done {
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
        stop_reason: None,
    });
}

fn request_body(
    model: &str,
    history: &[ChatMessage],
    tools: &[ToolDef],
    system: Option<&str>,
) -> String {
    // Ollama has a real `system` role, so unlike Claude this goes in the message
    // list — first, where every model that supports one expects to find it.
    let mut messages: Vec<serde_json::Value> = Vec::with_capacity(history.len() + 1);
    if let Some(system) = system.map(str::trim).filter(|text| !text.is_empty()) {
        messages.push(serde_json::json!({ "role": "system", "content": system }));
    }
    messages.extend(history.iter().map(
        |message| serde_json::json!({ "role": message.api_role(), "content": message.text() }),
    ));

    let mut body = serde_json::json!({
        "model": model,
        "stream": true,
        "messages": messages,
    });

    if !tools.is_empty() {
        // Ollama wraps each declaration in an OpenAI-shaped envelope rather
        // than taking Claude's flat form.
        body["tools"] = serde_json::Value::Array(
            tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.schema,
                        },
                    })
                })
                .collect(),
        );
    }

    body.to_string()
}

/// Best-effort model list for the "model not installed" message. Short timeout;
/// a failure here just means a less helpful error, never a worse one.
fn installed_models(endpoint: &str) -> Option<Vec<String>> {
    let response = ureq::get(format!("{endpoint}/api/tags"))
        .config()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_recv_response(Some(Duration::from_secs(3)))
        .build()
        .call()
        .ok()?;

    let (_parts, body) = response.into_parts();
    let text = body
        .into_with_config()
        .limit(MAX_ERROR_BODY)
        .read_to_string()
        .ok()?;

    let models = parse_tags(&text);
    (!models.is_empty()).then_some(models)
}

fn parse_tags(body: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("models").cloned())
        .and_then(|models| models.as_array().cloned())
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("name")?.as_str().map(str::to_string))
                .take(MAX_LISTED_MODELS)
                .collect()
        })
        .unwrap_or_default()
}

fn network_error(error: &ureq::Error, endpoint: &str) -> AiError {
    let detail = match error {
        ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => {
            "is `ollama serve` running?".to_string()
        }
        ureq::Error::Timeout(_) => "the request timed out".to_string(),
        other => other.to_string(),
    };
    AiError::Network {
        endpoint: endpoint.to_string(),
        detail,
    }
}

/// Pure over its inputs so every branch is unit-testable.
fn classify_http_error(status: u16, body: &str, model: &str) -> AiError {
    let message = error_message(body);
    let looks_missing = message
        .as_deref()
        .is_some_and(|text| text.contains("not found") || text.contains("try pulling"));

    if status == 404 || looks_missing {
        return AiError::ModelUnavailable {
            model: model.to_string(),
            hint: Some(format!("run `ollama pull {model}`")),
        };
    }

    AiError::Http {
        provider: PROVIDER.to_string(),
        status,
        message: message.unwrap_or_else(|| "no details returned".to_string()),
    }
}

fn error_message(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_streaming_request_body() {
        let body: serde_json::Value = serde_json::from_str(&request_body(
            "llama3.2",
            &[ChatMessage::user("hi")],
            &[],
            None,
        ))
        .expect("valid json");

        assert_eq!(body["model"], "llama3.2");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    /// Ollama does have a `system` role, so unlike Claude it goes in the message
    /// list — and it has to be first, which is where every model that honours
    /// one looks for it.
    #[test]
    fn the_system_prompt_leads_the_message_list() {
        let body: serde_json::Value = serde_json::from_str(&request_body(
            "llama3.2",
            &[ChatMessage::user("hi")],
            &[],
            Some("Be terse."),
        ))
        .expect("valid json");

        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "Be terse.");
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn a_blank_system_prompt_adds_no_message() {
        let body: serde_json::Value = serde_json::from_str(&request_body(
            "llama3.2",
            &[ChatMessage::user("hi")],
            &[],
            Some("  "),
        ))
        .expect("valid json");

        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn a_missing_model_names_the_pull_command() {
        let error = classify_http_error(404, r#"{"error":"model not found"}"#, "llama3.2");

        assert!(error.to_string().contains("ollama pull llama3.2"));
    }

    #[test]
    fn a_missing_model_is_detected_from_the_message_on_a_200_shaped_error() {
        let error = classify_http_error(
            400,
            r#"{"error":"model 'zephyr' not found, try pulling it first"}"#,
            "zephyr",
        );

        assert!(matches!(error, AiError::ModelUnavailable { .. }));
    }

    #[test]
    fn other_statuses_surface_the_message() {
        let error = classify_http_error(500, r#"{"error":"out of memory"}"#, "llama3.2");

        assert!(matches!(error, AiError::Http { status: 500, .. }));
        assert!(error.to_string().contains("out of memory"));
    }

    #[test]
    fn parses_the_installed_model_list() {
        let models = parse_tags(r#"{"models":[{"name":"llama3.2:latest"},{"name":"qwen2.5:7b"}]}"#);

        assert_eq!(models, vec!["llama3.2:latest", "qwen2.5:7b"]);
    }

    #[test]
    fn a_malformed_tag_list_yields_no_models() {
        assert!(parse_tags("not json").is_empty());
        assert!(parse_tags("{}").is_empty());
    }

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
            "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
        ));

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                AiEvent::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(matches!(events.last(), Some(AiEvent::Done { .. })));
    }

    #[test]
    fn keeps_trailing_content_on_the_final_object() {
        let events = collect("{\"message\":{\"content\":\"done!\"},\"done\":true}\n");

        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiEvent::Delta { text, .. } if text == "done!"))
        );
        assert!(matches!(events.last(), Some(AiEvent::Done { .. })));
    }

    #[test]
    fn a_truncated_stream_is_reported_as_a_protocol_error() {
        let events = collect("{\"message\":{\"content\":\"partial\"},\"done\":false}\n");

        assert!(matches!(
            events.last(),
            Some(AiEvent::Failed {
                error: AiError::Protocol { .. },
                ..
            })
        ));
    }

    #[test]
    fn an_error_object_mid_stream_is_terminal() {
        let events = collect("{\"error\":\"context length exceeded\"}\n");

        assert!(matches!(
            events.last(),
            Some(AiEvent::Failed {
                error: AiError::Http { .. },
                ..
            })
        ));
    }
}
