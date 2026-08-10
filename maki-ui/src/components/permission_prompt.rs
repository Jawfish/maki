//! The permission takeover at the bottom of the screen. Every option names the
//! action and the scope it applies to, the risky detail (exact command, cwd,
//! affected paths) stays folded until asked for, and a choice acknowledges
//! itself before the tool gets a chance to start.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use maki_agent::permissions::{DEFAULT_DENY_GUIDANCE, PermissionAnswer, generalized_scopes};
use maki_config::{FILE_WRITE_TOOLS, ToolKey};

use crate::components::Overlay;
use crate::components::form::render_form;
use crate::components::hint_line;
use crate::components::is_ctrl;
use crate::components::keybindings::key;
use crate::components::layout::MESSAGE_INDENT;
use crate::components::marker::State;
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Permission Required ";

const ALLOW_ONCE_LABEL: &str = "Allow once";
const ALLOW_SESSION_LABEL: &str = "Allow for this session";
const ALLOW_PROJECT_LABEL: &str = "Always allow this pattern here";
const ALLOW_GLOBAL_LABEL: &str = "Always allow this pattern everywhere";
const DENY_ONCE_LABEL: &str = "Deny once";
const DENY_PROJECT_LABEL: &str = "Always deny this pattern here";
const DENY_GLOBAL_LABEL: &str = "Always deny this pattern everywhere";

const LABEL_TOOL: &str = "tool";
const LABEL_SCOPE: &str = "scope";
const LABEL_COMMAND: &str = "command";
const LABEL_PATH: &str = "path";
const LABEL_CWD: &str = "cwd";
const LABEL_ALLOW: &str = "allow";
const LABEL_GUIDE: &str = "guide";
const LABEL_WIDTH: usize = 8;

const SUBTASK_TAG: &str = "[subtask] ";
const ELLIPSIS: &str = "…";
/// Longest scope the folded prompt shows; the exact text lives in the detail.
const SUMMARY_WIDTH: usize = 56;

const SHOW_DETAIL_HINT: &str = "Show detail";
const HIDE_DETAIL_HINT: &str = "Hide detail";
const CONFIRM_PREFIX: &str = "Confirm: ";
const CONFIRM_KEYS: &str = "Enter / y";
const ANY_KEY: &str = "any";
const CANCEL_HINT: &str = "Cancel";
const DENY_HINT_KEYS: &[(&str, &str)] = &[("Enter", "Deny"), ("Esc", CANCEL_HINT)];

const ACK_ALLOWED: &str = "allowed";
const ACK_DENIED: &str = "denied";
const ACK_SEPARATOR: &str = " · ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    AllowOnce,
    AllowSession,
    AllowProject,
    AllowGlobal,
    DenyOnce,
    DenyProject,
    DenyGlobal,
}

