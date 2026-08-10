use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fmt::Write;
use strum::EnumIter;
use unicode_width::UnicodeWidthStr;

macro_rules! mod_key {
    ($suffix:expr) => {
        concat!("Ctrl+", $suffix)
    };
}

macro_rules! upper {
    ('a') => {
        "A"
    };
    ('b') => {
        "B"
    };
    ('c') => {
        "C"
    };
    ('d') => {
        "D"
    };
    ('e') => {
        "E"
    };
    ('f') => {
        "F"
    };
    ('g') => {
        "G"
    };
    ('h') => {
        "H"
    };
    ('i') => {
        "I"
    };
    ('j') => {
        "J"
    };
    ('k') => {
        "K"
    };
    ('l') => {
        "L"
    };
    ('m') => {
        "M"
    };
    ('n') => {
        "N"
    };
    ('o') => {
        "O"
    };
    ('p') => {
        "P"
    };
    ('q') => {
        "Q"
    };
    ('r') => {
        "R"
    };
    ('s') => {
        "S"
    };
    ('t') => {
        "T"
    };
    ('u') => {
        "U"
    };
    ('v') => {
        "V"
    };
    ('w') => {
        "W"
    };
    ('x') => {
        "X"
    };
    ('y') => {
        "Y"
    };
    ('z') => {
        "Z"
    };
}

