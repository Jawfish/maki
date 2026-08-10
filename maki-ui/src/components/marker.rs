//! Single source of lifecycle-state presentation: every surface that shows
//! queued/running/done/failed/cancelled/needs-attention renders through here,
//! so no state is ever signalled by color alone.

use ratatui::style::Style;
use ratatui::text::Span;

use super::ToolStatus;
use crate::animation::{spinner_frame, spinner_str};
use crate::theme;

const GLYPH_GAP: &str = " ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    NeedsAttention,
}

impl From<ToolStatus> for State {
    fn from(status: ToolStatus) -> Self {
        match status {
            ToolStatus::InProgress => Self::Running,
            ToolStatus::Success => Self::Done,
            ToolStatus::Error => Self::Failed,
        }
    }
}

impl State {
    pub(crate) const fn glyph(self) -> &'static str {
        match self {
            Self::Queued => "◌",
            Self::Running => "●",
            Self::Done => "✔",
            Self::Failed => "✘",
            Self::Cancelled => "⊘",
            Self::NeedsAttention => "▲",
        }
    }

    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::NeedsAttention => "attention",
        }
    }

    /// Glyph and word together, for surfaces where the state is the message.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Queued => "◌ queued",
            Self::Running => "● running",
            Self::Done => "✔ done",
            Self::Failed => "✘ failed",
            Self::Cancelled => "⊘ cancelled",
            Self::NeedsAttention => "▲ attention",
        }
    }

    pub(crate) fn style(self) -> Style {
        let t = theme::current();
        match self {
            Self::Queued => t.queue,
            Self::Running => t.spinner,
            Self::Done => t.tool_success,
            Self::Failed => t.tool_error,
            Self::Cancelled => t.tool_dim,
            Self::NeedsAttention => t.status_notice,
        }
    }

    /// Glyph plus its trailing gap. `Running` animates on the elapsed time of
    /// whatever is running, so callers pass their own clock.
    pub(crate) fn glyph_text(self, elapsed_millis: u128) -> String {
        match self {
            Self::Running => format!("{}{GLYPH_GAP}", spinner_frame(elapsed_millis)),
            _ => format!("{}{GLYPH_GAP}", self.glyph()),
        }
    }

    pub(crate) fn glyph_span(self, elapsed_millis: u128) -> Span<'static> {
        Span::styled(self.glyph_text(elapsed_millis), self.style())
    }

    /// Glyph and word as separate spans so the animated glyph can still tick.
    pub(crate) fn label_spans(self, elapsed_millis: u128) -> [Span<'static>; 2] {
        [
            self.glyph_span(elapsed_millis),
            Span::styled(self.word(), self.style()),
        ]
    }
}

/// Turns whose side effects a workspace checkpoint cannot undo. They borrow
/// the attention glyph and style, with their own word.
pub(crate) const IRREVERSIBLE_WORD: &str = "irreversible";

pub(crate) fn irreversible_label() -> String {
    format!(
        "{}{GLYPH_GAP}{IRREVERSIBLE_WORD}",
        State::NeedsAttention.glyph()
    )
}

/// The running marker used by history tool headers. Only one thing on screen
/// animates: when the task line is up it owns the motion and the headers show
/// the static running glyph instead.
pub(crate) fn running_marker(elapsed_millis: u128, animated: bool) -> Span<'static> {
    let text = if animated {
        spinner_str(elapsed_millis).to_owned()
    } else {
        format!("{}{GLYPH_GAP}", State::Running.glyph())
    };
    Span::styled(text, State::Running.style())
}

#[cfg(test)]
mod tests {
    use super::{GLYPH_GAP, State, running_marker};
    use test_case::test_case;

    const RUNNING_MILLIS: u128 = 250;
    const ALL: &[State] = &[
        State::Queued,
        State::Running,
        State::Done,
        State::Failed,
        State::Cancelled,
        State::NeedsAttention,
    ];

    #[test_case(State::Queued ; "queued")]
    #[test_case(State::Running ; "running")]
    #[test_case(State::Done ; "done")]
    #[test_case(State::Failed ; "failed")]
    #[test_case(State::Cancelled ; "cancelled")]
    #[test_case(State::NeedsAttention ; "needs_attention")]
    fn label_is_glyph_and_word(state: State) {
        assert_eq!(
            state.label(),
            format!("{}{GLYPH_GAP}{}", state.glyph(), state.word())
        );
    }

    #[test]
    fn glyphs_are_unique_so_no_state_is_color_only() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.glyph(), b.glyph(), "{a:?} and {b:?} share a glyph");
                assert_ne!(a.word(), b.word(), "{a:?} and {b:?} share a word");
            }
        }
    }

    #[test]
    fn running_glyph_animates_while_others_are_static() {
        let a = State::Running.glyph_text(0);
        let b = State::Running.glyph_text(RUNNING_MILLIS);
        assert_ne!(a, b);
        assert_eq!(
            State::Done.glyph_text(RUNNING_MILLIS),
            State::Done.glyph_text(0)
        );
    }

    #[test]
    fn running_marker_freezes_when_it_defers_to_the_task_line() {
        let animated = running_marker(0, true).content;
        assert_ne!(animated, running_marker(RUNNING_MILLIS, true).content);
        assert_eq!(
            running_marker(RUNNING_MILLIS, false).content,
            running_marker(0, false).content
        );
        assert_eq!(
            running_marker(RUNNING_MILLIS, false).content,
            format!("{}{GLYPH_GAP}", State::Running.glyph())
        );
        assert_eq!(
            running_marker(0, false).content.chars().count(),
            animated.chars().count(),
            "static marker must keep the spinner's width"
        );
    }
}
