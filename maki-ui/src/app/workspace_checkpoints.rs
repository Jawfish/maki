//! Pre-turn workspace snapshots, so a rewind can put the files back too.
//!
//! Every run takes one snapshot before the agent starts, keyed by the message
//! index the turn will occupy. The git work runs on its own thread: the UI
//! only holds a receiver and reads the id when it needs one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use maki_storage::StateDir;
use maki_storage::checkpoints::{CheckpointError, CheckpointId, Checkpoints};

pub(crate) const NO_SNAPSHOT_ERR: &str = "No file snapshot for that turn, rewound the session only";
const RESTORE_FAILED_ERR: &str = "Failed to restore files";
const SNAPSHOT_LABEL_PREFIX: &str = "turn";

struct Shadow {
    state: StateDir,
    project_root: PathBuf,
    opened: Option<Checkpoints>,
}

impl Shadow {
    /// Opening initialises the shadow repo, so it happens on the worker
    /// thread and is remembered for every later turn.
    fn handle(&mut self) -> Result<&Checkpoints, CheckpointError> {
        if self.opened.is_none() {
            self.opened = Some(Checkpoints::open(&self.state, &self.project_root)?);
        }
        Ok(self
            .opened
            .as_ref()
            .expect("shadow repo was just opened above"))
    }
}

struct TurnRecord {
    /// Index of the user message this turn starts at.
    turn_index: usize,
    id: Option<CheckpointId>,
    pending: Option<flume::Receiver<Option<CheckpointId>>>,
    irreversible: bool,
}

impl TurnRecord {
    /// Waits for the snapshot thread. The snapshot was taken when the turn
    /// started, so by rewind time it has long since landed.
    fn resolve(&mut self) -> Option<&CheckpointId> {
        if let Some(rx) = self.pending.take() {
            self.id = rx.recv().ok().flatten();
        }
        self.id.as_ref()
    }
}

pub(crate) struct WorkspaceCheckpoints {
    shadow: Arc<Mutex<Shadow>>,
    turns: Vec<TurnRecord>,
}

impl WorkspaceCheckpoints {
    pub(crate) fn new(state: StateDir, project_root: PathBuf) -> Self {
        Self {
            shadow: Arc::new(Mutex::new(Shadow {
                state,
                project_root,
                opened: None,
            })),
            turns: Vec::new(),
        }
    }

    /// Snapshots the workspace as it is before `turn_index` runs.
    pub(crate) fn snapshot_before_turn(&mut self, turn_index: usize) {
        let (tx, rx) = flume::bounded(1);
        let shadow = Arc::clone(&self.shadow);
        let label = format!("{SNAPSHOT_LABEL_PREFIX} {turn_index}");
        thread::spawn(move || {
            let id = snapshot(&shadow, &label).unwrap_or_else(|e| {
                tracing::warn!(error = %e, turn_index, "workspace snapshot failed");
                None
            });
            let _ = tx.send(id);
        });
        self.turns.push(TurnRecord {
            turn_index,
            id: None,
            pending: Some(rx),
            irreversible: false,
        });
    }

    /// Flags the turn that just ended: its tools did something no snapshot
    /// can take back.
    pub(crate) fn mark_irreversible(&mut self) {
        if let Some(last) = self.turns.last_mut() {
            last.irreversible = true;
        }
    }

    /// The earliest turn a rewind would replay over irreversible work, since
    /// rewinding to a turn also discards every turn after it.
    pub(crate) fn irreversible_from(&self) -> Option<usize> {
        self.turns
            .iter()
            .filter(|t| t.irreversible)
            .map(|t| t.turn_index)
            .min()
    }

    /// Puts the workspace back to the snapshot taken before `turn_index`.
    pub(crate) fn restore_turn(&mut self, turn_index: usize) -> Result<(), String> {
        let record = self
            .turns
            .iter_mut()
            .find(|t| t.turn_index == turn_index)
            .ok_or_else(|| NO_SNAPSHOT_ERR.to_owned())?;
        let id = record
            .resolve()
            .cloned()
            .ok_or_else(|| NO_SNAPSHOT_ERR.to_owned())?;
        lock(&self.shadow)
            .handle()
            .and_then(|c| c.restore(&id))
            .map_err(|e| format!("{RESTORE_FAILED_ERR}: {e}"))
    }

