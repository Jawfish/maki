use std::fs;

use tracing::warn;

use crate::StateDir;

const SEEN_FILE: &str = "onboarding-seen";

/// True the first time maki runs against this state dir, false ever after.
/// The marker is written on the first call, so the caller reads it once at
/// startup and keeps the answer for the run.
pub fn take_first_run(dir: &StateDir) -> bool {
    let path = dir.path().join(SEEN_FILE);
    if path.exists() {
        return false;
    }
    if let Err(e) = fs::write(&path, []) {
        warn!(error = %e, "failed to persist onboarding marker");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_run_is_marked_and_never_repeats() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        assert!(take_first_run(&dir), "fresh state dir is a first run");
        assert!(tmp.path().join(SEEN_FILE).exists(), "marker persisted");
        assert!(!take_first_run(&dir), "second run is routine");
    }
}
