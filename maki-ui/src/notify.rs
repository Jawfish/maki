//! Desktop notifications, fired when a turn finishes or needs input while the
//! terminal window is unfocused. `notify_rust::Notification::show` blocks on
//! an IPC round trip (D-Bus on Linux/BSD), so it runs on its own thread
//! instead of stalling the event loop.
//!
//! Bodies are plain text. The XDG spec only allows markup when the server
//! advertises `body-markup`, and escaping unconditionally would print
//! `&amp;` on the servers that do not.
//!
//! With `ui.notify_model` set, the body is written by that model instead of
//! quoting the reply. The call is fire and forget: anything slower than
//! [`SUMMARY_TIMEOUT`], or any failure at all, falls back to the quote.

use std::path::Path;
use std::time::Duration;

use futures_lite::future;
use maki_providers::model::Model;
use maki_providers::provider::from_model_async;
use maki_providers::{ContentBlock, Message, RequestOptions, Role, ThinkingConfig, Timeouts};
use notify_rust::Notification;
use serde_json::Value;
use tracing::{debug, warn};

const APP_NAME: &str = "maki";
const SNIPPET_MAX_CHARS: usize = 160;
const ELLIPSIS: &str = "...";
/// A word-boundary cut is only worth it while it keeps this fraction of the
/// budget, otherwise one very long token would shrink the body to nothing.
const MIN_KEEP_DIVISOR: usize = 2;

/// Which project the popup is about, since several sessions look alike.
const TITLE_SEPARATOR: &str = " · ";
pub(crate) const STATUS_DONE: &str = "done";
pub(crate) const STATUS_NEEDS_INPUT: &str = "needs input";

/// Long enough for a whole reply on all but the largest turns. Past it the
/// middle goes: a one-line summary needs the framing and the outcome, not the
/// work in between.
const REPLY_MAX_CHARS: usize = 15_000;
const REPLY_HEAD_CHARS: usize = 4_000;
const ELISION: &str = "\n[... elided ...]\n";
/// The request is context for the reply, not the thing being summarized.
const REQUEST_MAX_CHARS: usize = 500;
/// The prompt asks for 120 characters. Models overshoot a little, so the hard
/// stop matches the quoted fallback's budget rather than chopping a line that
/// is only slightly long.
const SUMMARY_MAX_CHARS: usize = SNIPPET_MAX_CHARS;
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(3);
/// A cap, not a target: the prompt asks for one line. It also bounds the
/// damage if the model ignores that.
const SUMMARY_MAX_TOKENS: u32 = 512;
const SUMMARY_SYSTEM: &str = "You write one-line desktop notification bodies for a terminal coding agent. Given the user's request and the agent's reply, state what actually happened in at most 120 characters: the outcome, and what the user has to decide or do next if anything. Write plain text, no markdown, no quotes, no trailing period, no preamble. Never describe the reply itself (\"the assistant explains...\"); say what was done. Output the line and nothing else.";
const REQUEST_TAG: &str = "Request";
const REPLY_TAG: &str = "Reply";
const WAITING_NOTE: &str =
    "The run is parked waiting for the user; say what it needs, not that it finished.";

pub(crate) fn send(summary: impl Into<String>, body: impl Into<String>) {
    let summary = summary.into();
    let body = body.into();
    std::thread::spawn(move || {
        if let Err(e) = Notification::new()
            .appname(APP_NAME)
            .summary(&summary)
            .body(&body)
            .show()
        {
            warn!(error = %e, "desktop notification failed");
        }
    });
}

/// A one-line preview of a reply: markdown keeps its punctuation, but line
/// breaks and indentation collapse so lists and code do not blow the popup up.
pub(crate) fn snippet(text: &str) -> String {
    condense(text, SNIPPET_MAX_CHARS)
}

fn condense(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some((cut, _)) = flat.char_indices().nth(max_chars) else {
        return flat;
    };
    let head = &flat[..cut];
    let kept = match head.rsplit_once(' ') {
        Some((words, _)) if words.chars().count() >= max_chars / MIN_KEEP_DIVISOR => words,
        _ => head,
    };
    format!("{kept}{ELLIPSIS}")
}

/// `maki · maki · done`: which project, and whether it wants you back.
pub(crate) fn title(cwd: &str, status: &str) -> String {
    let project = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cwd);
    format!("{APP_NAME}{TITLE_SEPARATOR}{project}{TITLE_SEPARATOR}{status}")
}

/// Nothing to summarize when the quote already shows the whole reply.
pub(crate) fn worth_summarizing(reply: &str) -> bool {
    reply.chars().count() > SNIPPET_MAX_CHARS
}

/// Keeps the head and the tail, drops the middle: both ends carry the parts a
/// summary is made of.
fn clamp_reply(reply: &str) -> String {
    if reply.chars().count() <= REPLY_MAX_CHARS {
        return reply.to_string();
    }
    let head_end = char_index(reply, REPLY_HEAD_CHARS);
    let tail_len = REPLY_MAX_CHARS - REPLY_HEAD_CHARS;
    let tail_start = char_index(reply, reply.chars().count() - tail_len);
    format!("{}{ELISION}{}", &reply[..head_end], &reply[tail_start..])
}

fn char_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(index, _)| index)
}

