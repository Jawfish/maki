//! Per-turn workspace snapshots backed by a shadow git repository.
//!
//! The shadow repo lives in the maki state dir and only ever runs with an
//! explicit `--git-dir`, `--work-tree` and `GIT_INDEX_FILE`, so the project's
//! own `.git` (index, HEAD, reflog, `git status` output) is never touched.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::StateDir;
use crate::paths::normalize_path;

const CHECKPOINTS_DIR: &str = "checkpoints";
const SHADOW_REF: &str = "refs/heads/maki-checkpoints";
const INDEX_FILE: &str = "shadow-index";
const NO_CONFIG_FILE: &str = "no-config";
const AUTHOR_NAME: &str = "maki";
const AUTHOR_EMAIL: &str = "maki@localhost";
const LOG_FORMAT: &str = "--format=%H%x00%ct%x00%s";
const FIELD_SEPARATOR: char = '\0';
const LOG_FIELDS: usize = 3;
const INHERITED_GIT_VARS: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Storage(#[from] crate::StorageError),
    #[error("git {command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("checkpoint not found: {0}")]
    NotFound(String),
    #[error("unexpected git output for {command}: {output}")]
    Output { command: String, output: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckpointId(String);

impl CheckpointId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub label: String,
    /// Unix seconds.
    pub created_at: i64,
}

/// Handle to one project's shadow repository.
#[derive(Debug, Clone)]
pub struct Checkpoints {
    git_dir: PathBuf,
    worktree: PathBuf,
}

impl Checkpoints {
    /// Open (creating on first use) the shadow repo for `project_root`.
    pub fn open(state: &StateDir, project_root: &Path) -> Result<Self, CheckpointError> {
        let worktree = normalize_path(project_root);
        let git_dir = state
            .ensure_subdir(CHECKPOINTS_DIR)?
            .join(shadow_dir_name(&worktree));
        let shadow = Self { git_dir, worktree };
        shadow.init()?;
        Ok(shadow)
    }

    /// Snapshot the workspace. Returns `None` when nothing changed since the
    /// previous checkpoint, so callers can create one per turn unconditionally.
    pub fn create(&self, label: &str) -> Result<Option<Checkpoint>, CheckpointError> {
        self.stage_worktree()?;
        let tree = self.run_stdout(&["write-tree"])?;
        let parent = self.head_commit()?;
        if let Some(parent) = &parent
            && self.run_stdout(&["rev-parse", &format!("{parent}^{{tree}}")])? == tree
        {
            return Ok(None);
        }
        let mut args = vec!["commit-tree", tree.as_str(), "-m", label];
        if let Some(parent) = &parent {
            args.extend_from_slice(&["-p", parent.as_str()]);
        }
        let commit = self.run_stdout(&args)?;
        self.run_stdout(&["update-ref", SHADOW_REF, &commit])?;
        let created_at = self.run_stdout(&["show", "-s", "--format=%ct", &commit])?;
        Ok(Some(Checkpoint {
            id: CheckpointId(commit),
            label: label.to_string(),
            created_at: parse_timestamp(&created_at)?,
        }))
    }

    /// Checkpoints for this project, newest first.
    pub fn list(&self) -> Result<Vec<Checkpoint>, CheckpointError> {
        if self.head_commit()?.is_none() {
            return Ok(Vec::new());
        }
        let log = self.run_stdout(&["log", LOG_FORMAT, SHADOW_REF])?;
        log.lines()
            .filter(|l| !l.is_empty())
            .map(parse_log)
            .collect()
    }

    /// Return the workspace to `id`: restores modified and deleted files and
    /// removes files created after the checkpoint.
    pub fn restore(&self, id: &CheckpointId) -> Result<(), CheckpointError> {
        let commit = format!("{id}^{{commit}}");
        if self
            .run_stdout(&["rev-parse", "--verify", "--quiet", &commit])
            .is_err()
        {
            return Err(CheckpointError::NotFound(id.to_string()));
        }
        self.stage_worktree()?;
        self.run_stdout(&["read-tree", "-u", "--reset", id.as_str()])?;
        Ok(())
    }

    fn init(&self) -> Result<(), CheckpointError> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.git_dir)?;
        let git_dir = self.git_dir.display().to_string();
        run(Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg("--bare")
            .arg(&git_dir)
            .current_dir(&self.worktree))?;
        self.run_stdout(&["config", "core.bare", "false"])?;
        Ok(())
    }

    /// Record the whole worktree in the shadow index. Ignore rules come from
    /// the worktree's `.gitignore` files; git never scans `.git` itself.
    fn stage_worktree(&self) -> Result<(), CheckpointError> {
        self.run_stdout(&["add", "-A", "--", "."])?;
        Ok(())
    }

    fn head_commit(&self) -> Result<Option<String>, CheckpointError> {
        match self.run_stdout(&["rev-parse", "--verify", "--quiet", SHADOW_REF]) {
            Ok(commit) => Ok(Some(commit)),
            Err(CheckpointError::Git { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn run_stdout(&self, args: &[&str]) -> Result<String, CheckpointError> {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.worktree)
            .args(args)
            .current_dir(&self.worktree)
            .env("GIT_INDEX_FILE", self.git_dir.join(INDEX_FILE))
            .env("GIT_CONFIG_GLOBAL", self.git_dir.join(NO_CONFIG_FILE))
            .env("GIT_CONFIG_SYSTEM", self.git_dir.join(NO_CONFIG_FILE))
            .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
            .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
            .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
            .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
            .env("GIT_TERMINAL_PROMPT", "0");
        for var in INHERITED_GIT_VARS {
            command.env_remove(var);
        }
        run(&mut command)
    }
}

fn run(command: &mut Command) -> Result<String, CheckpointError> {
    let output = command.output()?;
    finish(command, output)
}

fn finish(command: &Command, output: Output) -> Result<String, CheckpointError> {
    if !output.status.success() {
        return Err(CheckpointError::Git {
            command: describe(command),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

fn describe(command: &Command) -> String {
    command
        .get_args()
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_log(line: &str) -> Result<Checkpoint, CheckpointError> {
    let mut fields = line.splitn(LOG_FIELDS, FIELD_SEPARATOR);
    let (Some(id), Some(timestamp), Some(label)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(CheckpointError::Output {
            command: LOG_FORMAT.to_string(),
            output: line.to_string(),
        });
    };
    Ok(Checkpoint {
        id: CheckpointId(id.to_string()),
        label: label.to_string(),
        created_at: parse_timestamp(timestamp)?,
    })
}

fn parse_timestamp(value: &str) -> Result<i64, CheckpointError> {
    value.trim().parse().map_err(|_| CheckpointError::Output {
        command: LOG_FORMAT.to_string(),
        output: value.to_string(),
    })
}

/// Stable per-project directory name: readable suffix plus a hash of the
/// absolute path so sibling projects with the same name never collide.
fn shadow_dir_name(worktree: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    worktree.hash(&mut hasher);
    let name = worktree
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{slug}-{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use test_case::test_case;

    use super::*;

    const TRACKED_FILE: &str = "tracked.txt";
    const UNTRACKED_FILE: &str = "untracked.txt";
    const IGNORED_FILE: &str = "build.log";
    const GITIGNORE: &str = ".gitignore";
    const ORIGINAL: &str = "original\n";
    const CHANGED: &str = "changed\n";
    const LABEL: &str = "turn 1";

    struct Project {
        _state: tempfile::TempDir,
        root: tempfile::TempDir,
    }

    impl Project {
        fn new(git_repo: bool) -> (Self, Checkpoints) {
            let state = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let project = Self {
                _state: state,
                root,
            };
            project.write(GITIGNORE, &format!("{IGNORED_FILE}\n"));
            project.write(TRACKED_FILE, ORIGINAL);
            project.write(IGNORED_FILE, ORIGINAL);
            if git_repo {
                project.user_git(&["init", "--quiet"]);
                project.user_git(&["add", "-A"]);
                project.user_git(&[
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@localhost",
                    "commit",
                    "--quiet",
                    "-m",
                    "init",
                ]);
            }
            let checkpoints = Checkpoints::open(
                &StateDir::from_path(project._state.path().to_path_buf()),
                project.root.path(),
            )
            .unwrap();
            (project, checkpoints)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        fn write(&self, name: &str, content: &str) {
            fs::write(self.path(name), content).unwrap();
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.path(name)).unwrap()
        }

        fn user_git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .args(args)
                .current_dir(self.root.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap()
        }
    }

    #[test_case(true; "inside a git repo")]
    #[test_case(false; "without a git repo")]
    fn create_captures_modified_and_untracked_files(git_repo: bool) {
        let (project, checkpoints) = Project::new(git_repo);
        project.write(UNTRACKED_FILE, ORIGINAL);
        let checkpoint = checkpoints.create(LABEL).unwrap().unwrap();

        let files = checkpoints
            .run_stdout(&["ls-tree", "-r", "--name-only", checkpoint.id.as_str()])
            .unwrap();
        assert!(files.lines().any(|f| f == TRACKED_FILE));
        assert!(files.lines().any(|f| f == UNTRACKED_FILE));
        assert_eq!(checkpoint.label, LABEL);
    }

    #[test]
    fn ignored_files_are_excluded() {
        let (_project, checkpoints) = Project::new(true);
        let checkpoint = checkpoints.create(LABEL).unwrap().unwrap();

        let files = checkpoints
            .run_stdout(&["ls-tree", "-r", "--name-only", checkpoint.id.as_str()])
            .unwrap();
        assert!(!files.lines().any(|f| f == IGNORED_FILE), "{files}");
    }

    #[test]
    fn create_without_changes_returns_none() {
        let (_project, checkpoints) = Project::new(true);
        assert!(checkpoints.create(LABEL).unwrap().is_some());
        assert!(checkpoints.create(LABEL).unwrap().is_none());
        assert_eq!(checkpoints.list().unwrap().len(), 1);
    }

    #[test]
    fn restore_round_trips_content_and_deletions() {
        let (project, checkpoints) = Project::new(true);
        project.write(UNTRACKED_FILE, ORIGINAL);
        let checkpoint = checkpoints.create(LABEL).unwrap().unwrap();

        project.write(TRACKED_FILE, CHANGED);
        fs::remove_file(project.path(UNTRACKED_FILE)).unwrap();
        project.write("added.txt", CHANGED);

        checkpoints.restore(&checkpoint.id).unwrap();

        assert_eq!(project.read(TRACKED_FILE), ORIGINAL);
        assert_eq!(project.read(UNTRACKED_FILE), ORIGINAL);
        assert!(!project.path("added.txt").exists());
    }

    #[test]
    fn restore_rejects_unknown_checkpoint() {
        let (_project, checkpoints) = Project::new(true);
        let missing = CheckpointId("0".repeat(40));
        assert!(matches!(
            checkpoints.restore(&missing),
            Err(CheckpointError::NotFound(_))
        ));
    }

    #[test]
    fn user_repo_status_and_head_are_untouched() {
        let (project, checkpoints) = Project::new(true);
        project.write(UNTRACKED_FILE, ORIGINAL);
        let status_before = project.user_git(&["status", "--porcelain=v2", "--branch"]);
        let head_before = project.user_git(&["rev-parse", "HEAD"]);
        let reflog_before = project.user_git(&["reflog", "--format=%H %gs"]);

        let checkpoint = checkpoints.create(LABEL).unwrap().unwrap();
        checkpoints.restore(&checkpoint.id).unwrap();

        assert_eq!(
            project.user_git(&["status", "--porcelain=v2", "--branch"]),
            status_before
        );
        assert_eq!(project.user_git(&["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            project.user_git(&["reflog", "--format=%H %gs"]),
            reflog_before
        );
    }
}