macro_rules! ctrl_bind {
    ($char:tt) => {
        Bind {
            code: KeyCode::Char($char),
            modifiers: KeyModifiers::CONTROL,
            label: mod_key!(upper!($char)),
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: &'static str,
}

impl Bind {
    pub fn matches(&self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    #[cfg(test)]
    pub const fn to_key_event(self) -> KeyEvent {
        KeyEvent {
            code: self.code,
            modifiers: self.modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }
}

pub mod key {
    use super::Bind;
    use crossterm::event::{KeyCode, KeyModifiers};

    pub const QUIT: Bind = ctrl_bind!('c');
    pub const HELP: Bind = ctrl_bind!('h');
    pub const PREV_CHAT: Bind = ctrl_bind!('p');
    pub const NEXT_CHAT: Bind = ctrl_bind!('n');
    pub const SCROLL_HALF_UP: Bind = ctrl_bind!('u');
    pub const SCROLL_HALF_DOWN: Bind = ctrl_bind!('d');
    pub const SCROLL_HALF_UP_ALT: Bind = Bind {
        code: KeyCode::Char('k'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+K",
    };
    pub const SCROLL_HALF_DOWN_ALT: Bind = Bind {
        code: KeyCode::Char('j'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+J",
    };
    pub const SCROLL_LINE_UP: Bind = ctrl_bind!('y');
    pub const SCROLL_LINE_DOWN: Bind = ctrl_bind!('e');
    pub const SCROLL_TOP: Bind = ctrl_bind!('g');
    pub const SCROLL_BOTTOM: Bind = ctrl_bind!('b');
    pub const POP_QUEUE: Bind = ctrl_bind!('q');
    pub const DELETE_WORD: Bind = ctrl_bind!('w');
    pub const SEARCH: Bind = ctrl_bind!('f');
    pub const FILE_PICKER: Bind = ctrl_bind!('s');
    pub const OPEN_EDITOR: Bind = ctrl_bind!('o');
    pub const PLAN_TOGGLE: Bind = ctrl_bind!('t');
    pub const TASKS: Bind = ctrl_bind!('x');
    pub const REFRESH: Bind = ctrl_bind!('r');
    pub const SUSPEND: Bind = ctrl_bind!('z');
    pub const DELETE: Bind = ctrl_bind!('d');
    pub const KILL_LINE: Bind = ctrl_bind!('k');
    pub const LINE_START: Bind = ctrl_bind!('a');
    pub const LINE_END: Bind = ctrl_bind!('e');
    pub const EDIT_INPUT: Bind = Bind {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+O",
    };
    /// Shares Ctrl+R with `REFRESH`: the usage modal intercepts it first,
    /// so refresh wins while that modal is open.
    pub const REVIEW: Bind = ctrl_bind!('r');
    /// Shares Ctrl+Y with `SCROLL_LINE_UP`: modals and pickers intercept it
    /// first, so line scroll wins while they are open.
    pub const TOGGLE_THINKING: Bind = ctrl_bind!('y');
    /// Folds the risk detail of a permission request in and out. Matched on
    /// the key alone, since terminals disagree on Shift for punctuation.
    pub const PERMISSION_DETAIL: Bind = Bind {
        code: KeyCode::Char('?'),
        modifiers: KeyModifiers::NONE,
        label: "?",
    };
    /// Shares Ctrl+F with `SEARCH`: only the rewind picker looks at it, and
    /// search is not reachable while that picker is open.
    pub const REWIND_FILES: Bind = ctrl_bind!('f');
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum KeybindContext {
    General,
    Editing,
    Streaming,
    Picker,
    FormInput,
    TaskPicker,
    RewindPicker,
    ThemePicker,
    ModelPicker,
    QueueFocus,
    CommandPalette,
    Search,
    FilePicker,
    Permission,
}

impl KeybindContext {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Streaming => "While Streaming",
            Self::Picker => "Pickers",
            Self::FormInput => "Form",
            Self::TaskPicker => "Task Picker",
            Self::RewindPicker => "Rewind Picker",
            Self::ThemePicker => "Theme Picker",
            Self::ModelPicker => "Model Picker",
            Self::QueueFocus => "Queue",
            Self::CommandPalette => "Commands",
            Self::Search => "Search",
            Self::FilePicker => "File Picker",
            Self::Permission => "Permission Prompt",
        }
    }

    pub const fn parent(self) -> Option<KeybindContext> {
        match self {
            Self::TaskPicker
            | Self::RewindPicker
            | Self::ThemePicker
            | Self::ModelPicker
            | Self::QueueFocus
            | Self::CommandPalette
            | Self::Search
            | Self::FilePicker => Some(Self::Picker),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    All,
    MacOnly,
    UnixOnly,
}

impl Platform {
    pub const fn is_visible(self) -> bool {
        match self {
            Self::All => true,
            Self::MacOnly => cfg!(target_os = "macos"),
            Self::UnixOnly => cfg!(unix),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum KeyLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    /// Alt on Mac, Single (first) on other platforms
    MacAlt(&'static str, &'static str),
    /// Multi on Mac, Multi (first slice) on other platforms
    MacMulti(&'static [&'static str], &'static [&'static str]),
}

pub const ALT_SEP: &str = " / ";

#[derive(Debug, Clone, Copy)]
pub enum ResolvedLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    Multi(&'static [&'static str]),
}

impl ResolvedLabel {
    pub fn display_width(self) -> usize {
        match self {
            Self::Single(s) => UnicodeWidthStr::width(s),
            Self::Alt(a, b) => {
                let sep_w = UnicodeWidthStr::width(ALT_SEP);
                UnicodeWidthStr::width(a) + sep_w + UnicodeWidthStr::width(b)
            }
            Self::Multi(keys) => {
                let sep_w = UnicodeWidthStr::width(ALT_SEP);
                keys.iter()
                    .map(|k| UnicodeWidthStr::width(*k))
                    .sum::<usize>()
                    + sep_w * keys.len().saturating_sub(1)
            }
        }
    }
}

impl KeyLabel {
    pub fn resolve(self) -> ResolvedLabel {
        match self {
            Self::Single(s) => ResolvedLabel::Single(s),
            Self::Alt(a, b) => ResolvedLabel::Alt(a, b),
            Self::MacAlt(a, b) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Alt(a, b)
                } else {
                    ResolvedLabel::Single(a)
                }
            }
            Self::MacMulti(normal, mac) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Multi(mac)
                } else {
                    ResolvedLabel::Multi(normal)
                }
            }
        }
    }

    #[cfg(test)]
    fn flat_str(&self) -> String {
        match self.resolve() {
            ResolvedLabel::Single(s) => s.to_string(),
            ResolvedLabel::Alt(a, b) => format!("{a}/{b}"),
            ResolvedLabel::Multi(keys) => keys.join("/"),
        }
    }
}

pub struct Keybind {
    pub label: KeyLabel,
    pub description: &'static str,
    pub context: KeybindContext,
    pub platform: Platform,
}

pub const KEYBINDS: &[Keybind] = &[
    Keybind {
        label: KeyLabel::Single(key::QUIT.label),
        description: "Quit / clear input",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::HELP.label),
        description: "Show keybindings",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::NEXT_CHAT.label, key::PREV_CHAT.label),
        description: "Next / previous task session",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SEARCH.label),
        description: "Search messages",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::FILE_PICKER.label),
        description: "File picker",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::OPEN_EDITOR.label),
        description: "Open plan in editor",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::PLAN_TOGGLE.label),
        description: "Toggle plan panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::TASKS.label),
        description: "Open tasks",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::REVIEW.label),
        description: "Review a code change",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::TOGGLE_THINKING.label),
        description: "Show / hide thinking",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SUSPEND.label),
        description: "Suspend process",
        context: KeybindContext::General,
        platform: Platform::UnixOnly,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Submit prompt",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::MacMulti(
            &["Shift+Enter", "Ctrl+Enter", "Ctrl+J", "Alt+Enter"],
            &["⇧↵", "⌃↵", "⌃J", "⌥↵"],
        ),
        description: "Newline",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Toggle mode",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("/command"),
        description: "Open command palette",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::MacAlt(key::DELETE_WORD.label, "⌥⌫"),
        description: "Delete word backward",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::MacMulti(&["Alt+←", "Alt+→"], &["⌥←", "⌥→"]),
        description: "Move word left / right",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(mod_key!("Del"), "⌥Del"),
        description: "Delete word forward",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Single(key::KILL_LINE.label),
        description: "Delete to end of line",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        label: KeyLabel::Single(key::LINE_START.label),
        description: "Jump to start of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("Home", "End"),
        description: "Jump to start/end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        description: "Scroll half page up / down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(
            key::SCROLL_HALF_UP_ALT.label,
            key::SCROLL_HALF_DOWN_ALT.label,
        ),
        description: "Scroll half page up / down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::LINE_END.label),
        description: "Jump to end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SCROLL_TOP.label),
        description: "Scroll to top",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::SCROLL_BOTTOM.label),
        description: "Scroll to bottom",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::POP_QUEUE.label),
        description: "Pop queue",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc Esc"),
        description: "Rewind",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::EDIT_INPUT.label),
        description: "Edit input in external editor",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate input history",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc Esc"),
        description: "Cancel agent",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate options",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Select option",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Select",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Type"),
        description: "Filter",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("PageUp", "PageDown"),
        description: "Scroll page up / down",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        description: "Scroll page up / down",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Rewind session only",
        context: KeybindContext::RewindPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::REWIND_FILES.label),
        description: "Rewind session and restore files",
        context: KeybindContext::RewindPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Enter"),
        description: "Remove item",
        context: KeybindContext::QueueFocus,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("Tab"),
        description: "Complete command",
        context: KeybindContext::CommandPalette,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("!/@/#/$"),
        description: "Set tier (strong/medium/weak/compaction)",
        context: KeybindContext::ModelPicker,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("y", "n"),
        description: "Allow once / deny once",
        context: KeybindContext::Permission,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single("s"),
        description: "Allow for this session",
        context: KeybindContext::Permission,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("a", "A"),
        description: "Always allow this pattern here / everywhere",
        context: KeybindContext::Permission,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Alt("d", "D"),
        description: "Always deny this pattern here / everywhere",
        context: KeybindContext::Permission,
        platform: Platform::All,
    },
    Keybind {
        label: KeyLabel::Single(key::PERMISSION_DETAIL.label),
        description: "Show / hide request detail",
        context: KeybindContext::Permission,
        platform: Platform::All,
    },
];

