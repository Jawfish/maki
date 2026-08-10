//! Just-in-time hints for features the UI cannot show on its own. One line,
//! at most one hint per session, gone for good once the user moves on. Every
//! key comes from the keybinding table, so a hint can never teach a key that
//! moved.

use std::collections::BTreeSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::keybindings::{KEYBINDS, ResolvedLabel};
use crate::components::layout::MESSAGE_INDENT;
use crate::theme;

pub(crate) const HINT_ROWS: u16 = 1;

/// A run has to be this long before queueing the next task is worth teaching.
pub(crate) const LONG_RUN_SECS: u64 = 30;

const KEY_SLOT: &str = "{key}";
const PREFIX: &str = "tip ";

const SEARCH_BIND: &str = "Search messages";
const REWIND_BIND: &str = "Rewind";
const SUBMIT_BIND: &str = "Submit prompt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    TruncatedOutput,
    IdleEsc,
    LongRun,
}

struct Hint {
    id: &'static str,
    trigger: Trigger,
    copy: &'static str,
    bind: &'static str,
}

const HINTS: &[Hint] = &[
    Hint {
        id: "truncated-output",
        trigger: Trigger::TruncatedOutput,
        copy: "That output is cut short. Click it to read all of it, or press {key} to search the session.",
        bind: SEARCH_BIND,
    },
    Hint {
        id: "idle-esc",
        trigger: Trigger::IdleEsc,
        copy: "Nothing to cancel right now. Press {key} to rewind the session to an earlier turn.",
        bind: REWIND_BIND,
    },
    Hint {
        id: "long-run",
        trigger: Trigger::LongRun,
        copy: "This one takes a while. Type your next task and press {key} to queue it.",
        bind: SUBMIT_BIND,
    },
];

/// Fires hints, remembers what the user has seen, and hands back the id the
/// caller should persist. The hint never takes a key of its own: the next key
/// press dismisses it, whatever that key was for.
pub(crate) struct Hints {
    enabled: bool,
    spent: BTreeSet<String>,
    active: Option<&'static Hint>,
}

impl Hints {
    pub(crate) fn new(enabled: bool, dismissed: BTreeSet<String>) -> Self {
        Self {
            enabled,
            spent: dismissed,
            active: None,
        }
    }

    /// Marks the hint spent as it fires, so a trigger that repeats in the same
    /// session stays quiet even before the user dismisses anything.
    pub(crate) fn fire(&mut self, trigger: Trigger) {
        if !self.enabled || self.active.is_some() {
            return;
        }
        let Some(hint) = HINTS
            .iter()
            .find(|h| h.trigger == trigger && !self.spent.contains(h.id))
        else {
            return;
        };
        if key_label(hint.bind).is_none() {
            return;
        }
        self.spent.insert(hint.id.to_owned());
        self.active = Some(hint);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Clears the line and reports the id to write to storage, so the hint
    /// stays gone in later sessions.
    pub(crate) fn dismiss(&mut self) -> Option<&'static str> {
        self.active.take().map(|hint| hint.id)
    }

    pub(crate) fn view(&self, frame: &mut Frame, area: Rect) {
        let Some(hint) = self.active else {
            return;
        };
        frame.render_widget(Paragraph::new(line(hint)), area);
    }
}

fn line(hint: &Hint) -> Line<'static> {
    let t = theme::current();
    let key = key_label(hint.bind).unwrap_or_default();
    let (before, after) = hint.copy.split_once(KEY_SLOT).unwrap_or((hint.copy, ""));
    Line::from(vec![
        Span::raw(MESSAGE_INDENT),
        Span::styled(PREFIX, t.status_notice),
        Span::styled(before.to_owned(), t.tool_dim),
        Span::styled(key.to_owned(), t.keybind_key),
        Span::styled(after.to_owned(), t.tool_dim),
    ])
}

fn key_label(description: &str) -> Option<&'static str> {
    let bind = KEYBINDS
        .iter()
        .find(|b| b.description == description && b.platform.is_visible())?;
    match bind.label.resolve() {
        ResolvedLabel::Single(label) | ResolvedLabel::Alt(label, _) => Some(label),
        ResolvedLabel::Multi(labels) => labels.first().copied(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const WIDTH: u16 = 120;
    const MAX_COPY_WIDTH: usize = 100;

    fn hints() -> Hints {
        Hints::new(true, BTreeSet::new())
    }

    fn rendered(hints: &Hints) -> String {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HINT_ROWS)).unwrap();
        terminal
            .draw(|frame| hints.view(frame, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.width)
            .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_owned()))
            .collect()
    }

    #[test_case(Trigger::TruncatedOutput; "truncated output")]
    #[test_case(Trigger::IdleEsc; "idle esc")]
    #[test_case(Trigger::LongRun; "long run")]
    fn a_trigger_fires_once_per_session(trigger: Trigger) {
        let mut hints = hints();
        hints.fire(trigger);
        assert!(hints.is_active(), "first {trigger:?} shows a hint");
        assert!(hints.dismiss().is_some());

        hints.fire(trigger);
        assert!(
            !hints.is_active(),
            "{trigger:?} stays quiet the second time"
        );
    }

    #[test_case(Trigger::TruncatedOutput; "truncated output")]
    #[test_case(Trigger::IdleEsc; "idle esc")]
    #[test_case(Trigger::LongRun; "long run")]
    fn a_dismissed_hint_stays_gone_after_a_restart(trigger: Trigger) {
        let mut first_run = hints();
        first_run.fire(trigger);
        let dismissed = first_run.dismiss().expect("hint was showing");

        let mut restarted = Hints::new(true, BTreeSet::from([dismissed.to_owned()]));
        restarted.fire(trigger);
        assert!(!restarted.is_active(), "{dismissed} came back");
    }

    #[test_case(Trigger::TruncatedOutput; "truncated output")]
    #[test_case(Trigger::IdleEsc; "idle esc")]
    #[test_case(Trigger::LongRun; "long run")]
    fn the_config_switch_suppresses_every_hint(trigger: Trigger) {
        let mut hints = Hints::new(false, BTreeSet::new());
        hints.fire(trigger);
        assert!(!hints.is_active(), "{trigger:?} fired with hints off");
        assert!(rendered(&hints).trim().is_empty());
    }

    #[test]
    fn only_one_hint_shows_at_a_time() {
        let mut hints = hints();
        hints.fire(Trigger::IdleEsc);
        hints.fire(Trigger::LongRun);
        let text = rendered(&hints);
        assert!(text.contains(key_label(REWIND_BIND).unwrap()), "{text}");

        hints.dismiss();
        hints.fire(Trigger::LongRun);
        assert!(hints.is_active(), "the queued trigger still gets its turn");
    }

    #[test]
    fn every_hint_names_its_real_key() {
        for hint in HINTS {
            let label = key_label(hint.bind).expect(hint.bind);
            let mut hints = hints();
            hints.fire(hint.trigger);
            let text = rendered(&hints);
            assert!(text.contains(label), "{} misses {label}: {text}", hint.id);
            assert!(hint.copy.contains(KEY_SLOT), "{} names no key", hint.id);
            assert!(
                hint.copy.len() < MAX_COPY_WIDTH,
                "{} is too long for one line",
                hint.id
            );
        }
    }
}