pub(crate) fn summary_prompt(request: Option<&str>, reply: &str, waiting: bool) -> String {
    let mut prompt = String::new();
    if let Some(request) = request {
        prompt.push_str(&format!(
            "{REQUEST_TAG}: {}\n\n",
            condense(request, REQUEST_MAX_CHARS)
        ));
    }
    prompt.push_str(&format!("{REPLY_TAG}:\n{}", clamp_reply(reply)));
    if waiting {
        prompt.push_str(&format!("\n\n{WAITING_NOTE}"));
    }
    prompt
}

/// The model's line, or `None` if it was slow, unreachable, or empty. Builds
/// its own provider: notifications are rare enough that caching one would be
/// state kept alive for nothing.
pub(crate) async fn summarize(spec: String, timeouts: Timeouts, prompt: String) -> Option<String> {
    let line = future::or(request_summary(&spec, timeouts, prompt), async {
        smol::Timer::after(SUMMARY_TIMEOUT).await;
        Err("timed out".to_string())
    })
    .await;
    match line {
        Ok(text) if !text.trim().is_empty() => Some(condense(&text, SUMMARY_MAX_CHARS)),
        Ok(_) => None,
        Err(e) => {
            debug!(model = %spec, error = %e, "notification summary failed");
            None
        }
    }
}

async fn request_summary(spec: &str, timeouts: Timeouts, prompt: String) -> Result<String, String> {
    let mut model = Model::from_spec(spec).map_err(|e| e.to_string())?;
    let provider = from_model_async(&mut model, timeouts)
        .await
        .map_err(|e| e.to_string())?;
    model.max_output_tokens = Some(SUMMARY_MAX_TOKENS);
    let messages = [Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: prompt }],
        ..Message::default()
    }];
    let opts = RequestOptions {
        thinking: ThinkingConfig::Off,
        fast: false,
    }
    .clamped(&model);

    let (event_tx, event_rx) = flume::unbounded();
    let response = provider
        .stream_message(
            &model,
            &messages,
            SUMMARY_SYSTEM,
            &Value::Array(Vec::new()),
            &event_tx,
            opts,
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    drop(event_rx);

    Ok(response
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{
        ELISION, ELLIPSIS, REPLY_HEAD_CHARS, REPLY_MAX_CHARS, SNIPPET_MAX_CHARS, STATUS_DONE,
        STATUS_NEEDS_INPUT, WAITING_NOTE, clamp_reply, condense, summary_prompt, title,
        worth_summarizing,
    };

    const MAX: usize = 10;

    #[test_case("short", "short" ; "under_budget_is_untouched")]
    #[test_case("ten chars!", "ten chars!" ; "exact_budget_is_untouched")]
    #[test_case("one\n\ntwo", "one two" ; "whitespace_runs_collapse")]
    #[test_case("  padded  ", "padded" ; "outer_whitespace_is_dropped")]
    #[test_case("alpha bravo charlie", "alpha..." ; "cuts_on_a_word_boundary")]
    #[test_case("astonishinglylongword tail", "astonishin..." ; "long_token_falls_back_to_a_hard_cut")]
    #[test_case("", "" ; "empty_stays_empty")]
    fn condense_cases(input: &str, expected: &str) {
        assert_eq!(condense(input, MAX), expected);
    }

    #[test]
    fn multibyte_text_cuts_on_a_char_boundary() {
        let condensed = condense("héllo wörld ünicode", MAX);
        assert_eq!(condensed, format!("héllo{ELLIPSIS}"));
    }

    #[test_case("/home/james/Projects/tool/maki", STATUS_DONE, "maki · maki · done" ; "basename_and_status")]
    #[test_case("/srv", STATUS_NEEDS_INPUT, "maki · srv · needs input" ; "waiting_state")]
    #[test_case("/", STATUS_DONE, "maki · / · done" ; "root_has_no_basename")]
    fn title_cases(cwd: &str, status: &str, expected: &str) {
        assert_eq!(title(cwd, status), expected);
    }

    #[test_case(SNIPPET_MAX_CHARS, false ; "a_reply_the_quote_already_shows")]
    #[test_case(SNIPPET_MAX_CHARS + 1, true ; "anything_the_quote_would_cut")]
    fn worth_summarizing_cases(len: usize, expected: bool) {
        assert_eq!(worth_summarizing(&"x".repeat(len)), expected);
    }

    #[test]
    fn short_replies_reach_the_model_whole() {
        let reply = "line one\n\nline two".repeat(50);
        assert_eq!(clamp_reply(&reply), reply);
    }

    #[test]
    fn long_replies_keep_both_ends_and_drop_the_middle() {
        let reply = format!(
            "{}{}{}",
            "h".repeat(REPLY_HEAD_CHARS),
            "m".repeat(10_000),
            "t".repeat(REPLY_MAX_CHARS)
        );
        let clamped = clamp_reply(&reply);
        assert_eq!(clamped.chars().count(), REPLY_MAX_CHARS + ELISION.len());
        assert!(clamped.starts_with(&"h".repeat(REPLY_HEAD_CHARS)));
        assert!(clamped.ends_with(&"t".repeat(REPLY_MAX_CHARS - REPLY_HEAD_CHARS)));
    }

    #[test]
    fn prompt_carries_request_reply_and_waiting_note() {
        let prompt = summary_prompt(Some("fix the parser"), "parser fixed", true);
        assert!(prompt.contains("Request: fix the parser"));
        assert!(prompt.contains("Reply:\nparser fixed"));
        assert!(prompt.contains(WAITING_NOTE));
    }

    #[test]
    fn prompt_without_a_request_is_reply_only() {
        let prompt = summary_prompt(None, "done", false);
        assert_eq!(prompt, "Reply:\ndone");
    }
}
