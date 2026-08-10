use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::marker::irreversible_label;
use crate::theme;

use crossterm::event::KeyEvent;
use maki_providers::{Message, Role};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

const TITLE: &str = " Rewind ";
const PREVIEW_MAX_LEN: usize = 80;
pub(crate) const NO_TURNS_MSG: &str = "No user turns to rewind to";
const SESSION_HINT: &str = " session";
const FILES_HINT: &str = " session + files";
const ENTER_LABEL: &str = "  Enter";
const FILES_KEY_LABEL: &str = "  ";

/// What a rewind puts back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindMode {
    /// Truncate the session, leave the workspace alone.
    SessionOnly,
    /// Truncate the session and restore the pre-turn file snapshot.
    SessionAndFiles,
}

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled(ENTER_LABEL, t.keybind_key),
        Span::styled(SESSION_HINT, t.tool_dim),
        Span::styled(
            format!("{FILES_KEY_LABEL}{}", key::REWIND_FILES.label),
            t.keybind_key,
        ),
        Span::styled(FILES_HINT, t.tool_dim),
    ])
}

pub enum RewindPickerAction {
    Consumed,
    Select(RewindEntry, RewindMode),
    Close,
}

pub struct RewindEntry {
    pub turn_index: usize,
    pub prompt_preview: String,
    pub prompt_text: String,
    /// Glyph plus word for turns a file restore cannot fully undo.
    pub irreversible_mark: Option<String>,
}

impl PickerItem for RewindEntry {
    fn label(&self) -> &str {
        &self.prompt_preview
    }

    fn suffix(&self) -> Option<&str> {
        self.irreversible_mark.as_deref()
    }
}

pub struct RewindPicker {
    picker: ListPicker<RewindEntry>,
}