pub fn all_contexts() -> impl Iterator<Item = KeybindContext> {
    use strum::IntoEnumIterator;
    KeybindContext::iter()
}

pub(crate) fn key_event_to_string(key: &KeyEvent) -> String {
    let mut s = String::new();
    let mods = key.modifiers;
    let is_char = matches!(key.code, KeyCode::Char(_));
    if mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push_str("alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) && !is_char {
        s.push_str("shift+");
    }
    match key.code {
        KeyCode::Char(' ') => s.push_str("space"),
        KeyCode::Char(c) => s.push(c),
        KeyCode::Enter => s.push_str("enter"),
        KeyCode::Esc => s.push_str("esc"),
        KeyCode::Tab => s.push_str("tab"),
        KeyCode::BackTab => {
            if !s.contains("shift+") {
                s.insert_str(0, "shift+");
            }
            s.push_str("tab");
        }
        KeyCode::Backspace => s.push_str("backspace"),
        KeyCode::Delete => s.push_str("delete"),
        KeyCode::Up => s.push_str("up"),
        KeyCode::Down => s.push_str("down"),
        KeyCode::Left => s.push_str("left"),
        KeyCode::Right => s.push_str("right"),
        KeyCode::Home => s.push_str("home"),
        KeyCode::End => s.push_str("end"),
        KeyCode::PageUp => s.push_str("pageup"),
        KeyCode::PageDown => s.push_str("pagedown"),
        KeyCode::F(n) => write!(s, "f{n}").unwrap(),
        KeyCode::Insert => s.push_str("insert"),
        _ => {}
    }
    s
}

