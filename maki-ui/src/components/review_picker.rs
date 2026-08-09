//! Review preset selection: pick what the reviewer should look at.
//!
//! Ported from OpenAI Codex (`codex-rs/tui/src/chatwidget/review_popups.rs`),
//! Apache-2.0. Custom instructions arrive as `/review <instructions>` instead
//! of a separate prompt view.

use std::process::Command;

use crossterm::event::KeyEvent;
use maki_agent::review::ReviewTarget;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

const TITLE: &str = " Review ";
const BRANCH_TITLE: &str = " Base branch ";
const COMMIT_TITLE: &str = " Commit ";
const NO_BRANCHES: &str = "No other branches found";
const NO_COMMITS: &str = "No commits found";
const BRANCH_LIMIT: &str = "--count=30";
const COMMIT_LIMIT: &str = "--max-count=50";
const COMMIT_FIELD_SEP: char = '\t';

pub enum ReviewPickerAction {
    Consumed,
    Select(ReviewTarget),
    Close,
}

/// What selecting a row does: run the review, or drill into a second list.
enum Step {
    Review(ReviewTarget),
    Branches,
    Commits,
}

pub struct ReviewEntry {
    label: String,
    detail: Option<String>,
    step: Step,
}

impl PickerItem for ReviewEntry {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl ReviewEntry {
    fn new(label: impl Into<String>, detail: Option<&str>, step: Step) -> Self {
        Self {
            label: label.into(),
            detail: detail.map(str::to_owned),
            step,
        }
    }
}

pub struct ReviewPicker {
    picker: ListPicker<ReviewEntry>,
}

impl ReviewPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
        }
    }

    pub fn open(&mut self) {
        self.picker.set_error_text(None);
        self.picker.open(
            vec![
                ReviewEntry::new(
                    "Review uncommitted changes",
                    Some("staged, unstaged, and untracked"),
                    Step::Review(ReviewTarget::UncommittedChanges),
                ),
                ReviewEntry::new(
                    "Review against a base branch",
                    Some("diff from the merge base"),
                    Step::Branches,
                ),
                ReviewEntry::new(
                    "Review a commit",
                    Some("pick a recent commit"),
                    Step::Commits,
                ),
            ],
            TITLE,
        );
    }

    fn open_branches(&mut self) {
        let entries: Vec<ReviewEntry> = git_lines(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "--sort=-committerdate",
            BRANCH_LIMIT,
            "refs/heads",
        ])
        .into_iter()
        .filter(|branch| Some(branch.as_str()) != current_branch().as_deref())
        .map(|branch| {
            ReviewEntry::new(
                branch.clone(),
                None,
                Step::Review(ReviewTarget::BaseBranch { branch }),
            )
        })
        .collect();

        self.reopen(entries, BRANCH_TITLE, NO_BRANCHES);
    }

    fn open_commits(&mut self) {
        let entries: Vec<ReviewEntry> = git_lines(&[
            "log",
            COMMIT_LIMIT,
            "--pretty=format:%h%x09%s",
            "--no-color",
        ])
        .into_iter()
        .filter_map(|line| {
            let (sha, subject) = line.split_once(COMMIT_FIELD_SEP)?;
            Some(ReviewEntry::new(
                subject,
                Some(sha),
                Step::Review(ReviewTarget::Commit {
                    sha: sha.to_owned(),
                    title: Some(subject.to_owned()),
                }),
            ))
        })
        .collect();

        self.reopen(entries, COMMIT_TITLE, NO_COMMITS);
    }

    /// An empty second list stays up with the reason on it, so the user is not
    /// dropped back to a blank prompt wondering what happened.
    fn reopen(&mut self, entries: Vec<ReviewEntry>, title: &str, empty_message: &str) {
        self.picker
            .set_error_text(entries.is_empty().then(|| empty_message.to_owned()));
        self.picker.open(entries, title);
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

    pub fn handle_key(&mut self, key: KeyEvent) -> ReviewPickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Select(entry) => match entry.step {
                Step::Review(target) => return ReviewPickerAction::Select(target),
                Step::Branches => self.open_branches(),
                Step::Commits => self.open_commits(),
            },
            PickerAction::Close => return ReviewPickerAction::Close,
            PickerAction::Consumed | PickerAction::Toggle(..) => {}
        }
        ReviewPickerAction::Consumed
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for ReviewPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn git_lines(args: &[&str]) -> Vec<String> {
    let Ok(output) = Command::new("git").args(args).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn current_branch() -> Option<String> {
    git_lines(&["rev-parse", "--abbrev-ref", "HEAD"])
        .into_iter()
        .next()
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};
    use test_case::test_case;

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn selected_target(picker: &mut ReviewPicker) -> Option<ReviewTarget> {
        match picker.handle_key(key(KeyCode::Enter)) {
            ReviewPickerAction::Select(target) => Some(target),
            _ => None,
        }
    }

    #[test]
    fn first_preset_reviews_uncommitted_changes() {
        let mut picker = ReviewPicker::new();
        picker.open();
        assert_eq!(
            selected_target(&mut picker),
            Some(ReviewTarget::UncommittedChanges)
        );
    }

    #[test_case(1 ; "base_branch")]
    #[test_case(2 ; "commit")]
    fn drilling_into_a_second_list_does_not_start_a_review(index: usize) {
        let mut picker = ReviewPicker::new();
        picker.open();
        picker.picker.select(index);
        assert!(selected_target(&mut picker).is_none());
        assert!(picker.is_open());
    }

    #[test]
    fn esc_closes() {
        let mut picker = ReviewPicker::new();
        picker.open();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            ReviewPickerAction::Close
        ));
    }

    #[test]
    fn reopen_with_no_entries_keeps_the_picker_and_explains_why() {
        let mut picker = ReviewPicker::new();
        picker.open();
        picker.reopen(Vec::new(), BRANCH_TITLE, NO_BRANCHES);
        assert!(picker.is_open());
        assert!(selected_target(&mut picker).is_none());
    }
}