impl RewindPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(footer_line),
        }
    }

    /// `irreversible_from` is the earliest turn whose run had effects no file
    /// snapshot can revert; that turn and everything after it carry the mark,
    /// since rewinding to one of them replays over that work.
    pub fn open(
        &mut self,
        messages: &[Message],
        irreversible_from: Option<usize>,
    ) -> Result<(), String> {
        let mut turn_num = 0usize;
        let mut entries: Vec<RewindEntry> = Vec::new();
        for (msg_idx, msg) in messages.iter().enumerate() {
            if !matches!(msg.role, Role::User) || msg.is_observation() {
                continue;
            }
            let Some(full_text) = msg.user_text() else {
                continue;
            };
            turn_num += 1;
            let first_line = full_text.lines().next().unwrap_or("");
            let preview = if first_line.len() > PREVIEW_MAX_LEN {
                format!(
                    "{turn_num}: {}...",
                    &first_line[..first_line.floor_char_boundary(PREVIEW_MAX_LEN)]
                )
            } else {
                format!("{turn_num}: {first_line}")
            };
            entries.push(RewindEntry {
                turn_index: msg_idx,
                prompt_preview: preview,
                prompt_text: full_text.to_owned(),
                irreversible_mark: irreversible_from
                    .is_some_and(|from| msg_idx >= from)
                    .then(irreversible_label),
            });
        }
        if entries.is_empty() {
            return Err(NO_TURNS_MSG.into());
        }
        entries.reverse();
        self.picker.open(entries, TITLE);
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> RewindPickerAction {
        if key::REWIND_FILES.matches(key_event) {
            return match self.picker.take_selected() {
                Some(entry) => RewindPickerAction::Select(entry, RewindMode::SessionAndFiles),
                None => RewindPickerAction::Consumed,
            };
        }
        match self.picker.handle_key(key_event) {
            PickerAction::Consumed => RewindPickerAction::Consumed,
            PickerAction::Select(entry) => {
                RewindPickerAction::Select(entry, RewindMode::SessionOnly)
            }
            PickerAction::Close => RewindPickerAction::Close,
            PickerAction::Toggle(..) => RewindPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    #[cfg(test)]
    pub(crate) fn marks(&self) -> Vec<bool> {
        (0..)
            .map_while(|i| self.picker.item(i))
            .map(|entry| entry.irreversible_mark.is_some())
            .collect()
    }
}

impl Overlay for RewindPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_providers::ContentBlock;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    fn user_msg(text: &str) -> Message {
        Message::user(text.into())
    }

    fn assistant_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response".into(),
            }],
            ..Default::default()
        }
    }

    #[test_case(&[]                                          ; "empty_messages")]
    #[test_case(&[assistant_msg()]                            ; "no_user_turns")]
    #[test_case(&[Message::synthetic("continue".into())]     ; "only_synthetic")]
    fn open_without_user_turns_returns_error(msgs: &[Message]) {
        let mut picker = RewindPicker::new();
        assert_eq!(picker.open(msgs, None), Err(NO_TURNS_MSG.into()));
    }

    #[test]
    fn entries_are_in_reverse_order() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("first"),
            assistant_msg(),
            user_msg("second"),
            assistant_msg(),
            user_msg("third"),
        ];
        picker.open(&msgs, None).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("third"));
        assert_eq!(item.turn_index, 4);
    }

    #[test]
    fn long_prompt_is_truncated_in_preview() {
        let mut picker = RewindPicker::new();
        let long_text = "a".repeat(120);
        picker.open(&[user_msg(&long_text)], None).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().ends_with("..."));
        assert!(item.label().len() < 90);
        assert_eq!(item.prompt_text, long_text);
    }

    #[test]
    fn multiline_prompt_uses_first_line_for_preview() {
        let mut picker = RewindPicker::new();
        picker.open(&[user_msg("first line\nsecond line")], None).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("first line"));
        assert!(!item.label().contains("second"));
        assert_eq!(item.prompt_text, "first line\nsecond line");
    }

    #[test]
    fn display_text_overrides_content() {
        let mut picker = RewindPicker::new();
        let msg = Message::user_display("ai sees this".into(), "user typed this".into());
        picker.open(&[msg], None).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("user typed this"));
        assert_eq!(item.prompt_text, "user typed this");
    }

    #[test]
    fn synthetic_messages_and_observations_are_excluded() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            Message::observation("build failed".into()),
            user_msg("real prompt"),
            assistant_msg(),
            Message::synthetic("[Cancelled by user]".into()),
        ];
        picker.open(&msgs, None).unwrap();
        let item = picker.picker.selected_item().unwrap();
        assert!(item.label().contains("real prompt"));
        assert_eq!(item.turn_index, 1);
    }

    #[test]
    fn turn_numbers_skip_synthetic() {
        let mut picker = RewindPicker::new();
        let msgs = vec![
            user_msg("first"),
            assistant_msg(),
            Message::synthetic("continue".into()),
            assistant_msg(),
            user_msg("second"),
        ];
        picker.open(&msgs, None).unwrap();
        let top = picker.picker.selected_item().unwrap();
        assert!(top.label().starts_with("2: second"));
    }

    const IRREVERSIBLE_TURN: usize = 2;
    const PICKER_WIDTH: u16 = 60;
    const PICKER_HEIGHT: u16 = 12;

    fn three_turns() -> Vec<Message> {
        vec![
            user_msg("first"),
            assistant_msg(),
            user_msg("second"),
            assistant_msg(),
            user_msg("third"),
        ]
    }

    fn rendered_text(picker: &mut RewindPicker) -> String {
        let backend = ratatui::backend::TestBackend::new(PICKER_WIDTH, PICKER_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, Rect::new(0, 0, PICKER_WIDTH, PICKER_HEIGHT));
            })
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

    #[test_case(key::REWIND_FILES.to_key_event(), RewindMode::SessionAndFiles ; "files_key")]
    #[test_case(KeyEvent::from(KeyCode::Enter), RewindMode::SessionOnly ; "enter")]
    fn selection_key_picks_the_restore_mode(key_event: KeyEvent, expected: RewindMode) {
        let mut picker = RewindPicker::new();
        picker.open(&three_turns(), None).unwrap();
        let RewindPickerAction::Select(entry, mode) = picker.handle_key(key_event) else {
            panic!("expected a selection");
        };
        assert_eq!(mode, expected);
        assert_eq!(entry.prompt_text, "third");
        assert!(!picker.is_open());
    }

    #[test]
    fn irreversible_turns_carry_a_glyph_and_a_word() {
        let mut picker = RewindPicker::new();
        picker
            .open(&three_turns(), Some(IRREVERSIBLE_TURN))
            .unwrap();
        let marked: Vec<Option<&str>> = (0..3)
            .map(|i| picker.picker.item(i).unwrap().suffix())
            .collect();
        let mark = Some(irreversible_label());
        assert_eq!(marked[0], mark.as_deref(), "newest turn is irreversible");
        assert_eq!(marked[1], mark.as_deref(), "the marked turn itself");
        assert_eq!(marked[2], None, "turn before the side effect is clean");

        let text = rendered_text(&mut picker);
        assert!(
            text.contains(&irreversible_label()),
            "picker must show the mark; got: {text}"
        );
    }
}