pub fn display_label(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut s = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        s.push_str("Ctrl+");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        s.push_str("Alt+");
    }
    if modifiers.contains(KeyModifiers::SHIFT) && !matches!(code, KeyCode::Char(_)) {
        s.push_str("Shift+");
    }
    match code {
        KeyCode::Char(' ') => s.push_str("Space"),
        KeyCode::Char(c) => s.extend(c.to_uppercase()),
        KeyCode::Enter => s.push_str("Enter"),
        KeyCode::Esc => s.push_str("Esc"),
        KeyCode::Tab => s.push_str("Tab"),
        KeyCode::Backspace => s.push_str("Backspace"),
        KeyCode::Delete => s.push_str("Del"),
        KeyCode::Up => s.push('↑'),
        KeyCode::Down => s.push('↓'),
        KeyCode::Left => s.push('←'),
        KeyCode::Right => s.push('→'),
        KeyCode::Home => s.push_str("Home"),
        KeyCode::End => s.push_str("End"),
        KeyCode::PageUp => s.push_str("PageUp"),
        KeyCode::PageDown => s.push_str("PageDown"),
        KeyCode::F(n) => write!(s, "F{n}").unwrap(),
        KeyCode::Insert => s.push_str("Insert"),
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use test_case::test_case;

    #[test_case(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), "ctrl+d")]
    #[test_case(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), "alt+x")]
    #[test_case(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT), "shift+tab")]
    #[test_case(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), "shift+tab")]
    #[test_case(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), "space")]
    #[test_case(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), "f5")]
    #[test_case(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), "a")]
    fn key_event_to_string_cases(input: KeyEvent, expected: &str) {
        assert_eq!(key_event_to_string(&input), expected);
    }

    #[test]
    fn bind_requires_exact_modifiers() {
        let bind = key::OPEN_EDITOR; // Ctrl+O
        let exact = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let extra = KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let wrong = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT);

        assert!(bind.matches(exact));
        assert!(!bind.matches(extra), "extra modifiers should not match");
        assert!(!bind.matches(wrong), "wrong modifier should not match");
    }

    #[test]
    fn every_context_has_at_least_one_keybind() {
        for ctx in all_contexts() {
            let has_own = KEYBINDS.iter().any(|kb| kb.context == ctx);
            let has_parent = ctx
                .parent()
                .is_some_and(|p| KEYBINDS.iter().any(|kb| kb.context == p));
            assert!(
                has_own || has_parent,
                "context {:?} has no keybinds and no parent with keybinds",
                ctx,
            );
        }
    }

    #[test]
    fn no_duplicate_entries() {
        for (i, a) in KEYBINDS.iter().enumerate() {
            for (j, b) in KEYBINDS.iter().enumerate() {
                if i != j && a.context == b.context {
                    assert!(
                        a.label.flat_str() != b.label.flat_str() || a.description != b.description,
                        "duplicate keybind: {} - {} in {:?}",
                        a.label.flat_str(),
                        a.description,
                        a.context,
                    );
                }
            }
        }
    }
}
