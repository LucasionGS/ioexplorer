//! The system prompt.
//!
//! Without one the model has no idea where it is running or what is expected of
//! it, and it defaults to the safest thing it knows: answering the question as
//! asked and handing anything uncertain back to the user. With tools available
//! that is exactly the wrong instinct — the point of `read_file` and
//! `run_command` is that the model can go and find out.
//!
//! Kept as one constant, and resolved once when the conversation opens, because
//! it renders near the front of the prompt: a per-turn rebuild would invalidate
//! the prompt cache on every round of a tool loop.

use crate::config::SpotlightAiConfig;

/// What the model is told before the first message.
///
/// Three jobs, in order of how often they matter: keep it short (this renders in
/// a launcher overlay, not a chat window), keep it working (use the tools, do
/// not stop at the first obstacle), and keep it from interrogating the user
/// (a question the tools can answer is not a question worth asking).
pub const AGENTIC_SYSTEM_PROMPT: &str = "\
You are the assistant inside ioexplorer-spotlight, a keyboard launcher on the \
user's Linux desktop. Replies appear in a small overlay above their search bar, \
so lead with the answer and keep it short. No preamble, no restating the \
question, no offers of further help.

Work things out yourself. When a tool can settle a question, use it instead of \
guessing or asking: read the file, list the directory, run the command. Chain as \
many calls as the job needs — looking something up, acting on what you found, \
and checking the result is one task, not three. If a command fails or returns \
something unexpected, diagnose it and try another approach; a failed first \
attempt is not an answer.

Tool results are real output from the user's actual machine. Trust them over \
your own assumptions about how their system is set up, and never describe what a \
command would probably print when you can run it and read what it did print.

Only ask the user something when you genuinely cannot continue without them: a \
fact no tool can supply, or a choice with consequences that is theirs to make. \
Never ask for permission to use a tool — anything that changes the system \
already prompts them on its own, so asking first only costs a round trip.

Commands run through `sh -c` with no terminal attached. They cannot prompt, \
their stdin is empty, and they are stopped if they outlive the timeout, so pass \
non-interactive flags and background anything long-running or graphical with \
`&`. You get back the merged stdout and stderr and the exit status.

Stop when the task is done, or when you are certain it cannot be. Then say what \
you found and what you changed.";

/// The system prompt for one provider — the user's override, or the built-in.
///
/// An override replaces rather than extends. Appending to text the user cannot
/// see would make their prompt behave differently from what they wrote, and
/// there is no way to debug that from the outside.
pub fn resolve(entry: &SpotlightAiConfig) -> Option<String> {
    match entry.system_prompt.as_deref().map(str::trim) {
        // An empty string is a deliberate "no system prompt at all", distinct
        // from the key being absent.
        Some("") => None,
        Some(custom) => Some(custom.to_string()),
        None => Some(AGENTIC_SYSTEM_PROMPT.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built through TOML rather than a struct literal, so these also check
    /// that the new keys really are optional in a user's config.
    fn entry(system_prompt: Option<&str>) -> SpotlightAiConfig {
        let mut entry: SpotlightAiConfig =
            toml::from_str("prefix = \"ai\"\nprovider = \"mock\"\n").expect("minimal ai entry");
        entry.system_prompt = system_prompt.map(str::to_string);
        entry
    }

    #[test]
    fn defaults_to_the_built_in_prompt() {
        assert_eq!(
            resolve(&entry(None)).as_deref(),
            Some(AGENTIC_SYSTEM_PROMPT)
        );
    }

    #[test]
    fn an_override_replaces_it_entirely() {
        let resolved = resolve(&entry(Some("Be terse."))).unwrap();
        assert_eq!(resolved, "Be terse.");
    }

    /// Explicitly empty means "send none", which is the only way to get the
    /// pre-prompt behaviour back.
    #[test]
    fn an_empty_override_sends_nothing() {
        assert_eq!(resolve(&entry(Some("   "))), None);
    }

    /// The prompt is the whole mechanism behind "keep working"; a rewrite that
    /// drops the instruction not to interrogate the user would regress the
    /// behaviour with nothing failing.
    #[test]
    fn the_built_in_prompt_tells_the_model_to_finish_on_its_own() {
        assert!(AGENTIC_SYSTEM_PROMPT.contains("Work things out yourself"));
        assert!(AGENTIC_SYSTEM_PROMPT.contains("Only ask the user something when"));
    }
}
