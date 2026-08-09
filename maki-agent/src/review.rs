//! Code review as an isolated task.
//!
//! The reviewer runs in its own conversation with its own rubric, and only the
//! result lands in the parent history. The parent model never decides to start
//! a review and never sees the investigation.
//!
//! Ported from OpenAI Codex (`codex-rs/core/src/tasks/review.rs`,
//! `codex-rs/prompts/src/review_request.rs`,
//! `codex-rs/protocol/src/review_format.rs`), Apache-2.0.

use std::path::PathBuf;

use maki_providers::{ContentBlock, Message, Role};
use serde::{Deserialize, Serialize};

use crate::agent::History;

/// System prompt for the reviewer conversation.
pub const REVIEW_PROMPT: &str = include_str!("prompts/review.md");

const UNCOMMITTED_PROMPT: &str = "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings.";
const INTERRUPTED_MESSAGE: &str =
    "Review was interrupted. Re-run /review and wait for it to complete.";
const FALLBACK_MESSAGE: &str = "Reviewer failed to output a response.";
const SHORT_SHA_LEN: usize = 7;

/// Recorded in the parent history so a later turn can answer questions about
/// the review without the parent model having watched it happen.
const EXIT_SUCCESS: &str = "<user_action>
  <context>User initiated a review task. Here's the full review output from the reviewer model.</context>
  <action>review</action>
  <results>
  {results}
  </results>
</user_action>";
const EXIT_INTERRUPTED: &str = "<user_action>
  <context>User initiated a review task, but was interrupted. If user asks about this, tell them to re-initiate a review with `/review` and wait for it to complete.</context>
  <action>review</action>
  <results>
  None.
  </results>
</user_action>";
const RESULTS_PLACEHOLDER: &str = "{results}";

/// What the reviewer is asked to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    /// Staged, unstaged, and untracked files.
    UncommittedChanges,
    /// Everything this branch adds on top of `branch`.
    BaseBranch { branch: String },
    /// A single commit, with its subject when the picker knows it.
    Commit { sha: String, title: Option<String> },
    /// Whatever the user typed after `/review`.
    Custom { instructions: String },
}

impl ReviewTarget {
    /// The first user message of the reviewer conversation.
    pub fn prompt(&self) -> String {
        match self {
            Self::UncommittedChanges => UNCOMMITTED_PROMPT.to_string(),
            Self::BaseBranch { branch } => format!(
                "Review the code changes against the base branch '{branch}'. \
                 Start by finding the merge base between the current branch and '{branch}' \
                 (`git merge-base HEAD {branch}`), then run `git diff` against that SHA to see \
                 what would merge into {branch}. Provide prioritized, actionable findings."
            ),
            Self::Commit { sha, title } => match title {
                Some(title) => format!(
                    "Review the code changes introduced by commit {sha} (\"{title}\"). \
                     Provide prioritized, actionable findings."
                ),
                None => format!(
                    "Review the code changes introduced by commit {sha}. \
                     Provide prioritized, actionable findings."
                ),
            },
            Self::Custom { instructions } => instructions.trim().to_string(),
        }
    }

    /// Short label for the status line and the queue panel.
    pub fn hint(&self) -> String {
        match self {
            Self::UncommittedChanges => "current changes".to_string(),
            Self::BaseBranch { branch } => format!("changes against '{branch}'"),
            Self::Commit { sha, title } => {
                let short: String = sha.chars().take(SHORT_SHA_LEN).collect();
                match title {
                    Some(title) => format!("commit {short}: {title}"),
                    None => format!("commit {short}"),
                }
            }
            Self::Custom { instructions } => instructions.trim().to_string(),
        }
    }
}