impl Choice {
    const ALLOW: &'static [Self] = &[
        Self::AllowOnce,
        Self::AllowSession,
        Self::AllowProject,
        Self::AllowGlobal,
    ];
    const DENY: &'static [Self] = &[Self::DenyOnce, Self::DenyProject, Self::DenyGlobal];

    const fn key(self) -> char {
        match self {
            Self::AllowOnce => 'y',
            Self::AllowSession => 's',
            Self::AllowProject => 'a',
            Self::AllowGlobal => 'A',
            Self::DenyOnce => 'n',
            Self::DenyProject => 'd',
            Self::DenyGlobal => 'D',
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => ALLOW_ONCE_LABEL,
            Self::AllowSession => ALLOW_SESSION_LABEL,
            Self::AllowProject => ALLOW_PROJECT_LABEL,
            Self::AllowGlobal => ALLOW_GLOBAL_LABEL,
            Self::DenyOnce => DENY_ONCE_LABEL,
            Self::DenyProject => DENY_PROJECT_LABEL,
            Self::DenyGlobal => DENY_GLOBAL_LABEL,
        }
    }

    const fn answer(self) -> PermissionAnswer {
        match self {
            Self::AllowOnce => PermissionAnswer::AllowOnce,
            Self::AllowSession => PermissionAnswer::AllowSession,
            Self::AllowProject => PermissionAnswer::AllowAlwaysLocal,
            Self::AllowGlobal => PermissionAnswer::AllowAlwaysGlobal,
            Self::DenyOnce => PermissionAnswer::Deny,
            Self::DenyProject => PermissionAnswer::DenyAlwaysLocal,
            Self::DenyGlobal => PermissionAnswer::DenyAlwaysGlobal,
        }
    }

    const fn is_allow(self) -> bool {
        matches!(
            self,
            Self::AllowOnce | Self::AllowSession | Self::AllowProject | Self::AllowGlobal
        )
    }

    fn from_key(code: KeyCode) -> Option<Self> {
        let KeyCode::Char(c) = code else {
            return None;
        };
        Self::ALLOW
            .iter()
            .chain(Self::DENY)
            .copied()
            .find(|choice| choice.key() == c)
    }

    /// The one-shot choices act immediately; the sticky ones ask again first.
    const fn needs_confirm(self) -> bool {
        !matches!(self, Self::AllowOnce | Self::DenyOnce)
    }

    fn acknowledgment(self) -> Line<'static> {
        let (state, word) = if self.is_allow() {
            (
                State::Running,
                format!("{ACK_ALLOWED}{ACK_SEPARATOR}{}", State::Running.word()),
            )
        } else {
            (State::Cancelled, ACK_DENIED.to_owned())
        };
        Line::from(vec![
            Span::raw(MESSAGE_INDENT),
            state.glyph_span(0),
            Span::styled(word, state.style()),
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PromptState {
    #[default]
    Normal,
    Confirm(Choice),
    DenyEditing,
}

pub struct Request {
    #[allow(dead_code)]
    id: String,
    tool: ToolKey,
    scopes: Vec<String>,
    cwd: String,
    subagent_id: Option<String>,
    allow_scopes: Vec<String>,
    state: PromptState,
    detail: bool,
    buffer: TextBuffer,
}

pub enum PermissionPrompt {
    Closed,
    Open(Box<Request>),
    Acknowledged(Choice),
}

impl Overlay for PermissionPrompt {
    fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    fn is_modal(&self) -> bool {
        false
    }

    fn close(&mut self) {
        *self = Self::Closed;
    }
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self::Closed
    }

    pub fn open(
        &mut self,
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        cwd: String,
        subagent_id: Option<String>,
    ) {
        let allow_scopes = generalized_scopes(&tool, &scopes);
        let allow_scopes = if allow_scopes == scopes {
            vec![]
        } else {
            allow_scopes
        };
        *self = Self::Open(Box::new(Request {
            id,
            tool,
            scopes,
            cwd,
            subagent_id,
            allow_scopes,
            state: PromptState::Normal,
            detail: false,
            buffer: TextBuffer::new(String::new()),
        }));
    }

    /// True only while the prompt still owns the keyboard and blocks the run.
    pub fn is_awaiting(&self) -> bool {
        matches!(self, Self::Open(_))
    }

    /// Replaces the question with the outcome the moment the user picks, so
    /// the choice lands before the tool reports anything back.
    pub(crate) fn acknowledge(&mut self, choice: Choice) {
        *self = Self::Acknowledged(choice);
    }

    pub fn clear_acknowledgment(&mut self) {
        if matches!(self, Self::Acknowledged(_)) {
            self.close();
        }
    }

    pub fn subagent_id(&self) -> Option<&str> {
        match self {
            Self::Open(request) => request.subagent_id.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<(Choice, PermissionAnswer)> {
        let Self::Open(request) = self else {
            return None;
        };
        let (state, detail, buffer) =
            (&mut request.state, &mut request.detail, &mut request.buffer);
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            return Some((Choice::DenyOnce, PermissionAnswer::Deny));
        }
        if *state == PromptState::DenyEditing {
            return match key.code {
                KeyCode::Enter => {
                    let text = buffer.value().trim().to_string();
                    let answer = if text.is_empty() {
                        PermissionAnswer::Deny
                    } else {
                        PermissionAnswer::DenyWithGuidance(text)
                    };
                    Some((Choice::DenyOnce, answer))
                }
                KeyCode::Esc => {
                    *buffer = TextBuffer::new(String::new());
                    *state = PromptState::Normal;
                    None
                }
                _ => {
                    buffer.handle_key(key);
                    None
                }
            };
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        if let PromptState::Confirm(choice) = *state {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some((choice, choice.answer())),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            };
        }
        // Shift is not stripped for punctuation on every terminal, so the
        // detail toggle matches on the key alone.
        if key.code == key::PERMISSION_DETAIL.code {
            *detail = !*detail;
            return None;
        }
        match Choice::from_key(key.code) {
            Some(Choice::DenyOnce) => {
                *state = PromptState::DenyEditing;
                None
            }
            Some(choice) if choice.needs_confirm() => {
                *state = PromptState::Confirm(choice);
                None
            }
            Some(choice) => Some((choice, choice.answer())),
            None => None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Self::Open(request) = self else {
            return false;
        };
        let (state, buffer) = (&request.state, &mut request.buffer);
        if *state == PromptState::DenyEditing {
            buffer.insert_text(text);
            return true;
        }
        false
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let Self::Open(request) = self else {
            return match self {
                Self::Acknowledged(choice) => {
                    vec![Line::raw(""), choice.acknowledgment(), Line::raw("")]
                }
                _ => vec![],
            };
        };
        let Request {
            tool,
            scopes,
            cwd,
            subagent_id,
            allow_scopes,
            state,
            detail,
            buffer,
            ..
        } = request.as_ref();
        let t = theme::current();

        let mut tool_spans = field_spans(LABEL_TOOL);
        if subagent_id.is_some() {
            tool_spans.push(Span::styled(SUBTASK_TAG, t.item_desc));
        }
        tool_spans.push(Span::styled(tool.to_string(), value_style()));

        let mut lines = vec![Line::raw(""), Line::from(tool_spans)];
        if *detail {
            lines.extend(detail_lines(tool, scopes, cwd, allow_scopes));
        } else if let Some(summary) = scopes.first() {
            lines.push(field_line(LABEL_SCOPE, &truncate(summary, SUMMARY_WIDTH)));
        }

        if *state == PromptState::DenyEditing {
            lines.push(guidance_line(buffer));
        }

        lines.push(Line::raw(""));
        match *state {
            PromptState::Confirm(choice) => lines.push(hint_line(&[
                (CONFIRM_KEYS, format!("{CONFIRM_PREFIX}{}", choice.label())),
                (ANY_KEY, CANCEL_HINT.to_owned()),
            ])),
            PromptState::DenyEditing => lines.push(hint_line(DENY_HINT_KEYS)),
            PromptState::Normal => lines.extend(option_rows(*detail)),
        }
        lines.push(Line::raw(""));
        lines
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_open() {
            return;
        }
        let lines = self.build_lines();
        let t = theme::current();
        render_form(&t, TITLE, frame, area, lines, (0, 0));
    }

    pub fn height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2);
        let lines = self.build_lines();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        para.line_count(inner_width) as u16 + 2
    }
}

fn value_style() -> Style {
    Style::new().fg(theme::current().foreground)
}

fn field_spans(label: &str) -> Vec<Span<'static>> {
    vec![
        Span::raw(MESSAGE_INDENT),
        Span::styled(format!("{label:<LABEL_WIDTH$}"), theme::current().tool_dim),
    ]
}

