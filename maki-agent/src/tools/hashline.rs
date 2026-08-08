use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_lock::{Mutex as AsyncMutex, MutexGuardArc};
use sha2::{Digest, Sha256};

use super::hashline_patch::{apply_patch, parse_patch};

const BOM: &str = "\u{feff}";
const DEFAULT_MAX_VERSIONS_PER_PATH: usize = 8;
const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const STALE_WINDOW_RADIUS: usize = 3;
const NO_OP_LOOP_LIMIT: usize = 3;

pub type ContentTag = String;

type AtomicWriter = dyn Fn(&Path, &[u8]) -> Result<(), String> + Send + Sync;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TextFormat {
    pub bom: bool,
    pub crlf: bool,
}

impl TextFormat {
    pub fn restore(self, normalized: &str) -> String {
        let line_endings = if self.crlf {
            normalized.replace('\n', "\r\n")
        } else {
            normalized.to_owned()
        };
        if self.bom {
            format!("{BOM}{line_endings}")
        } else {
            line_endings
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub content: Arc<str>,
    pub format: TextFormat,
    pub tag: ContentTag,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrittenSnapshot {
    pub bytes: usize,
    pub snapshot: Snapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditResult {
    pub before: Arc<str>,
    pub after: Arc<str>,
    pub snapshot: Snapshot,
}

#[derive(Debug)]
struct StoredSnapshot {
    snapshot: Snapshot,
    sequence: u64,
}

#[derive(Debug, Default)]
struct SnapshotStore {
    paths: HashMap<PathBuf, VecDeque<StoredSnapshot>>,
    total_bytes: usize,
    next_sequence: u64,
}

#[derive(Debug)]
struct NoOpAttempt {
    tag: String,
    patch: String,
    count: usize,
}

pub struct HashlineState {
    store: Mutex<SnapshotStore>,
    locks: Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
    no_op_attempts: Mutex<HashMap<PathBuf, NoOpAttempt>>,
    max_versions_per_path: usize,
    max_total_bytes: usize,
    writer: Arc<AtomicWriter>,
}

impl Default for HashlineState {
    fn default() -> Self {
        Self::new()
    }
}

impl HashlineState {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_VERSIONS_PER_PATH, DEFAULT_MAX_TOTAL_BYTES)
    }

    pub fn with_limits(max_versions_per_path: usize, max_total_bytes: usize) -> Self {
        Self {
            store: Mutex::new(SnapshotStore::default()),
            locks: Mutex::new(HashMap::new()),
            no_op_attempts: Mutex::new(HashMap::new()),
            max_versions_per_path,
            max_total_bytes,
            writer: Arc::new(|path, bytes| {
                maki_storage::atomic_write(path, bytes).map_err(|error| error.to_string())
            }),
        }
    }

    #[cfg(test)]
    fn with_writer(writer: Arc<AtomicWriter>) -> Self {
        Self {
            writer,
            ..Self::new()
        }
    }

    pub fn record(&self, path: &Path, raw_content: &str) -> Snapshot {
        let (content, format) = normalize(raw_content);
        self.record_normalized(path, content, format)
    }

    pub fn record_normalized(&self, path: &Path, content: String, format: TextFormat) -> Snapshot {
        let path = canonical_path(path);
        let snapshot = Snapshot {
            tag: content_tag(&content),
            content: Arc::from(content),
            format,
        };
        let mut store = self.store.lock().unwrap_or_else(|error| error.into_inner());
        let existing = store.paths.get(&path).and_then(|versions| {
            versions.iter().find(|stored| {
                stored.snapshot.tag == snapshot.tag
                    && stored.snapshot.content == snapshot.content
                    && stored.snapshot.format == snapshot.format
            })
        });
        if existing.is_some() {
            return snapshot;
        }

        let sequence = store.next_sequence;
        store.next_sequence = store.next_sequence.wrapping_add(1);
        store.total_bytes += snapshot.content.len();
        let removed_bytes = {
            let versions = store.paths.entry(path.clone()).or_default();
            versions.push_back(StoredSnapshot {
                snapshot: snapshot.clone(),
                sequence,
            });
            let mut removed_bytes = 0;
            while versions.len() > self.max_versions_per_path {
                if let Some(removed) = versions.pop_front() {
                    removed_bytes += removed.snapshot.content.len();
                }
            }
            removed_bytes
        };
        store.total_bytes -= removed_bytes;
        self.evict_to_byte_limit(&mut store);
        snapshot
    }

    pub fn get(&self, path: &Path, tag: &str) -> Option<Snapshot> {
        let path = canonical_path(path);
        self.store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .paths
            .get(&path)?
            .iter()
            .find(|stored| stored.snapshot.tag == tag)
            .map(|stored| stored.snapshot.clone())
    }

    pub async fn lock_path(&self, path: &Path) -> MutexGuardArc<()> {
        let path = canonical_path(path);
        let lock = self
            .locks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(path)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone();
        lock.lock_arc().await
    }

    pub async fn edit(&self, path: &Path, tag: &str, patch: &str) -> Result<EditResult, String> {
        let edits = parse_patch(patch).map_err(|error| error.to_string())?;
        let snapshot = self.get(path, tag);
        let _guard = self.lock_path(path).await;
        let bytes = fs::read(path).map_err(|error| format!("read error: {error}"))?;
        let current = String::from_utf8(bytes).map_err(|_| {
            format!(
                "non-utf8 content: {}; re-read cannot proceed",
                path.display()
            )
        })?;
        let (before, format) = normalize(&current);
        let current_tag = content_tag(&before);
        let is_stale = snapshot.as_ref().is_none_or(|snapshot| {
            current_tag != tag || before.as_str() != snapshot.content.as_ref()
        });
        if is_stale {
            self.record_normalized(path, before.clone(), format);
            return Err(stale_error(path, tag, &current_tag, &before, &edits));
        }
        let after = apply_patch(&before, &edits).map_err(|error| error.to_string())?;
        if after == before {
            return Err(self.no_op_error(path, tag, patch));
        }
        self.clear_no_op_attempt(path);

        let restored = format.restore(&after).into_bytes();
        let writer = Arc::clone(&self.writer);
        let write_path = path.to_path_buf();
        smol::unblock(move || writer(&write_path, &restored)).await?;
        let written = self.record_normalized(path, after.clone(), format);
        Ok(EditResult {
            before: Arc::from(before),
            after: Arc::from(after),
            snapshot: written,
        })
    }

    pub async fn write(&self, path: &Path, content: &str) -> Result<WrittenSnapshot, String> {
        let _guard = self.lock_path(path).await;
        self.clear_no_op_attempt(path);
        let (normalized, supplied_format) = normalize(content);
        let format = match fs::read(path) {
            Ok(bytes) => {
                let current = String::from_utf8(bytes)
                    .map_err(|_| format!("non-utf8 content: {}", path.display()))?;
                let (_, current_format) = normalize(&current);
                TextFormat {
                    bom: supplied_format.bom || current_format.bom,
                    crlf: supplied_format.crlf || current_format.crlf,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => supplied_format,
            Err(error) => return Err(error.to_string()),
        };
        let restored = format.restore(&normalized);
        let path = path.to_path_buf();
        let bytes = restored.into_bytes();
        let byte_count = bytes.len();
        let writer = Arc::clone(&self.writer);
        let write_path = path.clone();
        smol::unblock(move || writer(&write_path, &bytes)).await?;
        let snapshot = self.record_normalized(&path, normalized, format);
        Ok(WrittenSnapshot {
            bytes: byte_count,
            snapshot,
        })
    }

    fn no_op_error(&self, path: &Path, tag: &str, patch: &str) -> String {
        let path_key = canonical_path(path);
        let mut attempts = self
            .no_op_attempts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let attempt = attempts.entry(path_key).or_insert_with(|| NoOpAttempt {
            tag: tag.to_owned(),
            patch: patch.to_owned(),
            count: 0,
        });
        if attempt.tag == tag && attempt.patch == patch {
            attempt.count += 1;
        } else {
            *attempt = NoOpAttempt {
                tag: tag.to_owned(),
                patch: patch.to_owned(),
                count: 1,
            };
        }
        if attempt.count >= NO_OP_LOOP_LIMIT {
            format!(
                "hard failure: no-op edit loop for {}: the same payload made no changes {NO_OP_LOOP_LIMIT} times; stop retrying it and inspect the current file",
                path.display()
            )
        } else {
            format!(
                "patch makes no changes to {} (identical no-op attempt {}/{}); do not retry this payload: use a different patch or inspect the current file",
                path.display(),
                attempt.count,
                NO_OP_LOOP_LIMIT
            )
        }
    }

    fn clear_no_op_attempt(&self, path: &Path) {
        self.no_op_attempts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&canonical_path(path));
    }

    fn evict_to_byte_limit(&self, store: &mut SnapshotStore) {
        while store.total_bytes > self.max_total_bytes {
            let oldest_path = store
                .paths
                .iter()
                .filter_map(|(path, versions)| versions.front().map(|v| (path.clone(), v.sequence)))
                .min_by_key(|(_, sequence)| *sequence)
                .map(|(path, _)| path);
            let Some(path) = oldest_path else {
                break;
            };
            let versions = store.paths.get_mut(&path).expect("path came from store");
            if let Some(removed) = versions.pop_front() {
                store.total_bytes -= removed.snapshot.content.len();
            }
            if versions.is_empty() {
                store.paths.remove(&path);
            }
        }
    }
}

fn stale_error(
    path: &Path,
    stale_tag: &str,
    current_tag: &str,
    content: &str,
    edits: &[super::hashline_patch::Edit],
) -> String {
    let lines: Vec<_> = content.lines().collect();
    let anchor = edits
        .first()
        .and_then(super::hashline_patch::Edit::anchor)
        .unwrap_or_else(|| lines.len().max(1));
    let center = anchor.clamp(1, lines.len().max(1));
    let start = center.saturating_sub(STALE_WINDOW_RADIUS).max(1);
    let end = center.saturating_add(STALE_WINDOW_RADIUS).min(lines.len());
    let window = if lines.is_empty() {
        "(file is empty)".to_owned()
    } else {
        (start..=end)
            .map(|line| format!("{line}: {}", lines[line - 1]))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "stale tag {stale_tag} for {}. Fresh tag: {current_tag}. Current lines around requested anchor {anchor}:\n{window}\nRe-author the patch against this fresh tag and numbering; do not retry the stale payload.",
        path.display()
    )
}

pub fn normalize(content: &str) -> (String, TextFormat) {
    let (content, bom) = content
        .strip_prefix(BOM)
        .map_or((content, false), |content| (content, true));
    let crlf_count = content.matches("\r\n").count();
    let lf_count = content.matches('\n').count();
    (
        content.replace("\r\n", "\n"),
        TextFormat {
            bom,
            crlf: crlf_count > lf_count.saturating_sub(crlf_count),
        },
    )
}

pub fn content_tag(content: &str) -> ContentTag {
    let digest = Sha256::digest(content.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        let Some(parent) = path.parent() else {
            return path.to_path_buf();
        };
        fs::canonicalize(parent)
            .map(|parent| parent.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|_| path.to_path_buf())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn tags_are_stable_lowercase_64_bit_hex() {
        let tag = content_tag("same content");
        assert_eq!(tag.len(), 16);
        assert!(tag.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(tag, content_tag("same content"));
    }

    #[test]
    fn normalization_round_trips_bom_and_dominant_crlf() {
        let raw = "\u{feff}one\r\ntwo\r\nthree\n";
        let (normalized, format) = normalize(raw);
        assert_eq!(normalized, "one\ntwo\nthree\n");
        assert_eq!(
            format,
            TextFormat {
                bom: true,
                crlf: true
            }
        );
        assert_eq!(
            format.restore(&normalized),
            "\u{feff}one\r\ntwo\r\nthree\r\n"
        );
    }

    #[test]
    fn store_evicts_per_path_and_total_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let store = HashlineState::with_limits(2, 5);
        let old = store.record(&first, "aa");
        let middle = store.record(&first, "bb");
        let current = store.record(&first, "cc");
        assert!(store.get(&first, &old.tag).is_none());
        assert_eq!(store.get(&first, &middle.tag), Some(middle.clone()));
        store.record(&second, "ddd");
        assert!(store.get(&first, &middle.tag).is_none());
        assert_eq!(store.get(&first, &current.tag), Some(current));
    }

    #[test]
    fn canonical_path_unifies_existing_symlink_targets() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, "content").unwrap();
        assert_eq!(canonical_path(&path), fs::canonicalize(path).unwrap());
    }

    #[test]
    fn failed_atomic_write_leaves_previous_content_and_store_unchanged() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "before").unwrap();
            let attempted = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&attempted);
            let state = HashlineState::with_writer(Arc::new(move |_, _| {
                flag.store(true, Ordering::SeqCst);
                Err("injected failure".into())
            }));

