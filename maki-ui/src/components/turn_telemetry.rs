//! Per-turn structural telemetry: what the run touched, derived only from
//! tool events already flowing through the UI. No model call, no shelling
//! out; the closure block at the end of a run is rendered from this alone.

use crate::components::layout::{MESSAGE_INDENT, right_align};
use crate::components::marker::State;
use crate::components::timing::format_duration;
use crate::theme;
use maki_agent::diff::{DiffLine, compute_hunks};
use maki_agent::tools::{BASH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME, TASK_TOOL_NAME};
use maki_agent::{ToolDoneEvent, ToolOutput, ToolStartEvent};
use maki_providers::add_cost;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};

/// A run closes with a summary when it edited anything, or when it ran enough
/// tools that the scrollback is no longer readable at a glance.
const MIN_EDITS_FOR_CLOSURE: usize = 1;
const MIN_TOOLS_FOR_CLOSURE: usize = 2;

const STAT_COL: usize = 9;
const FIELD_GAP: &str = "  ";
const COST_DECIMALS: usize = 3;
const ADDED_SIGN: &str = "+";
const REMOVED_SIGN: &str = "-";
const FILE_WORD: &str = "file";
const COMMAND_WORD: &str = "command";
const SUBAGENT_WORD: &str = "subagent";
const PLURAL: &str = "s";
const NO_CHANGES: &str = "no file changes";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileChange {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandRun {
    pub summary: String,
    pub failed: bool,
}

/// Everything a closure block needs, accumulated while the run happens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnTelemetry {
    files: Vec<FileChange>,
    commands: Vec<CommandRun>,
    subagents: Vec<String>,
    /// Tools that started and never reported back: the run ended with those
    /// decisions still open.
    pending: Vec<String>,
    /// Start order matters: the newest still-running tool is the phase the
    /// task line reports.
    running: Vec<(String, String)>,
    tool_count: usize,
    cost: Option<f64>,
    started_at: Option<Instant>,
    elapsed: Option<Duration>,
}

impl TurnTelemetry {
    pub(crate) fn started() -> Self {
        Self {
            started_at: Some(Instant::now()),
            ..Self::default()
        }
    }

    pub(crate) fn record_start(&mut self, event: &ToolStartEvent) {
        let label = describe(&event.tool, &event.summary);
        if event.tool.as_ref() == TASK_TOOL_NAME {
            self.subagents.push(label.clone());
        }
        self.running.push((event.id.clone(), label));
    }

    pub(crate) fn record_done(&mut self, event: &ToolDoneEvent) {
        let label = match self.running.iter().position(|(id, _)| *id == event.id) {
            Some(i) => self.running.remove(i).1,
            None => event.tool.to_string(),
        };
        self.tool_count += 1;
        if is_command(&event.tool) {
            self.commands.push(CommandRun {
                summary: label,
                failed: event.is_error,
            });
        }
        if !event.is_error {
            self.record_file_change(&event.output);
        }
    }

    pub(crate) fn record_cost(&mut self, cost: Option<f64>) {
        add_cost(&mut self.cost, cost);
    }

    /// The tool the run is waiting on right now, if any.
    pub(crate) fn current_tool(&self) -> Option<&str> {
        self.running.last().map(|(_, label)| label.as_str())
    }

    pub(crate) fn running_for(&self) -> Option<Duration> {
        self.started_at.map(|start| start.elapsed())
    }

    /// Freezes the run: whatever is still running is a pending decision.
    pub(crate) fn finish(&mut self) {
        self.elapsed = self.started_at.map(|start| start.elapsed());
        self.pending = self.running.drain(..).map(|(_, label)| label).collect();
        self.pending.sort();
    }

    pub(crate) fn qualifies(&self) -> bool {
        self.files.len() >= MIN_EDITS_FOR_CLOSURE || self.tool_count >= MIN_TOOLS_FOR_CLOSURE
    }