    /// Turns from `turn_index` on no longer exist after a rewind.
    pub(crate) fn forget_from(&mut self, turn_index: usize) {
        self.turns.retain(|t| t.turn_index < turn_index);
    }
}

/// A turn that changed nothing gets no new commit, so it shares the newest
/// one: that commit already describes the workspace it starts from.
fn snapshot(
    shadow: &Mutex<Shadow>,
    label: &str,
) -> Result<Option<CheckpointId>, CheckpointError> {
    let mut guard = lock(shadow);
    let checkpoints = guard.handle()?;
    match checkpoints.create(label)? {
        Some(created) => Ok(Some(created.id)),
        None => Ok(checkpoints.list()?.into_iter().next().map(|c| c.id)),
    }
}

fn lock(shadow: &Mutex<Shadow>) -> MutexGuard<'_, Shadow> {
    shadow.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{NO_SNAPSHOT_ERR, WorkspaceCheckpoints};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const FILE: &str = "notes.txt";
    const BEFORE_FIRST: &str = "before first turn\n";
    const BEFORE_SECOND: &str = "before second turn\n";
    const AFTER_SECOND: &str = "after second turn\n";
    const FIRST_TURN: usize = 0;
    const SECOND_TURN: usize = 2;

    struct Project {
        _state: TempDir,
        root: TempDir,
        checkpoints: WorkspaceCheckpoints,
    }

    impl Project {
        fn new() -> Self {
            let state = TempDir::new().unwrap();
            let root = TempDir::new().unwrap();
            let checkpoints = WorkspaceCheckpoints::new(
                maki_storage::StateDir::from_path(state.path().to_path_buf()),
                root.path().to_path_buf(),
            );
            Self {
                _state: state,
                root,
                checkpoints,
            }
        }

        fn path(&self) -> PathBuf {
            self.root.path().join(FILE)
        }

        fn write(&self, content: &str) {
            fs::write(self.path(), content).unwrap();
        }

        fn read(&self) -> String {
            fs::read_to_string(self.path()).unwrap()
        }

        /// Snapshots run off-thread; resolving one waits for it.
        fn snapshot(&mut self, turn_index: usize) {
            self.checkpoints.snapshot_before_turn(turn_index);
            self.checkpoints
                .turns
                .last_mut()
                .expect("snapshot just pushed a record")
                .resolve();
        }
    }

    #[test]
    fn restore_uses_the_snapshot_taken_before_that_turn() {
        let mut project = Project::new();
        project.write(BEFORE_FIRST);
        project.snapshot(FIRST_TURN);
        project.write(BEFORE_SECOND);
        project.snapshot(SECOND_TURN);
        project.write(AFTER_SECOND);

        project.checkpoints.restore_turn(SECOND_TURN).unwrap();
        assert_eq!(project.read(), BEFORE_SECOND);

        project.checkpoints.restore_turn(FIRST_TURN).unwrap();
        assert_eq!(project.read(), BEFORE_FIRST);
    }

    #[test]
    fn restore_without_a_snapshot_reports_it() {
        let mut project = Project::new();
        assert_eq!(
            project.checkpoints.restore_turn(FIRST_TURN),
            Err(NO_SNAPSHOT_ERR.to_owned())
        );
    }

    #[test]
    fn irreversible_turns_are_reported_from_the_earliest_one() {
        let mut project = Project::new();
        project.write(BEFORE_FIRST);
        project.snapshot(FIRST_TURN);
        assert_eq!(project.checkpoints.irreversible_from(), None);
        project.checkpoints.mark_irreversible();
        project.write(BEFORE_SECOND);
        project.snapshot(SECOND_TURN);
        project.checkpoints.mark_irreversible();
        assert_eq!(project.checkpoints.irreversible_from(), Some(FIRST_TURN));

        project.checkpoints.forget_from(FIRST_TURN);
        assert_eq!(project.checkpoints.irreversible_from(), None);
    }
}