fn field_line(label: &str, value: &str) -> Line<'static> {
    let mut spans = field_spans(label);
    spans.push(Span::styled(value.to_owned(), value_style()));
    Line::from(spans)
}

/// Repeats the label only on the first row so a list of scopes reads as one
/// field rather than as several.
fn field_lines(label: &str, values: &[String]) -> Vec<Line<'static>> {
    values
        .iter()
        .enumerate()
        .map(|(i, value)| field_line(if i == 0 { label } else { "" }, value))
        .collect()
}

fn detail_lines(
    tool: &ToolKey,
    scopes: &[String],
    cwd: &str,
    allow_scopes: &[String],
) -> Vec<Line<'static>> {
    let scope_label = if is_file_tool(tool) {
        LABEL_PATH
    } else {
        LABEL_COMMAND
    };
    let mut lines = field_lines(scope_label, scopes);
    lines.push(field_line(LABEL_CWD, cwd));
    lines.extend(field_lines(LABEL_ALLOW, allow_scopes));
    lines
}

fn is_file_tool(tool: &ToolKey) -> bool {
    matches!(tool, ToolKey::Native(name) if FILE_WRITE_TOOLS.contains(&name.as_ref()))
}

fn guidance_line(buffer: &TextBuffer) -> Line<'static> {
    let t = theme::current();
    let text = buffer.value();
    let (display_text, cursor_pos) = if text.is_empty() {
        (DEFAULT_DENY_GUIDANCE, 0)
    } else {
        (text.as_str(), TextBuffer::char_to_byte(&text, buffer.x()))
    };
    let (before, after) = display_text.split_at(cursor_pos);
    let mut chars = after.chars();
    let cursor_ch = chars.next().unwrap_or(' ');
    let rest: String = chars.collect();

    let mut spans = field_spans(LABEL_GUIDE);
    if text.is_empty() {
        spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
        spans.push(Span::styled(rest, t.tool_dim));
    } else {
        spans.push(Span::raw(before.to_string()));
        spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
        if !rest.is_empty() {
            spans.push(Span::raw(rest));
        }
    }
    Line::from(spans)
}