    pub(crate) fn closure_lines(&self) -> Vec<Line<'static>> {
        let t = theme::current();
        let mut lines = vec![Line::from(self.headline_spans())];
        for file in &self.files {
            lines.push(Line::from(vec![
                Span::styled(MESSAGE_INDENT, t.tool_dim),
                Span::styled(file.path.clone(), t.tool_path),
                Span::styled(
                    right_align(&format!("{ADDED_SIGN}{}", file.added), STAT_COL),
                    t.diff_new,
                ),
                Span::styled(
                    right_align(&format!("{REMOVED_SIGN}{}", file.removed), STAT_COL),
                    t.diff_old,
                ),
            ]));
        }
        for command in self.commands.iter().filter(|c| c.failed) {
            lines.push(marked_row(State::Failed, &command.summary));
        }
        for pending in &self.pending {
            lines.push(marked_row(State::NeedsAttention, pending));
        }
        lines
    }

    pub(crate) fn search_text(&self) -> String {
        self.closure_lines()
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

    fn headline_spans(&self) -> Vec<Span<'static>> {
        let state = if self.commands.iter().any(|c| c.failed) {
            State::Failed
        } else if self.pending.is_empty() {
            State::Done
        } else {
            State::NeedsAttention
        };
        let mut spans = state.label_spans(0).to_vec();
        for field in self.headline_fields() {
            spans.push(Span::styled(
                format!("{FIELD_GAP}{field}"),
                theme::current().tool_dim,
            ));
        }
        spans
    }

    fn headline_fields(&self) -> Vec<String> {
        let mut fields = Vec::new();
        if self.files.is_empty() {
            fields.push(NO_CHANGES.to_owned());
        } else {
            fields.push(count_word(self.files.len(), FILE_WORD));
        }
        if !self.commands.is_empty() {
            fields.push(count_word(self.commands.len(), COMMAND_WORD));
        }
        if !self.subagents.is_empty() {
            fields.push(count_word(self.subagents.len(), SUBAGENT_WORD));
        }
        if let Some(elapsed) = self.elapsed {
            fields.push(format_duration(elapsed));
        }
        if let Some(cost) = self.cost {
            fields.push(format!("${cost:.prec$}", prec = COST_DECIMALS));
        }
        fields
    }

    fn record_file_change(&mut self, output: &ToolOutput) {
        let (path, added, removed) = match output {
            ToolOutput::Diff {
                path, before, after, ..
            } => {
                let (added, removed) = diff_stat(before, after);
                (path.clone(), added, removed)
            }
            ToolOutput::WriteCode { path, lines, .. } => (path.clone(), lines.len(), 0),
            _ => return,
        };
        match self.files.iter_mut().find(|f| f.path == path) {
            Some(existing) => {
                existing.added += added;
                existing.removed += removed;
            }
            None => self.files.push(FileChange {
                path,
                added,
                removed,
            }),
        }
    }
}

fn marked_row(state: State, text: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(MESSAGE_INDENT, state.style())];
    spans.extend(state.label_spans(0));
    spans.push(Span::styled(
        format!("{FIELD_GAP}{text}"),
        theme::current().tool_dim,
    ));
    Line::from(spans)
}

fn is_command(tool: &str) -> bool {
    tool == BASH_TOOL_NAME || tool == CODE_EXECUTION_TOOL_NAME
}

fn describe(tool: &str, summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        tool.to_owned()
    } else {
        summary.to_owned()
    }
}

fn count_word(count: usize, word: &str) -> String {
    let plural = if count == 1 { "" } else { PLURAL };
    format!("{count} {word}{plural}")
}

