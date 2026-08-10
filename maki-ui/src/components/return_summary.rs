//! The block that ends a run, in its two shapes: the closure block right
//! after a turn, and the "since you left" summary replayed when the user
//! comes back to a run that finished or blocked while they were away.
//!
//! Both are built from turn telemetry alone, so the default path never calls
//! a model. With `ui.polish_summaries` on and `ui.notify_model` set, one prose
//! line is added by that weak model, reusing the notification summarizer.

use std::time::Duration;

use maki_config::UiConfig;
use maki_providers::Timeouts;
use ratatui::text::{Line, Span};

use crate::components::layout::MESSAGE_INDENT;
use crate::components::turn_telemetry::TurnTelemetry;
use crate::notify::{summarize, summary_prompt};
use crate::theme;

/// A run that ends after the user has been quiet this long counts as one that
/// ended while they were away: the scrollback is cold either way.
pub(crate) const IDLE_GAP: Duration = Duration::from_secs(300);

const RETURN_LEAD: &str = "since you left";

#[derive(Debug, Clone, PartialEq)]
pub struct SummaryBlock {
    telemetry: TurnTelemetry,
    returning: bool,
    prose: Option<String>,
}

impl SummaryBlock {
    pub(crate) fn closure(telemetry: TurnTelemetry) -> Self {
        Self {
            telemetry,
            returning: false,
            prose: None,
        }
    }

    pub(crate) fn returning(telemetry: TurnTelemetry) -> Self {
        Self {
            telemetry,
            returning: true,
            prose: None,
        }
    }

    pub(crate) fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = self
            .telemetry
            .lines(self.returning.then_some(RETURN_LEAD.to_owned()));
        if let Some(prose) = &self.prose {
            let style = theme::current().tool_dim;
            lines.push(Line::from(vec![
                Span::styled(MESSAGE_INDENT, style),
                Span::styled(prose.clone(), style),
            ]));
        }
        lines
    }

    pub(crate) fn search_text(&self) -> String {
        self.lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The weak model to write summary prose with, or `None` for the structural
/// block: the opt-in is off, or there is no model to opt in to.
pub(crate) fn polish_model(ui: &UiConfig) -> Option<String> {
    ui.polish_summaries
        .then(|| ui.notify_model.clone())
        .flatten()
}

/// Adds the prose line, and hands the block back untouched when the model is
/// slow, unreachable, or silent.
pub(crate) async fn polish(
    spec: String,
    timeouts: Timeouts,
    goal: String,
    mut block: SummaryBlock,
) -> SummaryBlock {
    let structural = block.search_text();
    let prompt = summary_prompt(
        (!goal.trim().is_empty()).then_some(goal.as_str()),
        &structural,
        false,
    );
    block.prose = summarize(spec, timeouts, prompt).await;
    block
}

#[cfg(test)]
mod tests {
    use super::{RETURN_LEAD, SummaryBlock, polish_model};
    use crate::components::turn_telemetry::TurnTelemetry;
    use maki_config::UiConfig;
    use test_case::test_case;

    const MODEL: &str = "openai/gpt-5-nano";

    fn config(polish: bool, model: Option<&str>) -> UiConfig {
        UiConfig {
            polish_summaries: polish,
            notify_model: model.map(str::to_owned),
            ..UiConfig::default()
        }
    }

    #[test_case(false, None, false ; "off_by_default")]
    #[test_case(false, Some(MODEL), false ; "a_notify_model_alone_polishes_nothing")]
    #[test_case(true, None, false ; "the_flag_alone_has_no_model_to_call")]
    #[test_case(true, Some(MODEL), true ; "flag_and_model_together_polish")]
    fn polish_model_cases(polish: bool, model: Option<&str>, expected: bool) {
        assert_eq!(polish_model(&config(polish, model)).is_some(), expected);
    }

    #[test]
    fn only_the_return_block_says_since_you_left() {
        let telemetry = TurnTelemetry::default();
        assert!(
            SummaryBlock::returning(telemetry.clone())
                .search_text()
                .contains(RETURN_LEAD)
        );
        assert!(
            !SummaryBlock::closure(telemetry)
                .search_text()
                .contains(RETURN_LEAD)
        );
    }
}
