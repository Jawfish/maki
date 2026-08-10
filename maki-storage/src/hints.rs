use std::collections::BTreeSet;
use std::fs;

use tracing::warn;

use crate::{StateDir, atomic_write};

const DISMISSED_FILE: &str = "hints-dismissed";

/// Ids of the discoverability hints the user already dismissed, one per line.
/// Read once at startup, so a hint dismissed in an earlier run never comes
/// back.
pub fn dismissed(dir: &StateDir) -> BTreeSet<String> {
    let path = dir.path().join(DISMISSED_FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return BTreeSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn dismiss(dir: &StateDir, id: &str) {
    let mut ids = dismissed(dir);
    if !ids.insert(id.to_owned()) {
        return;
    }
    let mut body = ids.into_iter().collect::<Vec<_>>().join("\n");
    body.push('\n');
    if let Err(e) = atomic_write(&dir.path().join(DISMISSED_FILE), body.as_bytes()) {
        warn!(error = %e, hint = %id, "failed to persist hint dismissal");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIRST_HINT: &str = "truncated-output";
    const SECOND_HINT: &str = "idle-esc";

    #[test]
    fn dismissals_survive_a_restart() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        assert!(dismissed(&dir).is_empty(), "fresh state dir dismisses none");

        dismiss(&dir, FIRST_HINT);
        dismiss(&dir, SECOND_HINT);
        dismiss(&dir, FIRST_HINT);

        let reopened = StateDir::from_path(tmp.path().to_path_buf());
        let ids = dismissed(&reopened);
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(
            ids.contains(FIRST_HINT) && ids.contains(SECOND_HINT),
            "{ids:?}"
        );
    }
}