fn diff_stat(before: &str, after: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for hunk in compute_hunks(before, after) {
        for line in hunk.lines {
            match line {
                DiffLine::Added(_) => added += 1,
                DiffLine::Removed(_) => removed += 1,
                DiffLine::Unchanged(_) => {}
            }
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::{MIN_TOOLS_FOR_CLOSURE, NO_CHANGES, TurnTelemetry};
    use crate::components::marker::State;
    use maki_agent::tools::{BASH_TOOL_NAME, EDIT_TOOL_NAME, QUESTION_TOOL_NAME, TASK_TOOL_NAME};
    use maki_agent::{ToolDoneEvent, ToolOutput, ToolStartEvent};
    use ratatui::text::Line;
    use std::sync::Arc;
    use test_case::test_case;

    const PATH: &str = "src/main.rs";
    const BEFORE: &str = "one\ntwo\nthree\n";
    const AFTER: &str = "one\ntwo changed\nthree\nfour\n";
    const CMD: &str = "cargo test";
    const QUESTION: &str = "which port?";
    const SUBAGENT: &str = "review the diff";
    const EXPECTED_ADDED: &str = "+2";
    const EXPECTED_REMOVED: &str = "-1";

    fn start(id: &str, tool: &str, summary: &str) -> ToolStartEvent {
        ToolStartEvent {
            id: id.into(),
            tool: Arc::from(tool),
            summary: summary.into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }
    }

    fn done(id: &str, tool: &str, output: ToolOutput, is_error: bool) -> ToolDoneEvent {
        ToolDoneEvent {
            id: id.into(),
            tool: Arc::from(tool),
            output,
            is_error,
            annotation: None,
            written_path: None,
        }
    }

    fn edit_output() -> ToolOutput {
        ToolOutput::Diff {
            path: PATH.into(),
            before: BEFORE.into(),
            after: AFTER.into(),
            summary: String::new(),
        }
    }

    fn plain() -> ToolOutput {
        ToolOutput::Plain(String::new().into())
    }

    fn edited_turn() -> TurnTelemetry {
        let mut t = TurnTelemetry::started();
        t.record_start(&start("e1", EDIT_TOOL_NAME, ""));
        t.record_done(&done("e1", EDIT_TOOL_NAME, edit_output(), false));
        t
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn edits_aggregate_per_path_with_line_counts() {
        let mut t = edited_turn();
        t.record_start(&start("e2", EDIT_TOOL_NAME, ""));
        t.record_done(&done("e2", EDIT_TOOL_NAME, edit_output(), false));
        t.finish();
        let text = text_of(&t.closure_lines());
        assert!(text.contains(PATH), "{text}");
        assert!(text.contains("+4"), "{text}");
        assert!(text.contains("-2"), "{text}");
    }

    #[test]
    fn single_edit_reports_its_diffstat() {
        let mut t = edited_turn();
        t.finish();
        let text = text_of(&t.closure_lines());
        assert!(text.contains(EXPECTED_ADDED), "{text}");
        assert!(text.contains(EXPECTED_REMOVED), "{text}");
    }

    #[test]
    fn failed_command_and_subagent_are_recorded() {
        let mut t = TurnTelemetry::started();
        t.record_start(&start("b1", BASH_TOOL_NAME, CMD));
        t.record_done(&done("b1", BASH_TOOL_NAME, plain(), true));
        t.record_start(&start("t1", TASK_TOOL_NAME, SUBAGENT));
        t.record_done(&done("t1", TASK_TOOL_NAME, plain(), false));
        t.finish();
        let text = text_of(&t.closure_lines());
        assert!(text.contains(State::Failed.label()), "{text}");
        assert!(text.contains(CMD), "{text}");
        assert!(text.contains("1 subagent"), "{text}");
        assert!(text.contains(NO_CHANGES), "{text}");
    }

    #[test]
    fn unfinished_tool_becomes_a_pending_decision() {
        let mut t = edited_turn();
        t.record_start(&start("q1", QUESTION_TOOL_NAME, QUESTION));
        t.finish();
        let text = text_of(&t.closure_lines());
        assert!(text.contains(State::NeedsAttention.label()), "{text}");
        assert!(text.contains(QUESTION), "{text}");
    }

    #[test_case(0, false, false ; "nothing_ran")]
    #[test_case(1, false, false ; "single_read_only_tool")]
    #[test_case(MIN_TOOLS_FOR_CLOSURE, false, true ; "enough_tools")]
    #[test_case(0, true, true ; "any_edit")]
    fn qualifying_rule(plain_tools: usize, edited: bool, expected: bool) {
        let mut t = TurnTelemetry::started();
        for i in 0..plain_tools {
            let id = format!("p{i}");
            t.record_start(&start(&id, BASH_TOOL_NAME, CMD));
            t.record_done(&done(&id, BASH_TOOL_NAME, plain(), false));
        }
        if edited {
            t.record_start(&start("e1", EDIT_TOOL_NAME, ""));
            t.record_done(&done("e1", EDIT_TOOL_NAME, edit_output(), false));
        }
        t.finish();
        assert_eq!(t.qualifies(), expected);
    }

    #[test]
    fn cost_accumulates_across_turns() {
        const FIRST: f64 = 0.001;
        const SECOND: f64 = 0.002;
        let mut t = edited_turn();
        t.record_cost(Some(FIRST));
        t.record_cost(Some(SECOND));
        t.finish();
        assert!(t.search_text().contains("$0.003"), "{}", t.search_text());
    }
}
