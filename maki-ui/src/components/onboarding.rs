//! The blank session block: what maki does, plus a few paths the user can
//! take right away. Keys come from the keybinding table, so the block never
//! teaches a key that moved.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::components::keybindings::{KEYBINDS, ResolvedLabel};
use crate::components::layout::{MESSAGE_INDENT, SPACING_BLOCK, SPACING_SECTION};
use crate::theme;

const VALUE_FIRST_RUN: &str = "maki reads your code, edits files and runs commands.";
const VALUE_RETURNING: &str = "New session. Same project, empty context.";

const SUBMIT_BIND: &str = "Submit prompt";
const PALETTE_BIND: &str = "Open command palette";
const FILE_PICKER_BIND: &str = "File picker";
const HELP_BIND: &str = "Show keybindings";

const FIRST_RUN_PATHS: &[(&str, &str)] = &[
    (SUBMIT_BIND, "send a task, like \"explain this repo\""),
    (PALETTE_BIND, "run a command"),
    (FILE_PICKER_BIND, "insert a file path"),
    (HELP_BIND, "see every key"),
];

const RETURNING_PATHS: &[(&str, &str)] = &[
    (PALETTE_BIND, "run a command"),
    (FILE_PICKER_BIND, "insert a file path"),
    (HELP_BIND, "see every key"),
];

const KEY_GAP: usize = 2;
const MIN_WIDTH: u16 = 40;

pub(crate) struct Onboarding {
    first_run: bool,
    spent: bool,
}

impl Onboarding {
    pub(crate) fn new(first_run: bool) -> Self {
        Self {
            first_run,
            spent: false,
        }
    }

    /// Drawn only while the session is blank, and never again once it holds
    /// anything.
    pub(crate) fn view(&mut self, frame: &mut Frame, area: Rect, session_empty: bool) {
        if !session_empty {
            self.spent = true;
        }
        if self.spent || area.width < MIN_WIDTH {
            return;
        }
        let lines = self.lines();
        let height = lines.len() as u16;
        let bottom_gap = SPACING_SECTION as u16;
        if area.height < height + bottom_gap {
            return;
        }
        let width = lines.iter().map(line_width).max().unwrap_or(0) as u16;
        let rect = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height - height - bottom_gap,
            width: width.min(area.width),
            height,
        };
        frame.render_widget(Paragraph::new(lines), rect);
    }

    fn paths(&self) -> &'static [(&'static str, &'static str)] {
        if self.first_run {
            FIRST_RUN_PATHS
        } else {
            RETURNING_PATHS
        }
    }

    fn lines(&self) -> Vec<Line<'static>> {
        let t = theme::current();
        let value = if self.first_run {
            VALUE_FIRST_RUN
        } else {
            VALUE_RETURNING
        };
        let paths: Vec<(&str, &str)> = self
            .paths()
            .iter()
            .filter_map(|&(bind, text)| Some((key_label(bind)?, text)))
            .collect();
        let key_col = paths
            .iter()
            .map(|(key, _)| UnicodeWidthStr::width(*key))
            .max()
            .unwrap_or(0)
            + KEY_GAP;

        let mut lines = vec![Line::from(Span::styled(value.to_owned(), t.assistant))];
        lines.extend(vec![Line::default(); SPACING_BLOCK]);
        lines.extend(paths.into_iter().map(|(key, text)| {
            let pad = key_col.saturating_sub(UnicodeWidthStr::width(key));
            Line::from(vec![
                Span::raw(MESSAGE_INDENT),
                Span::styled(key.to_owned(), t.keybind_key),
                Span::raw(" ".repeat(pad)),
                Span::styled(text.to_owned(), t.tool_dim),
            ])
        }));
        lines
    }
}

/// The label the keybinding table itself shows for a bind, so the block and
/// the help modal can never disagree.
fn key_label(description: &str) -> Option<&'static str> {
    let bind = KEYBINDS
        .iter()
        .find(|b| b.description == description && b.platform.is_visible())?;
    match bind.label.resolve() {
        ResolvedLabel::Single(label) | ResolvedLabel::Alt(label, _) => Some(label),
        ResolvedLabel::Multi(labels) => labels.first().copied(),
    }
}

fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::keybindings::key;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const MIN_PATHS: usize = 2;
    const MAX_PATHS: usize = 4;
    const WIDTH: u16 = 70;
    const HEIGHT: u16 = 20;
    const EXAMPLE_PROMPT: &str = "explain this repo";

    fn rendered(onboarding: &mut Onboarding, session_empty: bool) -> String {
        let backend = TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| onboarding.view(frame, frame.area(), session_empty))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn first_run_leads_with_the_value_line_and_an_example_prompt() {
        let text = rendered(&mut Onboarding::new(true), true);
        assert!(text.contains(VALUE_FIRST_RUN), "{text}");
        assert!(text.contains(EXAMPLE_PROMPT), "{text}");
        assert!(!text.contains(VALUE_RETURNING), "{text}");
    }

    #[test]
    fn routine_session_drops_the_first_run_copy() {
        let text = rendered(&mut Onboarding::new(false), true);
        assert!(text.contains(VALUE_RETURNING), "{text}");
        assert!(!text.contains(VALUE_FIRST_RUN), "{text}");
        assert!(!text.contains(EXAMPLE_PROMPT), "{text}");
    }

    #[test_case(true; "first run")]
    #[test_case(false; "routine")]
    fn key_hints_come_from_the_real_binds(first_run: bool) {
        let text = rendered(&mut Onboarding::new(first_run), true);
        for label in [key::FILE_PICKER.label, key::HELP.label] {
            assert!(text.contains(label), "missing {label} in {text}");
        }
        let paths = Onboarding::new(first_run).paths().len();
        assert!(
            (MIN_PATHS..=MAX_PATHS).contains(&paths),
            "{paths} paths offered"
        );
    }

    #[test]
    fn content_retires_the_block_for_good() {
        let mut onboarding = Onboarding::new(true);
        assert!(rendered(&mut onboarding, true).contains(VALUE_FIRST_RUN));

        assert!(!rendered(&mut onboarding, false).contains(VALUE_FIRST_RUN));
        assert!(
            !rendered(&mut onboarding, true).contains(VALUE_FIRST_RUN),
            "an emptied session does not bring it back"
        );
    }
}
