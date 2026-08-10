//! Desktop notifications, fired when a turn finishes or needs input while the
//! terminal window is unfocused. `notify_rust::Notification::show` blocks on
//! an IPC round trip (D-Bus on Linux/BSD), so it runs on its own thread
//! instead of stalling the event loop.
//!
//! Bodies are plain text. The XDG spec only allows markup when the server
//! advertises `body-markup`, and escaping unconditionally would print
//! `&amp;` on the servers that do not.

use notify_rust::Notification;
use tracing::warn;

const APP_NAME: &str = "maki";
const SNIPPET_MAX_CHARS: usize = 160;
const ELLIPSIS: &str = "...";
/// A word-boundary cut is only worth it while it keeps this fraction of the
/// budget, otherwise one very long token would shrink the body to nothing.
const MIN_KEEP_DIVISOR: usize = 2;

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

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::{ELLIPSIS, condense};

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
}