/// Structured result the reviewer is asked to emit as its final message.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ReviewOutput {
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub overall_correctness: String,
    #[serde(default)]
    pub overall_explanation: String,
    #[serde(default)]
    pub overall_confidence_score: f32,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ReviewFinding {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub confidence_score: f32,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub code_location: ReviewCodeLocation,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ReviewCodeLocation {
    #[serde(default)]
    pub absolute_file_path: PathBuf,
    #[serde(default)]
    pub line_range: ReviewLineRange,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewLineRange {
    #[serde(default)]
    pub start: u32,
    #[serde(default)]
    pub end: u32,
}

/// Reviewers do not always honour the schema. Try the whole message, then the
/// widest `{...}` span inside it, then keep the prose as the explanation so the
/// work is never thrown away.
pub fn parse_review_output(text: &str) -> ReviewOutput {
    if let Ok(output) = serde_json::from_str::<ReviewOutput>(text) {
        return output;
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
        && let Ok(output) = serde_json::from_str::<ReviewOutput>(slice)
    {
        return output;
    }
    ReviewOutput {
        overall_explanation: text.to_string(),
        ..Default::default()
    }
}

fn format_location(finding: &ReviewFinding) -> String {
    let path = finding.code_location.absolute_file_path.display();
    let start = finding.code_location.line_range.start;
    let end = finding.code_location.line_range.end;
    format!("{path}:{start}-{end}")
}

fn format_findings_block(findings: &[ReviewFinding]) -> String {
    let mut lines = vec![
        String::new(),
        if findings.len() > 1 {
            "Full review comments:".to_string()
        } else {
            "Review comment:".to_string()
        },
    ];

    for finding in findings {
        lines.push(String::new());
        lines.push(format!(
            "- {} - {}",
            finding.title,
            format_location(finding)
        ));
        lines.extend(finding.body.lines().map(|line| format!("  {line}")));
    }

    lines.join("\n")
}

/// Human-readable review summary shown in the transcript.
pub fn render_review_output(output: &ReviewOutput) -> String {
    let mut sections = Vec::new();
    let explanation = output.overall_explanation.trim();
    if !explanation.is_empty() {
        sections.push(explanation.to_string());
    }
    if !output.findings.is_empty() {
        let block = format_findings_block(&output.findings);
        let trimmed = block.trim();
        if !trimmed.is_empty() {
            sections.push(trimmed.to_string());
        }
    }
    if sections.is_empty() {
        FALLBACK_MESSAGE.to_string()
    } else {
        sections.join("\n\n")
    }
}

/// Records the outcome in the parent conversation and returns the text to
/// display. `None` means the review was cancelled or never produced output.
///
/// The user-role half is synthetic: it carries the raw findings to the model on
/// the next turn without drawing a bubble the user never typed.
pub fn record_review(history: &mut History, output: Option<&ReviewOutput>) -> String {
    let (context, display) = match output {
        Some(output) => {
            let mut results = output.overall_explanation.trim().to_string();
            if !output.findings.is_empty() {
                results.push('\n');
                results.push_str(&format_findings_block(&output.findings));
            }
            (
                EXIT_SUCCESS.replace(RESULTS_PLACEHOLDER, &results),
                render_review_output(output),
            )
        }
        None => (
            EXIT_INTERRUPTED.to_string(),
            INTERRUPTED_MESSAGE.to_string(),
        ),
    };

    history.push(Message::synthetic(context));
    history.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: display.clone(),
        }],
        ..Default::default()
    });
    display
}

