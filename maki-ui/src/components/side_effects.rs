//! Classifies a tool invocation as reversible by a workspace checkpoint or
//! not. A shadow-git snapshot can undo file edits; it cannot unsend a request,
//! uninstall a package or unpush a commit, so the rewind picker warns about
//! turns that did any of those.

use maki_agent::tools::{BASH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME};

const WEBFETCH_TOOL_NAME: &str = "webfetch";
const WEBSEARCH_TOOL_NAME: &str = "websearch";

/// Tools that reach outside the workspace no matter what they are asked.
const NETWORK_TOOLS: [&str; 2] = [WEBFETCH_TOOL_NAME, WEBSEARCH_TOOL_NAME];

/// Tools whose effect depends on the command they were given.
const COMMAND_TOOLS: [&str; 2] = [BASH_TOOL_NAME, CODE_EXECUTION_TOOL_NAME];

/// Command prefixes, as whitespace-separated tokens, whose effects outlive the
/// workspace: publishing, installing and plain network calls.
const IRREVERSIBLE_COMMANDS: [&[&str]; 22] = [
    &["git", "push"],
    &["gh", "pr"],
    &["gh", "release"],
    &["npm", "install"],
    &["npm", "publish"],
    &["pnpm", "add"],
    &["yarn", "add"],
    &["pip", "install"],
    &["uv", "add"],
    &["cargo", "install"],
    &["cargo", "publish"],
    &["apt", "install"],
    &["apt-get", "install"],
    &["brew", "install"],
    &["docker", "push"],
    &["kubectl", "apply"],
    &["terraform", "apply"],
    &["curl"],
    &["wget"],
    &["ssh"],
    &["scp"],
    &["rsync"],
];

pub(crate) fn is_irreversible(tool: &str, summary: &str) -> bool {
    if NETWORK_TOOLS.contains(&tool) {
        return true;
    }
    if !COMMAND_TOOLS.contains(&tool) {
        return false;
    }
    let tokens: Vec<String> = summary
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    IRREVERSIBLE_COMMANDS
        .iter()
        .any(|pattern| starts_any_command(&tokens, pattern))
}

/// True when `pattern` appears as consecutive whole tokens, so `curl` matches
/// a call but not a path like `src/curly.rs`.
fn starts_any_command(tokens: &[String], pattern: &[&str]) -> bool {
    tokens
        .windows(pattern.len())
        .any(|window| window.iter().zip(pattern).all(|(token, want)| token == want))
}

#[cfg(test)]
mod tests {
    use super::is_irreversible;
    use maki_agent::tools::{BASH_TOOL_NAME, EDIT_TOOL_NAME, WRITE_TOOL_NAME};
    use test_case::test_case;

    #[test_case(BASH_TOOL_NAME, "git push origin main", true ; "git_push")]
    #[test_case(BASH_TOOL_NAME, "npm install left-pad", true ; "npm_install")]
    #[test_case(BASH_TOOL_NAME, "curl https://example.com", true ; "curl")]
    #[test_case(BASH_TOOL_NAME, "cd repo && git push", true ; "push_after_chain")]
    #[test_case(BASH_TOOL_NAME, "GIT PUSH", true ; "case_insensitive")]
    #[test_case(BASH_TOOL_NAME, "cargo build", false ; "build")]
    #[test_case(BASH_TOOL_NAME, "git status", false ; "status")]
    #[test_case(BASH_TOOL_NAME, "rg curly src/curly.rs", false ; "token_not_substring")]
    #[test_case(BASH_TOOL_NAME, "", false ; "empty_command")]
    #[test_case(WRITE_TOOL_NAME, "src/main.rs", false ; "write_is_workspace_only")]
    #[test_case(EDIT_TOOL_NAME, "git push", false ; "summary_ignored_for_file_tools")]
    #[test_case("webfetch", "https://example.com", true ; "webfetch")]
    #[test_case("websearch", "rust lifetimes", true ; "websearch")]
    fn classifier_marks_non_workspace_effects(tool: &str, summary: &str, expected: bool) {
        assert_eq!(is_irreversible(tool, summary), expected);
    }
}
