//! One line above the history while the agent runs: what it was asked to do,
//! what it is doing now, how long it has been at it, and anything blocking it.
//! It is the run's single animated locus, so history markers freeze while it
//! is up. Idle sessions never reserve the row.

use std::time::{Duration, Instant};

use ratatui::text::{Line, Span};

use super::RetryInfo;
use crate::components::marker::State;
use crate::components::timing::format_duration;
use crate::theme;

pub(crate) const TASK_LINE_ROWS: u16 = 1;

const FIELD_SEPARATOR: &str = " · ";
const ELLIPSIS: &str = "…";
const PHASE_STREAMING: &str = "streaming";
const PHASE_WAITING: &str = "waiting";
const NEEDS_APPROVAL: &str = "needs approval";
const RETRY_PREFIX: &str = "retrying in ";
const RETRY_SECS: &str = "s";
const ATTEMPT_PREFIX: &str = " (#";
const ATTEMPT_SUFFIX: &str = ")";
/// Below this many cells left over the goal reads as noise, so it is dropped.
const MIN_GOAL_WIDTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Blocking {
    Permission,
    Retry { attempt: u32, secs: u64 },
}

impl Blocking {
    fn text(&self) -> String {
        match self {
            Self::Permission => NEEDS_APPROVAL.to_owned(),
            Self::Retry { attempt, secs } => {
                format!("{RETRY_PREFIX}{secs}{RETRY_SECS}{ATTEMPT_PREFIX}{attempt}{ATTEMPT_SUFFIX}")
            }
        }
    }
}

/// Everything the line needs, read straight off the app state each frame.
pub(crate) struct TaskLineInput<'a> {
    pub enabled: bool,
    pub streaming: bool,
    pub goal: &'a str,
    pub tool: Option<&'a str>,
    pub elapsed: Option<Duration>,
    pub permission_pending: bool,
    pub retry: Option<&'a RetryInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskLine {
    goal: String,
    phase: String,
    elapsed: Option<Duration>,
    blocking: Option<Blocking>,
}

impl TaskLine {
    /// `None` means no row is reserved at all: the agent is idle or the user
    /// turned the line off.
    pub(crate) fn build(input: TaskLineInput<'_>) -> Option<Self> {
        if !input.enabled || !input.streaming {
            return None;
        }
        let blocking = if input.permission_pending {
            Some(Blocking::Permission)
        } else {
            input.retry.map(|retry| Blocking::Retry {
                attempt: retry.attempt,
                secs: retry
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
            })
        };
        let phase = match (input.tool, blocking.is_some()) {
            (Some(tool), _) => tool.to_owned(),
            (None, true) => PHASE_WAITING.to_owned(),
            (None, false) => PHASE_STREAMING.to_owned(),
        };
        Some(Self {
            goal: input.goal.trim().to_owned(),
            phase,
            elapsed: input.elapsed,
            blocking,
        })
    }

    /// Phase, elapsed, and blocking state always fit; the goal takes whatever
    /// width is left and is cut short, or dropped, when that is too little.
    pub(crate) fn line(&self, width: u16) -> Line<'static> {
        let t = theme::current();
        let animation_millis = self.elapsed.map_or(0, |e| e.as_millis());
        let mut spans = vec![
            State::Running.glyph_span(animation_millis),
            Span::styled(self.phase.clone(), t.tool),
        ];
        let mut used = span_width(&spans);

        if let Some(elapsed) = self.elapsed {
            let text = format!("{FIELD_SEPARATOR}{}", format_duration(elapsed));
            used += text.chars().count();
            spans.push(Span::styled(text, t.status_dim));
        }
        if let Some(blocking) = &self.blocking {
            let state = State::NeedsAttention;
            let text = format!("{FIELD_SEPARATOR}{} {}", state.glyph(), blocking.text());
            used += text.chars().count();
            spans.push(Span::styled(text, state.style()));
        }

        let room = usize::from(width).saturating_sub(used + FIELD_SEPARATOR.chars().count());
        if !self.goal.is_empty() && room >= MIN_GOAL_WIDTH {
            spans.push(Span::styled(
                format!("{FIELD_SEPARATOR}{}", truncate(&self.goal, room)),
                t.status_dim,
            ));
        }
        Line::from(spans)
    }
}


fn span_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