            let error = state.write(&path, "after").await.unwrap_err();
            assert_eq!(error, "injected failure");
            assert!(attempted.load(Ordering::SeqCst));
            assert_eq!(fs::read_to_string(&path).unwrap(), "before");
            assert!(state.get(&path, &content_tag("after")).is_none());
        });
    }

    #[test]
    fn edit_rejects_stale_same_mtime_content_and_invalid_utf8() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "one\ntwo\n").unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, "one\ntwo\n");
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            fs::write(&path, "one\nchanged\n").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();

            let error = state
                .edit(&path, &snapshot.tag, "PUT 2.=2:\n+new")
                .await
                .unwrap_err();
            assert!(error.contains("stale tag"), "got: {error}");
            assert!(
                error.contains(&format!("Fresh tag: {}", content_tag("one\nchanged\n"))),
                "got: {error}"
            );
            assert!(error.contains("1: one\n2: changed"), "got: {error}");
            assert!(error.contains("requested anchor 2"), "got: {error}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "one\nchanged\n");

            fs::write(&path, [0xff]).unwrap();
            let error = state
                .edit(&path, &snapshot.tag, "PUT 2.=2:\n+new")
                .await
                .unwrap_err();
            assert!(error.contains("non-utf8 content"), "got: {error}");
        });
    }

    #[test]
    fn concurrent_edits_serialize_and_second_tag_fails_stale() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "one\n").unwrap();
            let state = Arc::new(HashlineState::new());
            let tag = state.record(&path, "one\n").tag;
            let first = {
                let state = Arc::clone(&state);
                let path = path.clone();
                let tag = tag.clone();
                smol::spawn(async move { state.edit(&path, &tag, "PUT 1.=1:\n+first").await })
            };
            let second = {
                let state = Arc::clone(&state);
                let path = path.clone();
                smol::spawn(async move { state.edit(&path, &tag, "PUT 1.=1:\n+second").await })
            };
            let (first, second) = futures_lite::future::zip(first, second).await;
            assert!(first.is_ok() ^ second.is_ok());
            assert!(first.is_err() || second.is_err());
            let content = fs::read_to_string(&path).unwrap();
            assert!(content == "first\n" || content == "second\n");
        });
    }

    #[test]
    fn failed_edit_write_leaves_file_and_snapshot_unchanged() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "before\n").unwrap();
            let state = HashlineState::with_writer(Arc::new(|_, _| Err("injected failure".into())));
            let snapshot = state.record(&path, "before\n");

            let error = state
                .edit(&path, &snapshot.tag, "PUT 1.=1:\n+after")
                .await
                .unwrap_err();
            assert_eq!(error, "injected failure");
            assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
            assert!(state.get(&path, &content_tag("after\n")).is_none());
        });
    }

    #[test]
    fn edit_chains_tags_and_rejects_no_op() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "one\ntwo\n").unwrap();
            let state = HashlineState::new();
            let initial = state.record(&path, "one\ntwo\n");
            let first = state
                .edit(&path, &initial.tag, "PUT 2.=2:\n+second")
                .await
                .unwrap();
            let second = state
                .edit(&path, &first.snapshot.tag, "PUT >2:\n+three")
                .await
                .unwrap();
            assert_eq!(&*second.after, "one\nsecond\nthree\n");

            let error = state
                .edit(&path, &second.snapshot.tag, "PUT 2.=2:\n+second")
                .await
                .unwrap_err();
            assert!(error.contains("no changes"), "got: {error}");
        });
    }

    #[test]
    fn third_identical_no_op_names_loop_and_changed_payload_resets_count() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "one\ntwo\n").unwrap();
            let state = HashlineState::new();
            let tag = state.record(&path, "one\ntwo\n").tag;
            let patch = "PUT 2.=2:\n+two";

            for expected_attempt in 1..NO_OP_LOOP_LIMIT {
                let error = state.edit(&path, &tag, patch).await.unwrap_err();
                assert!(
                    error.contains(&format!("identical no-op attempt {expected_attempt}/3")),
                    "got: {error}"
                );
            }
            let error = state.edit(&path, &tag, patch).await.unwrap_err();
            assert!(
                error.contains("hard failure: no-op edit loop"),
                "got: {error}"
            );
            assert!(error.contains("same payload"), "got: {error}");
            assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");

            let changed = state
                .edit(&path, &tag, "PUT 1.=1:\n+one")
                .await
                .unwrap_err();
            assert!(
                changed.contains("identical no-op attempt 1/3"),
                "got: {changed}"
            );
        });
    }

    #[test]
    fn stale_window_centers_first_anchor_and_clamps_to_current_file() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            let original = (1..=12)
                .map(|line| format!("old {line}\n"))
                .collect::<String>();
            let current = (1..=12)
                .map(|line| format!("new {line}\n"))
                .collect::<String>();
            fs::write(&path, &original).unwrap();
            let state = HashlineState::new();
            let tag = state.record(&path, &original).tag;
            fs::write(&path, &current).unwrap();

            let error = state
                .edit(&path, &tag, "PUT 9.=9:\n+replacement")
                .await
                .unwrap_err();
            assert!(error.contains("6: new 6"), "got: {error}");
            assert!(error.contains("12: new 12"), "got: {error}");
            assert!(!error.contains("5: new 5"), "got: {error}");
            assert!(state.get(&path, &content_tag(&current)).is_some());
        });
    }

    #[test]
    fn write_preserves_existing_bom_and_crlf_and_records_normalized_snapshot() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "\u{feff}before\r\n").unwrap();
            let state = HashlineState::new();

            let written = state.write(&path, "after\n").await.unwrap();

            assert_eq!(fs::read_to_string(&path).unwrap(), "\u{feff}after\r\n");
            assert_eq!(&*written.snapshot.content, "after\n");
            assert_eq!(
                written.snapshot.format,
                TextFormat {
                    bom: true,
                    crlf: true
                }
            );
            assert_eq!(
                state.get(&path, &written.snapshot.tag),
                Some(written.snapshot)
            );
        });
    }
}
