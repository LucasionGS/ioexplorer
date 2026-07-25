//! A scripted local provider that performs no network I/O.
//!
//! Exists so the chat UI — streaming, growth, autoscroll, cancellation, error
//! rendering — can be exercised before any credentials or local model exist.
//! Configure it with `provider = "mock"`.
//!
//! Two prompts are special so the failure paths are reachable too:
//! a prompt containing `error` fails, and one containing `slow` streams
//! deliberately slowly.

use std::{thread, time::Duration};

use super::{AiError, AiEvent, ChatMessage};

/// Pause between tokens — slow enough to watch, fast enough not to annoy.
const TOKEN_DELAY: Duration = Duration::from_millis(28);
const SLOW_TOKEN_DELAY: Duration = Duration::from_millis(300);

pub fn run(
    history: &[ChatMessage],
    generation: u64,
    is_stale: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(AiEvent) -> bool,
) {
    let prompt = history
        .iter()
        .rev()
        .find(|message| matches!(message.role, super::Role::User))
        .map(|message| message.text.as_str())
        .unwrap_or_default();
    let lowered = prompt.to_lowercase();

    if lowered.contains("error") {
        emit(AiEvent::Failed {
            generation,
            error: AiError::Http {
                provider: "Mock".to_string(),
                status: 500,
                message: "the mock provider was asked to fail".to_string(),
            },
        });
        return;
    }

    // Mirrors the real providers: a reasoning phase with no visible output.
    if !emit(AiEvent::Thinking { generation }) {
        return;
    }
    thread::sleep(TOKEN_DELAY * 4);

    let delay = if lowered.contains("slow") {
        SLOW_TOKEN_DELAY
    } else {
        TOKEN_DELAY
    };

    for token in reply(prompt, history.len()) {
        if is_stale() {
            return;
        }
        if !emit(AiEvent::Delta {
            generation,
            text: token,
        }) {
            return;
        }
        thread::sleep(delay);
    }

    emit(AiEvent::Done {
        generation,
        stop_reason: Some("end_turn".to_string()),
    });
}

/// Splits a canned reply into word-sized tokens, keeping the trailing spaces so
/// the reassembled text is byte-identical to `reply_text`.
fn reply(prompt: &str, turns: usize) -> Vec<String> {
    reply_text(prompt, turns)
        .split_inclusive(' ')
        .map(str::to_string)
        .collect()
}

fn reply_text(prompt: &str, turns: usize) -> String {
    if prompt.trim().is_empty() {
        return "I did not receive a prompt.".to_string();
    }

    format!(
        "This is the mock provider, so nothing left your machine. \
You said: \"{prompt}\". This is turn {turns} of the conversation. \
Ask again with the word 'slow' to watch the stream token by token, \
or 'error' to see how a failure renders."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(prompt: &str) -> Vec<AiEvent> {
        let mut events = Vec::new();
        let never_stale = || false;
        let mut emit = |event: AiEvent| {
            events.push(event);
            true
        };
        run(&[ChatMessage::user(prompt)], 1, &never_stale, &mut emit);
        events
    }

    fn streamed(events: &[AiEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                AiEvent::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn streams_a_reply_that_reassembles_exactly() {
        let events = collect("hello");

        assert_eq!(streamed(&events), reply_text("hello", 1));
        assert!(matches!(events.last(), Some(AiEvent::Done { .. })));
    }

    #[test]
    fn echoes_the_prompt_so_multi_turn_context_is_visible() {
        assert!(streamed(&collect("banana")).contains("banana"));
    }

    #[test]
    fn signals_thinking_before_any_text() {
        let events = collect("hello");

        let thinking = events
            .iter()
            .position(|event| matches!(event, AiEvent::Thinking { .. }));
        let first_delta = events
            .iter()
            .position(|event| matches!(event, AiEvent::Delta { .. }));
        assert!(thinking < first_delta);
    }

    #[test]
    fn an_error_prompt_fails_without_streaming() {
        let events = collect("please error");

        assert!(matches!(events.first(), Some(AiEvent::Failed { .. })));
        assert!(streamed(&events).is_empty());
    }

    #[test]
    fn an_empty_prompt_still_replies() {
        assert!(streamed(&collect("   ")).contains("did not receive"));
    }

    #[test]
    fn stops_promptly_once_superseded() {
        let mut events = Vec::new();
        let always_stale = || true;
        let mut emit = |event: AiEvent| {
            events.push(event);
            true
        };
        run(&[ChatMessage::user("hello")], 1, &always_stale, &mut emit);

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AiEvent::Done { .. })),
            "a superseded generation must not report completion"
        );
    }
}