fn truncate(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    let keep = room.saturating_sub(ELLIPSIS.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Blocking, ELLIPSIS, MIN_GOAL_WIDTH, NEEDS_APPROVAL, PHASE_STREAMING, PHASE_WAITING,
        RETRY_PREFIX, TaskLine, TaskLineInput,
    };
    use crate::components::RetryInfo;
    use crate::components::marker::State;
    use crate::components::timing::format_duration;
    use std::time::{Duration, Instant};
    use test_case::test_case;

    const GOAL: &str = "Fix the failing tests";
    const TOOL: &str = "bash";
    const ELAPSED: Duration = Duration::from_secs(12);
    const WIDE: u16 = 80;
    const NARROW: u16 = 24;
    const RETRY_ATTEMPT: u32 = 2;
    const RETRY_SECS: u64 = 5;
    const RETRY_MESSAGE: &str = "rate limited";

    fn input<'a>(goal: &'a str, streaming: bool, enabled: bool) -> TaskLineInput<'a> {
        TaskLineInput {
            enabled,
            streaming,
            goal,
            tool: Some(TOOL),
            elapsed: Some(ELAPSED),
            permission_pending: false,
            retry: None,
        }
    }

    fn text(line: &TaskLine, width: u16) -> String {
        line.line(width)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test]
    fn shows_goal_phase_and_elapsed_during_a_run() {
        let line = TaskLine::build(input(GOAL, true, true)).expect("visible during a run");
        let rendered = text(&line, WIDE);
        assert!(rendered.contains(GOAL), "{rendered}");
        assert!(rendered.contains(TOOL), "{rendered}");
        assert!(rendered.contains(&format_duration(ELAPSED)), "{rendered}");
    }

    #[test_case(false, true ; "idle_run")]
    #[test_case(true, false ; "disabled_by_config")]
    fn hidden_when_there_is_nothing_to_watch(streaming: bool, enabled: bool) {
        assert_eq!(TaskLine::build(input(GOAL, streaming, enabled)), None);
    }

    #[test]
    fn permission_pending_surfaces_as_blocking() {
        let line = TaskLine::build(TaskLineInput {
            permission_pending: true,
            ..input(GOAL, true, true)
        })
        .expect("visible during a run");
        let rendered = text(&line, WIDE);
        assert!(rendered.contains(NEEDS_APPROVAL), "{rendered}");
        assert!(rendered.contains(State::NeedsAttention.glyph()), "{rendered}");
    }

    #[test]
    fn retry_countdown_surfaces_as_blocking() {
        let retry = RetryInfo {
            attempt: RETRY_ATTEMPT,
            message: RETRY_MESSAGE.to_owned(),
            deadline: Instant::now() + Duration::from_secs(RETRY_SECS),
        };
        let line = TaskLine::build(TaskLineInput {
            tool: None,
            retry: Some(&retry),
            ..input(GOAL, true, true)
        })
        .expect("visible during a run");
        let rendered = text(&line, WIDE);
        assert!(rendered.contains(RETRY_PREFIX), "{rendered}");
        assert!(rendered.contains(PHASE_WAITING), "{rendered}");
    }

    #[test]
    fn streaming_without_a_tool_names_the_phase() {
        let line = TaskLine::build(TaskLineInput {
            tool: None,
            ..input(GOAL, true, true)
        })
        .expect("visible during a run");
        assert!(text(&line, WIDE).contains(PHASE_STREAMING));
    }

    #[test]
    fn narrow_width_truncates_the_goal_and_keeps_phase_and_elapsed() {
        let line = TaskLine::build(input(GOAL, true, true)).expect("visible during a run");
        let rendered = text(&line, NARROW);
        assert!(!rendered.contains(GOAL), "goal must not survive whole");
        assert!(rendered.contains(TOOL), "{rendered}");
        assert!(rendered.contains(&format_duration(ELAPSED)), "{rendered}");
        assert!(rendered.chars().count() <= usize::from(NARROW), "{rendered}");
    }

    #[test]
    fn goal_is_dropped_when_no_room_is_left() {
        let line = TaskLine::build(input(GOAL, true, true)).expect("visible during a run");
        let rendered = text(&line, u16::try_from(MIN_GOAL_WIDTH).unwrap());
        assert!(!rendered.contains(ELLIPSIS), "{rendered}");
        assert!(rendered.contains(TOOL), "{rendered}");
    }

    #[test]
    fn blocking_text_is_stable() {
        assert_eq!(Blocking::Permission.text(), NEEDS_APPROVAL);
        assert!(
            Blocking::Retry {
                attempt: RETRY_ATTEMPT,
                secs: RETRY_SECS,
            }
            .text()
            .starts_with(RETRY_PREFIX)
        );
    }
}