/// The reviewer's verdict is its last assistant message.
pub fn final_assistant_text(history: &History) -> &str {
    history
        .as_slice()
        .iter()
        .rev()
        .find(|msg| matches!(msg.role, Role::Assistant))
        .and_then(|msg| {
            msg.content.iter().rev().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const SHA: &str = "abc1234def";

    fn finding(title: &str, path: &str, start: u32, end: u32) -> ReviewFinding {
        ReviewFinding {
            title: title.into(),
            body: "why it breaks".into(),
            code_location: ReviewCodeLocation {
                absolute_file_path: path.into(),
                line_range: ReviewLineRange { start, end },
            },
            ..Default::default()
        }
    }

    #[test_case(ReviewTarget::UncommittedChanges, "current changes" ; "uncommitted")]
    #[test_case(ReviewTarget::BaseBranch { branch: "main".into() }, "changes against 'main'" ; "base_branch")]
    #[test_case(ReviewTarget::Commit { sha: SHA.into(), title: None }, "commit abc1234" ; "commit_without_title")]
    #[test_case(ReviewTarget::Commit { sha: SHA.into(), title: Some("fix parser".into()) }, "commit abc1234: fix parser" ; "commit_with_title")]
    #[test_case(ReviewTarget::Custom { instructions: "  look at auth  ".into() }, "look at auth" ; "custom_is_trimmed")]
    fn hint_describes_target(target: ReviewTarget, expected: &str) {
        assert_eq!(target.hint(), expected);
    }

    #[test_case(ReviewTarget::UncommittedChanges, "untracked" ; "uncommitted_mentions_untracked")]
    #[test_case(ReviewTarget::BaseBranch { branch: "main".into() }, "git merge-base HEAD main" ; "base_branch_gives_merge_base_command")]
    #[test_case(ReviewTarget::Commit { sha: SHA.into(), title: None }, SHA ; "commit_names_full_sha")]
    #[test_case(ReviewTarget::Custom { instructions: "look at auth".into() }, "look at auth" ; "custom_is_verbatim")]
    fn prompt_carries_target(target: ReviewTarget, expected: &str) {
        assert!(target.prompt().contains(expected), "{}", target.prompt());
    }

    #[test]
    fn parses_bare_json() {
        let output = parse_review_output(
            r#"{"findings":[],"overall_correctness":"patch is correct","overall_explanation":"looks fine","overall_confidence_score":0.9}"#,
        );
        assert_eq!(output.overall_correctness, "patch is correct");
        assert_eq!(output.overall_explanation, "looks fine");
    }

    #[test]
    fn parses_json_wrapped_in_prose() {
        let output = parse_review_output(
            "Here you go:\n```json\n{\"overall_explanation\":\"all good\"}\n```\nThanks!",
        );
        assert_eq!(output.overall_explanation, "all good");
        assert!(output.findings.is_empty());
    }

    #[test]
    fn keeps_unparseable_text_as_explanation() {
        let output = parse_review_output("no json here");
        assert_eq!(output.overall_explanation, "no json here");
    }

    #[test]
    fn renders_fallback_when_output_is_empty() {
        assert_eq!(
            render_review_output(&ReviewOutput::default()),
            FALLBACK_MESSAGE
        );
    }

    #[test]
    fn renders_explanation_and_findings() {
        let output = ReviewOutput {
            findings: vec![
                finding("[P1] Fix the off-by-one", "src/a.rs", 10, 12),
                finding("[P2] Drop the dead branch", "src/b.rs", 3, 3),
            ],
            overall_explanation: "two issues".into(),
            ..Default::default()
        };
        let text = render_review_output(&output);
        assert!(text.starts_with("two issues"));
        assert!(text.contains("Full review comments:"));
        assert!(text.contains("- [P1] Fix the off-by-one - src/a.rs:10-12"));
        assert!(text.contains("  why it breaks"));
    }

    #[test]
    fn single_finding_uses_singular_header() {
        let output = ReviewOutput {
            findings: vec![finding("[P0] Boom", "src/a.rs", 1, 2)],
            ..Default::default()
        };
        assert!(render_review_output(&output).contains("Review comment:"));
    }

    #[test]
    fn records_success_as_synthetic_context_plus_assistant_text() {
        let mut history = History::new(vec![]);
        let output = ReviewOutput {
            findings: vec![finding("[P1] Fix it", "src/a.rs", 1, 2)],
            overall_explanation: "one issue".into(),
            ..Default::default()
        };

        let display = record_review(&mut history, Some(&output));

        let msgs = history.as_slice();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(msgs[0].user_text().is_none(), "context must stay hidden");
        assert!(matches!(msgs[1].role, Role::Assistant));
        assert!(display.contains("[P1] Fix it"));
    }

    #[test]
    fn records_interruption_without_findings() {
        let mut history = History::new(vec![]);
        let display = record_review(&mut history, None);
        assert_eq!(display, INTERRUPTED_MESSAGE);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn final_assistant_text_reads_last_assistant_block() {
        let history = History::new(vec![
            Message::user("go".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "first".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "verdict".into(),
                }],
                ..Default::default()
            },
        ]);
        assert_eq!(final_assistant_text(&history), "verdict");
    }

    #[test]
    fn final_assistant_text_is_empty_without_assistant_turn() {
        let history = History::new(vec![Message::user("go".into())]);
        assert_eq!(final_assistant_text(&history), "");
    }
}