/// Allow options on one row, deny options on the next, detail toggle last.
fn option_rows(detail: bool) -> Vec<Line<'static>> {
    let row = |choices: &[Choice]| -> Vec<(String, &'static str)> {
        choices
            .iter()
            .map(|c| (c.key().to_string(), c.label()))
            .collect()
    };
    let detail_hint = if detail {
        HIDE_DETAIL_HINT
    } else {
        SHOW_DETAIL_HINT
    };
    let rows = [
        row(Choice::ALLOW),
        row(Choice::DENY),
        vec![(key::PERMISSION_DETAIL.label.to_owned(), detail_hint)],
    ];
    aligned_hint_rows(&rows)
}

fn aligned_hint_rows(rows: &[Vec<(String, &'static str)>]) -> Vec<Line<'static>> {
    let t = theme::current();
    let cell_len = |(key, desc): &(String, &str)| key.chars().count() + 1 + desc.chars().count();
    let mut col_widths = vec![0usize; rows.iter().map(Vec::len).max().unwrap_or(0)];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            col_widths[i] = col_widths[i].max(cell_len(cell));
        }
    }
    rows.iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(row.len() * 2);
            for (i, cell) in row.iter().enumerate() {
                spans.push(Span::styled(
                    format!("{MESSAGE_INDENT}{}", cell.0),
                    t.keybind_key,
                ));
                let pad = if i + 1 < row.len() {
                    col_widths[i].saturating_sub(cell_len(cell))
                } else {
                    0
                };
                spans.push(Span::styled(
                    format!(" {}{:pad$}", cell.1, "", pad = pad),
                    t.tool_dim,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let keep = width.saturating_sub(ELLIPSIS.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_agent::permissions::PermissionAnswer;
    use maki_config::ToolKey;
    use test_case::test_case;

    use super::{
        ACK_ALLOWED, ACK_DENIED, ALLOW_GLOBAL_LABEL, ALLOW_ONCE_LABEL, ALLOW_PROJECT_LABEL,
        ALLOW_SESSION_LABEL, Choice, DENY_GLOBAL_LABEL, DENY_ONCE_LABEL, DENY_PROJECT_LABEL,
        LABEL_COMMAND, LABEL_CWD, LABEL_PATH, PermissionPrompt, PromptState, Request,
    };
    use crate::components::keybindings::key;
    use crate::components::marker::State;

    const COMMAND: &str = "git push origin main";
    const PATH: &str = "/tmp/project/src/main.rs";
    const CWD: &str = "/tmp/project";
    const WRITE_TOOL: &str = "write";
    const BASH_TOOL: &str = "bash";
    const GUIDANCE: &str = "Use cat";
    const NOT_OPEN: &str = "expected an open prompt";

    fn prompt_for(tool: &str, scope: &str) -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.open(
            "id".into(),
            ToolKey::native(tool),
            vec![scope.into()],
            CWD.into(),
            None,
        );
        prompt
    }

    fn open_prompt() -> PermissionPrompt {
        prompt_for(BASH_TOOL, COMMAND)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn answer(prompt: &mut PermissionPrompt, code: KeyCode) -> Option<PermissionAnswer> {
        prompt.handle_key(key(code)).map(|(_, answer)| answer)
    }

    fn text(prompt: &PermissionPrompt) -> String {
        prompt
            .build_lines()
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|s| s.content.to_string())
            .collect()
    }

    #[test_case(Choice::AllowOnce, ALLOW_ONCE_LABEL ; "allow_once")]
    #[test_case(Choice::AllowSession, ALLOW_SESSION_LABEL ; "allow_session")]
    #[test_case(Choice::AllowProject, ALLOW_PROJECT_LABEL ; "allow_project")]
    #[test_case(Choice::AllowGlobal, ALLOW_GLOBAL_LABEL ; "allow_global")]
    #[test_case(Choice::DenyOnce, DENY_ONCE_LABEL ; "deny_once")]
    #[test_case(Choice::DenyProject, DENY_PROJECT_LABEL ; "deny_project")]
    #[test_case(Choice::DenyGlobal, DENY_GLOBAL_LABEL ; "deny_global")]
    fn option_label_names_action_and_scope(choice: Choice, expected: &str) {
        assert_eq!(choice.label(), expected);
        let rendered = text(&open_prompt());
        assert!(rendered.contains(expected), "{rendered}");
    }

    #[test]
    fn detail_is_folded_until_asked_for() {
        let mut prompt = open_prompt();
        let folded = text(&prompt);
        assert!(!folded.contains(CWD), "{folded}");

        prompt.handle_key(key::PERMISSION_DETAIL.to_key_event());
        let expanded = text(&prompt);
        assert!(expanded.contains(COMMAND), "{expanded}");
        assert!(expanded.contains(LABEL_COMMAND), "{expanded}");
        assert!(expanded.contains(CWD), "{expanded}");
    }

    #[test]
    fn detail_labels_paths_for_file_tools() {
        let mut prompt = prompt_for(WRITE_TOOL, PATH);
        prompt.handle_key(key::PERMISSION_DETAIL.to_key_event());
        let expanded = text(&prompt);
        assert!(expanded.contains(LABEL_PATH), "{expanded}");
        assert!(expanded.contains(PATH), "{expanded}");
        assert!(expanded.contains(LABEL_CWD), "{expanded}");
    }

    #[test]
    fn allow_acknowledges_as_running() {
        let mut prompt = open_prompt();
        let (choice, answer) = prompt.handle_key(key(KeyCode::Char('y'))).expect("answer");
        assert_eq!(answer, PermissionAnswer::AllowOnce);
        prompt.acknowledge(choice);
        let rendered = text(&prompt);
        assert!(rendered.contains(ACK_ALLOWED), "{rendered}");
        assert!(rendered.contains(State::Running.word()), "{rendered}");
    }

    #[test]
    fn deny_acknowledges_as_denied() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        let (choice, _) = prompt.handle_key(key(KeyCode::Enter)).expect("answer");
        prompt.acknowledge(choice);
        let rendered = text(&prompt);
        assert!(rendered.contains(ACK_DENIED), "{rendered}");
        assert!(rendered.contains(State::Cancelled.glyph()), "{rendered}");
    }

    #[test]
    fn acknowledgment_clears_but_a_live_prompt_stays() {
        let mut prompt = open_prompt();
        prompt.clear_acknowledgment();
        assert!(prompt.is_awaiting());
        prompt.acknowledge(Choice::AllowOnce);
        assert!(!prompt.is_awaiting());
        prompt.clear_acknowledgment();
        assert!(matches!(prompt, PermissionPrompt::Closed));
    }

    #[test_case('s', PermissionAnswer::AllowSession ; "session")]
    #[test_case('a', PermissionAnswer::AllowAlwaysLocal ; "allow_project")]
    #[test_case('A', PermissionAnswer::AllowAlwaysGlobal ; "allow_global")]
    #[test_case('d', PermissionAnswer::DenyAlwaysLocal ; "deny_project")]
    #[test_case('D', PermissionAnswer::DenyAlwaysGlobal ; "deny_global")]
    fn sticky_choices_confirm_before_answering(pressed: char, expected: PermissionAnswer) {
        let mut prompt = open_prompt();
        assert_eq!(answer(&mut prompt, KeyCode::Char(pressed)), None);
        let confirming = text(&prompt);
        assert!(confirming.contains(expected_label(pressed)), "{confirming}");
        assert_eq!(answer(&mut prompt, KeyCode::Enter), Some(expected));
    }

    fn expected_label(pressed: char) -> &'static str {
        Choice::ALLOW
            .iter()
            .chain(Choice::DENY)
            .find(|c| c.key() == pressed)
            .expect("known key")
            .label()
    }

    fn request(prompt: &PermissionPrompt) -> &Request {
        match prompt {
            PermissionPrompt::Open(request) => request,
            _ => panic!("{NOT_OPEN}"),
        }
    }

    #[test]
    fn ctrl_c_denies() {
        let mut prompt = open_prompt();
        assert_eq!(
            prompt.handle_key(ctrl_c()).map(|(_, a)| a),
            Some(PermissionAnswer::Deny)
        );
        let mut editing = open_prompt();
        editing.handle_key(key(KeyCode::Char('n')));
        editing.handle_key(key(KeyCode::Char('t')));
        assert_eq!(
            editing.handle_key(ctrl_c()).map(|(_, a)| a),
            Some(PermissionAnswer::Deny)
        );
    }

    #[test]
    fn n_goes_to_deny_editing() {
        let mut prompt = open_prompt();
        assert_eq!(answer(&mut prompt, KeyCode::Char('n')), None);
        assert_eq!(request(&prompt).state, PromptState::DenyEditing);
    }

    #[test]
    fn deny_editing_esc_returns_to_normal() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_key(key(KeyCode::Char('t')));
        assert_eq!(answer(&mut prompt, KeyCode::Esc), None);
        assert_eq!(request(&prompt).state, PromptState::Normal);
        assert!(request(&prompt).buffer.value().is_empty());
    }

    #[test]
    fn deny_editing_enter_empty_sends_deny() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            answer(&mut prompt, KeyCode::Enter),
            Some(PermissionAnswer::Deny)
        );
    }

    #[test]
    fn deny_editing_with_text_sends_guidance() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste(GUIDANCE);
        assert_eq!(
            answer(&mut prompt, KeyCode::Enter),
            Some(PermissionAnswer::DenyWithGuidance(GUIDANCE.into()))
        );
    }

    #[test]
    fn handle_paste_requires_editing_mode() {
        let mut prompt = open_prompt();
        assert!(!prompt.handle_paste("ignored"));
        prompt.handle_key(key(KeyCode::Char('n')));
        assert!(prompt.handle_paste(GUIDANCE));
        assert_eq!(request(&prompt).buffer.value(), GUIDANCE);
    }

    #[test]
    fn wildcard_tool_key_opens() {
        let mut prompt = PermissionPrompt::new();
        prompt.open("id".into(), ToolKey::Wildcard, vec![], CWD.into(), None);
        assert!(prompt.is_awaiting());
    }
}
