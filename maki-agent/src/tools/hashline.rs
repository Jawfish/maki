use async_lock::{Mutex as AsyncMutex, MutexGuardArc};
use maki_tree_sitter::resolve_block;
use sha2::{Digest, Sha256};
use similar::{DiffTag, TextDiff};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::hashline_patch::{Edit, parse_patch};

const BOM: &str = "\u{feff}";
const DEFAULT_MAX_VERSIONS_PER_PATH: usize = 8;
const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const STALE_WINDOW_RADIUS: usize = 3;
const NO_OP_LOOP_LIMIT: usize = 3;
const REMAP_WARNING_PREFIX: &str = "warning: stale line anchors remapped";
const VALIDATED_ANCHORS_WARNING_PREFIX: &str =
    "warning: stale file drift detected; line anchors validated unchanged";
const VERIFY_DIFF_GUIDANCE: &str = "verify the diff matches your intent";
const STABLE_DRIFT_WARNING: &str =
    "warning: stale file drift detected; head/tail-only inserts remain position-stable";
const INVALID_TAG_ERROR: &str = "invalid revision tag: expected exactly 16 lowercase ASCII hex characters; use a tag from read or the previous edit result";

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
    pub path: PathBuf,
    pub before: Arc<str>,
    pub after: Arc<str>,
    pub snapshot: Snapshot,
    pub warning: Option<String>,
}

pub struct EditSection<'a> {
    pub path: &'a Path,
    pub tag: &'a str,
    pub patch: &'a str,
}

struct PreflightEdit {
    path: PathBuf,
    before_bytes: Vec<u8>,
    before: String,
    after: String,
    format: TextFormat,
    warning: Option<String>,
}

#[derive(Debug)]
struct ByteEdit {
    range: Range<usize>,
    affected: Option<Range<usize>>,
    block_target: bool,
    replacement: String,
    patch_line: usize,
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

    pub(crate) fn has_two_prior_no_ops(&self, path: &Path, tag: &str, patch: &str) -> bool {
        self.no_op_attempts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&canonical_path(path))
            .is_some_and(|attempt| {
                attempt.tag == tag
                    && attempt.patch == patch
                    && attempt.count >= NO_OP_LOOP_LIMIT - 1
            })
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
        self.edit_sections(&[EditSection { path, tag, patch }])
            .await
            .map(|mut results| results.remove(0))
    }

    pub async fn edit_sections(
        &self,
        sections: &[EditSection<'_>],
    ) -> Result<Vec<EditResult>, String> {
        if sections.is_empty() {
            return Err("edit requires at least one section".into());
        }
        for (index, section) in sections.iter().enumerate() {
            validate_tag(section.tag).map_err(|error| {
                let path = canonical_path(section.path);
                if sections.len() == 1 {
                    format!("{}: {error}", path.display())
                } else {
                    format!("section {} ({}): {error}", index + 1, path.display())
                }
            })?;
        }
        let mut ordered_paths = sections
            .iter()
            .map(|section| canonical_path(section.path))
            .collect::<Vec<_>>();
        ordered_paths.sort();
        if let Some(duplicate) = ordered_paths
            .windows(2)
            .find_map(|paths| (paths[0] == paths[1]).then(|| paths[0].as_path()))
        {
            return Err(format!(
                "duplicate canonical path {}; merge its operations into one section",
                duplicate.display()
            ));
        }

        let mut guards = Vec::with_capacity(ordered_paths.len());
        for path in &ordered_paths {
            guards.push(self.lock_path(path).await);
        }

        let mut preflight = Vec::with_capacity(sections.len());
        for (index, section) in sections.iter().enumerate() {
            let path = canonical_path(section.path);
            let edit = self.preflight(section).map_err(|error| {
                if sections.len() == 1 {
                    format!("{}: {error}", path.display())
                } else {
                    format!("section {} ({}): {error}", index + 1, path.display())
                }
            })?;
            preflight.push(edit);
        }

        let writer = Arc::clone(&self.writer);
        let mut committed: Vec<usize> = Vec::with_capacity(preflight.len());
        for (index, edit) in preflight.iter().enumerate() {
            let path = edit.path.clone();
            let bytes = edit.format.restore(&edit.after).into_bytes();
            let commit_writer = Arc::clone(&writer);
            if let Err(error) = smol::unblock(move || commit_writer(&path, &bytes)).await {
                let mut rollback_errors = Vec::new();
                for landed in committed.iter().rev().map(|&landed| &preflight[landed]) {
                    let path = landed.path.clone();
                    let bytes = landed.before_bytes.clone();
                    let writer = Arc::clone(&writer);
                    if let Err(rollback_error) = smol::unblock(move || writer(&path, &bytes)).await
                    {
                        rollback_errors
                            .push(format!("{}: {rollback_error}", landed.path.display()));
                    }
                }
                if rollback_errors.is_empty() {
                    return Err(format!(
                        "commit failed for {}: {error}; rolled back {} landed section(s)",
                        edit.path.display(),
                        committed.len()
                    ));
                }
                return Err(format!(
                    "commit failed for {}: {error}; rollback incomplete: {}",
                    edit.path.display(),
                    rollback_errors.join(", ")
                ));
            }
            committed.push(index);
        }

        let results = preflight
            .into_iter()
            .map(|edit| {
                self.clear_no_op_attempt(&edit.path);
                let snapshot = self.record_normalized(&edit.path, edit.after.clone(), edit.format);
                EditResult {
                    path: edit.path,
                    before: Arc::from(edit.before),
                    after: Arc::from(edit.after),
                    snapshot,
                    warning: edit.warning,
                }
            })
            .collect();
        drop(guards);
        Ok(results)
    }

    fn preflight(&self, section: &EditSection<'_>) -> Result<PreflightEdit, String> {
        let path = canonical_path(section.path);
        let edits = parse_patch(section.patch).map_err(|error| error.to_string())?;
        let snapshot = self.get(&path, section.tag);
        let before_bytes = fs::read(&path).map_err(|error| format!("read error: {error}"))?;
        let current = String::from_utf8(before_bytes.clone())
            .map_err(|_| "non-utf8 content; re-read cannot proceed".to_owned())?;
        let (before, format) = normalize(&current);
        let current_tag = content_tag(&before);
        let is_stale = snapshot.as_ref().is_none_or(|snapshot| {
            current_tag != section.tag || before.as_str() != snapshot.content.as_ref()
        });
        let (edits, warning) = if is_stale {
            let remapped = snapshot
                .as_ref()
                .map(|snapshot| remap_edits(&snapshot.content, &before, &edits));
            let Some(Ok((edits, warning))) = remapped else {
                let rejection = remapped.and_then(Result::err);
                self.record_normalized(&path, before.clone(), format);
                return Err(stale_error(
                    section.tag,
                    &current_tag,
                    &before,
                    &edits,
                    snapshot.is_some(),
                    rejection,
                ));
            };
            (edits, Some(warning))
        } else {
            (edits, None)
        };
        let byte_edits = lower_edits(&before, &path, &edits)?;
        let after = apply_byte_edits(&before, byte_edits)?;
        if after == before {
            return Err(self.no_op_error(&path, section.tag, section.patch));
        }
        Ok(PreflightEdit {
            path,
            before_bytes,
            before,
            after,
            format,
            warning,
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
        let attempt = attempts
            .entry(path_key.clone())
            .or_insert_with(|| NoOpAttempt {
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
                path_key.display()
            )
        } else {
            format!(
                "patch makes no changes to {} (identical no-op attempt {}/{}); do not retry this payload: use a different patch or inspect the current file",
                path_key.display(),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemapRejection {
    AmbiguousOrChangedContext,
    NonUniformOffsets,
    BlockTarget,
}

impl RemapRejection {
    fn message(self) -> &'static str {
        match self {
            Self::AmbiguousOrChangedContext => {
                "automatic remap rejected: anchor ambiguity or changed surrounding context"
            }
            Self::NonUniformOffsets => {
                "automatic remap rejected: affected anchors moved by non-uniform offsets"
            }
            Self::BlockTarget => "block targets cannot be remapped against stale content",
        }
    }
}

fn remap_edits(
    previous: &str,
    current: &str,
    edits: &[Edit],
) -> Result<(Vec<Edit>, String), RemapRejection> {
    if edits.iter().any(|edit| {
        matches!(
            edit,
            Edit::ReplaceBlock { .. } | Edit::InsertAfterBlock { .. } | Edit::CutBlock { .. }
        )
    }) {
        return Err(RemapRejection::BlockTarget);
    }
    if edits
        .iter()
        .all(|edit| matches!(edit, Edit::InsertHead { .. } | Edit::InsertTail { .. }))
    {
        return Ok((edits.to_vec(), STABLE_DRIFT_WARNING.to_owned()));
    }

    let line_map = unchanged_line_map(previous, current);
    let targeted = targeted_lines(edits);
    if !validate_anchor_context(previous, current, &line_map, &targeted) {
        return Err(RemapRejection::AmbiguousOrChangedContext);
    }

    let mut mappings = Vec::new();
    let mut map_line = |line: usize| {
        let mapped = line_map
            .get(&line)
            .copied()
            .ok_or(RemapRejection::AmbiguousOrChangedContext)?;
        mappings.push((line, mapped));
        Ok(mapped)
    };
    let mut remapped = Vec::with_capacity(edits.len());
    for edit in edits {
        remapped.push(match edit {
            Edit::Replace {
                start,
                end,
                lines,
                patch_line,
            } => Edit::Replace {
                start: map_line(*start)?,
                end: map_line(*end)?,
                lines: lines.clone(),
                patch_line: *patch_line,
            },
            Edit::Cut {
                start,
                end,
                patch_line,
            } => Edit::Cut {
                start: map_line(*start)?,
                end: map_line(*end)?,
                patch_line: *patch_line,
            },
            Edit::InsertBefore {
                line,
                lines,
                patch_line,
            } => Edit::InsertBefore {
                line: map_line(*line)?,
                lines: lines.clone(),
                patch_line: *patch_line,
            },
            Edit::InsertAfter {
                line,
                lines,
                patch_line,
            } => Edit::InsertAfter {
                line: map_line(*line)?,
                lines: lines.clone(),
                patch_line: *patch_line,
            },
            Edit::InsertHead { .. } | Edit::InsertTail { .. } => edit.clone(),
            Edit::ReplaceBlock { .. } | Edit::InsertAfterBlock { .. } | Edit::CutBlock { .. } => {
                unreachable!()
            }
        });
    }
    mappings.sort_unstable();
    mappings.dedup();
    let Some(offset) = mappings
        .first()
        .map(|(old, new)| *new as isize - *old as isize)
    else {
        return Err(RemapRejection::AmbiguousOrChangedContext);
    };
    if !mappings
        .iter()
        .all(|(old, new)| *new as isize - *old as isize == offset)
    {
        return Err(RemapRejection::NonUniformOffsets);
    }
    let warning = if offset == 0 {
        let anchors = mappings
            .iter()
            .map(|(line, _)| line.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{VALIDATED_ANCHORS_WARNING_PREFIX}: {anchors}; {VERIFY_DIFF_GUIDANCE}")
    } else {
        let mappings = mappings
            .iter()
            .map(|(old, new)| format!("{old}→{new}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{REMAP_WARNING_PREFIX}: {mappings}; {VERIFY_DIFF_GUIDANCE}")
    };
    Ok((remapped, warning))
}

fn content_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content
            .strip_suffix('\n')
            .unwrap_or(content)
            .split('\n')
            .collect()
    }
}

fn unchanged_line_map(previous: &str, current: &str) -> HashMap<usize, usize> {
    let diff = TextDiff::from_lines(previous, current);
    let mut map = HashMap::new();
    for operation in diff.ops() {
        if operation.tag() != DiffTag::Equal {
            continue;
        }
        for offset in 0..operation.old_range().len() {
            map.insert(
                operation.old_range().start + offset + 1,
                operation.new_range().start + offset + 1,
            );
        }
    }
    map
}

fn targeted_lines(edits: &[Edit]) -> Vec<usize> {
    let mut lines = Vec::new();
    for edit in edits {
        match edit {
            Edit::Replace { start, end, .. } | Edit::Cut { start, end, .. } => {
                lines.extend(*start..=*end);
            }
            Edit::InsertBefore { line, .. }
            | Edit::InsertAfter { line, .. }
            | Edit::ReplaceBlock { line, .. }
            | Edit::InsertAfterBlock { line, .. }
            | Edit::CutBlock { line, .. } => lines.push(*line),
            Edit::InsertHead { .. } | Edit::InsertTail { .. } => {}
        }
    }
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn validate_anchor_context(
    previous: &str,
    current: &str,
    line_map: &HashMap<usize, usize>,
    targeted: &[usize],
) -> bool {
    let previous_lines = content_lines(previous);
    let current_lines = content_lines(current);
    let previous_duplicates = duplicated_lines(&previous_lines);
    let current_duplicates = duplicated_lines(&current_lines);

    targeted.iter().all(|line| {
        let Some(mapped) = line_map.get(line).copied() else {
            return false;
        };
        let start = targeted.partition_point(|candidate| *candidate < *line);
        let mut run_start = start;
        while run_start > 0 && targeted[run_start - 1] + 1 == targeted[run_start] {
            run_start -= 1;
        }
        let mut run_end = start;
        while run_end + 1 < targeted.len() && targeted[run_end] + 1 == targeted[run_end + 1] {
            run_end += 1;
        }
        let before = targeted[run_start].checked_sub(1).filter(|line| *line > 0);
        let after = (targeted[run_end] < previous_lines.len()).then_some(targeted[run_end] + 1);
        let context_matches = |context: usize| {
            line_map.get(&context).copied()
                == Some((mapped as isize + context as isize - *line as isize) as usize)
        };
        let duplicated = previous_duplicates.contains(previous_lines[*line - 1])
            || current_duplicates.contains(current_lines[mapped - 1]);
        if duplicated {
            let contexts: Vec<_> = [before, after].into_iter().flatten().collect();
            !contexts.is_empty() && contexts.into_iter().all(context_matches)
        } else {
            [before, after].into_iter().flatten().any(context_matches)
        }
    })
}

fn duplicated_lines<'a>(lines: &[&'a str]) -> std::collections::HashSet<&'a str> {
    let mut seen = std::collections::HashSet::new();
    lines
        .iter()
        .copied()
        .filter(|line| !seen.insert(*line))
        .collect()
}

fn lower_edits(content: &str, path: &Path, edits: &[Edit]) -> Result<Vec<ByteEdit>, String> {
    let line_starts = line_starts(content);
    let line_count = if content.is_empty() {
        0
    } else {
        line_starts.len()
    };
    let mut lowered = Vec::with_capacity(edits.len());
    for edit in edits {
        let out_of_bounds = |line: usize| {
            format!(
                "patch line {}: line {line} is out of bounds for a {line_count}-line file",
                edit_patch_line(edit)
            )
        };
        let line_start = |line: usize| {
            line.checked_sub(1)
                .and_then(|index| line_starts.get(index).copied())
                .ok_or_else(|| out_of_bounds(line))
        };
        let lines_text = |lines: &[String]| lines.join("\n");
        let byte_edit = match edit {
            Edit::Replace {
                start,
                end,
                lines,
                patch_line,
            } => {
                let start_byte = line_start(*start)?;
                let end_byte = line_end_with_newline(content, &line_starts, *end)
                    .ok_or_else(|| out_of_bounds(*end))?;
                let mut replacement = lines_text(lines);
                if end_byte > 0 && content.as_bytes().get(end_byte - 1) == Some(&b'\n') {
                    replacement.push('\n');
                }
                ByteEdit {
                    block_target: false,
                    range: start_byte..end_byte,
                    affected: Some(start_byte..line_content_end(content, end_byte)),
                    replacement,
                    patch_line: *patch_line,
                }
            }
            Edit::Cut {
                start,
                end,
                patch_line,
            } => {
                let start_byte = line_start(*start)?;
                let end_byte = line_end_with_newline(content, &line_starts, *end)
                    .ok_or_else(|| out_of_bounds(*end))?;
                ByteEdit {
                    range: start_byte..end_byte,
                    affected: Some(start_byte..line_content_end(content, end_byte)),
                    replacement: String::new(),
                    patch_line: *patch_line,
                    block_target: false,
                }
            }
            Edit::InsertBefore {
                line,
                lines,
                patch_line,
            } => {
                let position = line_start(*line)?;
                let end = line_end_with_newline(content, &line_starts, *line)
                    .ok_or_else(|| out_of_bounds(*line))?;
                ByteEdit {
                    range: position..position,
                    affected: Some(position..line_content_end(content, end)),
                    block_target: false,
                    replacement: format!("{}\n", lines_text(lines)),
                    patch_line: *patch_line,
                }
            }
            Edit::InsertAfter {
                line,
                lines,
                patch_line,
            } => {
                let start = line_start(*line)?;
                let position = line_end_with_newline(content, &line_starts, *line)
                    .ok_or_else(|| out_of_bounds(*line))?;
                let replacement =
                    if content.as_bytes().get(position.wrapping_sub(1)) == Some(&b'\n') {
                        format!("{}\n", lines_text(lines))
                    } else {
                        format!("\n{}", lines_text(lines))
                    };
                ByteEdit {
                    range: position..position,
                    affected: Some(start..line_content_end(content, position)),
                    block_target: false,
                    replacement,
                    patch_line: *patch_line,
                }
            }
            Edit::InsertHead { lines, patch_line } => ByteEdit {
                range: 0..0,
                affected: None,
                block_target: false,
                replacement: if content.is_empty() {
                    lines_text(lines)
                } else {
                    format!("{}\n", lines_text(lines))
                },
                patch_line: *patch_line,
            },
            Edit::InsertTail { lines, patch_line } => ByteEdit {
                range: content.len()..content.len(),
                affected: None,
                block_target: false,
                replacement: if content.is_empty() {
                    lines_text(lines)
                } else if content.ends_with('\n') {
                    format!("{}\n", lines_text(lines))
                } else {
                    format!("\n{}", lines_text(lines))
                },
                patch_line: *patch_line,
            },
            Edit::ReplaceBlock {
                line,
                lines,
                patch_line,
            } => {
                let range = resolve_block(content, path, *line)
                    .map_err(|error| format!("patch line {patch_line}: {error}"))?;
                ByteEdit {
                    range: range.clone(),
                    affected: Some(range),
                    block_target: true,
                    replacement: lines_text(lines),
                    patch_line: *patch_line,
                }
            }
            Edit::CutBlock { line, patch_line } => {
                let range = resolve_block(content, path, *line)
                    .map_err(|error| format!("patch line {patch_line}: {error}"))?;
                ByteEdit {
                    range: range.clone(),
                    affected: Some(range),
                    block_target: true,
                    replacement: String::new(),
                    patch_line: *patch_line,
                }
            }
            Edit::InsertAfterBlock {
                line,
                lines,
                patch_line,
            } => {
                let affected = resolve_block(content, path, *line)
                    .map_err(|error| format!("patch line {patch_line}: {error}"))?;
                ByteEdit {
                    range: affected.end..affected.end,
                    affected: Some(affected),
                    block_target: true,
                    replacement: format!("\n{}", lines_text(lines)),
                    patch_line: *patch_line,
                }
            }
        };
        lowered.push(byte_edit);
    }
    validate_byte_overlaps(&lowered)?;
    Ok(lowered)
}

fn line_starts(content: &str) -> Vec<usize> {
    if content.is_empty() {
        return Vec::new();
    }
    std::iter::once(0)
        .chain(
            content
                .match_indices('\n')
                .map(|(index, _)| index + 1)
                .filter(|start| *start < content.len()),
        )
        .collect()
}

fn line_end_with_newline(content: &str, starts: &[usize], line: usize) -> Option<usize> {
    starts
        .get(line)
        .copied()
        .or_else(|| (line == starts.len()).then_some(content.len()))
}

fn line_content_end(content: &str, end: usize) -> usize {
    end - usize::from(end > 0 && content.as_bytes()[end - 1] == b'\n')
}

fn edit_patch_line(edit: &Edit) -> usize {
    match edit {
        Edit::Replace { patch_line, .. }
        | Edit::ReplaceBlock { patch_line, .. }
        | Edit::InsertBefore { patch_line, .. }
        | Edit::InsertAfter { patch_line, .. }
        | Edit::InsertAfterBlock { patch_line, .. }
        | Edit::InsertHead { patch_line, .. }
        | Edit::InsertTail { patch_line, .. }
        | Edit::Cut { patch_line, .. }
        | Edit::CutBlock { patch_line, .. } => *patch_line,
    }
}

fn validate_byte_overlaps(edits: &[ByteEdit]) -> Result<(), String> {
    for (index, left) in edits.iter().enumerate() {
        let Some(left_range) = &left.affected else {
            continue;
        };
        for right in &edits[index + 1..] {
            let Some(right_range) = &right.affected else {
                continue;
            };
            if (left.block_target || right.block_target)
                && left_range.start < right_range.end
                && right_range.start < left_range.end
            {
                return Err(format!(
                    "patch line {}: resolved byte range {}..{} overlaps {}..{} from patch line {}",
                    right.patch_line,
                    right_range.start,
                    right_range.end,
                    left_range.start,
                    left_range.end,
                    left.patch_line
                ));
            }
        }
    }
    Ok(())
}

fn apply_byte_edits(content: &str, mut edits: Vec<ByteEdit>) -> Result<String, String> {
    edits.sort_unstable_by_key(|edit| (edit.range.start, edit.range.end, edit.patch_line));
    let mut result = content.to_owned();
    for edit in edits.into_iter().rev() {
        if !result.is_char_boundary(edit.range.start) || !result.is_char_boundary(edit.range.end) {
            return Err(format!(
                "patch line {}: resolved edit is not on UTF-8 boundaries",
                edit.patch_line
            ));
        }
        result.replace_range(edit.range, &edit.replacement);
    }
    Ok(result)
}

fn stale_error(
    stale_tag: &str,
    current_tag: &str,
    content: &str,
    edits: &[Edit],
    snapshot_available: bool,
    rejection: Option<RemapRejection>,
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
    let reason = rejection
        .map(|rejection| format!(" {}.", rejection.message()))
        .unwrap_or_default();
    let tag_guidance = if snapshot_available {
        format!("stale tag {stale_tag}. Fresh tag: {current_tag}.")
    } else if stale_tag == current_tag {
        format!(
            "Tag {stale_tag} matches the live normalized content, but its snapshot is unavailable (possibly evicted), so anchors cannot be verified."
        )
    } else {
        format!(
            "Snapshot for supplied tag {stale_tag} is unavailable (possibly evicted). Current tag: {current_tag}."
        )
    };
    let recovery = if !snapshot_available || rejection == Some(RemapRejection::BlockTarget) {
        "Re-read the file to create a verified snapshot, then re-author the patch from that read's tag and numbering; do not retry the stale payload."
    } else {
        "Re-author the patch directly against this fresh tag and window; do not retry the stale payload."
    };
    format!(
        "{tag_guidance}{reason} Current lines around requested anchor {anchor}:\n{window}\n{recovery}"
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

fn validate_tag(tag: &str) -> Result<(), &'static str> {
    if tag.len() == 16
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(INVALID_TAG_ERROR)
    }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use test_case::test_case;

    #[test]
    fn tags_are_stable_lowercase_64_bit_hex() {
        let tag = content_tag("same content");
        assert_eq!(tag.len(), 16);
        assert!(tag.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(tag, content_tag("same content"));
    }

    #[test_case("0123456789abcde"; "short")]
    #[test_case("0123456789abcdef0"; "long")]
    #[test_case("0123456789abcdeF"; "uppercase")]
    #[test_case("0123456789abcdeg"; "nonhex")]
    fn edit_rejects_invalid_tag_before_patch_parsing_or_file_read(tag: &str) {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("missing");
            let state = HashlineState::new();

            let error = state.edit(&path, tag, "not a patch").await.unwrap_err();

            assert_eq!(
                error,
                format!("{}: {INVALID_TAG_ERROR}", canonical_path(&path).display())
            );
        });
    }

    #[test]
    fn invalid_tag_in_multi_section_reports_context_and_writes_nothing() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let first = dir.path().join("first");
            let second = dir.path().join("second");
            fs::write(&first, "one\n").unwrap();
            fs::write(&second, "two\n").unwrap();
            let writes = Arc::new(AtomicUsize::new(0));
            let write_count = Arc::clone(&writes);
            let state = HashlineState::with_writer(Arc::new(move |path, bytes| {
                write_count.fetch_add(1, Ordering::SeqCst);
                fs::write(path, bytes).map_err(|error| error.to_string())
            }));
            let first_tag = state.record(&first, "one\n").tag;
            let sections = [
                EditSection {
                    path: &first,
                    tag: &first_tag,
                    patch: "not a patch",
                },
                EditSection {
                    path: &second,
                    tag: "0123456789ABCDEf",
                    patch: "PUT 1.=1:\n+changed",
                },
            ];

            let error = state.edit_sections(&sections).await.unwrap_err();

            assert_eq!(
                error,
                format!(
                    "section 2 ({}): {INVALID_TAG_ERROR}",
                    canonical_path(&second).display()
                )
            );
            assert_eq!(writes.load(Ordering::SeqCst), 0);
            assert_eq!(fs::read_to_string(&first).unwrap(), "one\n");
            assert_eq!(fs::read_to_string(&second).unwrap(), "two\n");
        });
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
            assert!(
                error.contains("Re-author the patch directly"),
                "got: {error}"
            );
            assert!(!error.contains("Re-read"), "got: {error}");
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
            assert!(error.contains("injected failure"), "got: {error}");
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
    fn stale_insert_above_target_remaps_with_warning_and_fresh_tag() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            let original = "alpha\nbeta\ngamma\n";
            let current = "new\nalpha\nbeta\ngamma\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, original);
            fs::write(&path, current).unwrap();

            let result = state
                .edit(&path, &snapshot.tag, "PUT 2.=2:\n+changed")
                .await
                .unwrap();

            assert_eq!(&*result.after, "new\nalpha\nchanged\ngamma\n");
            assert_eq!(
                result.warning.as_deref(),
                Some(
                    "warning: stale line anchors remapped: 2→3; verify the diff matches your intent"
                )
            );
            assert_eq!(result.snapshot.tag, content_tag(&result.after));
            assert_eq!(fs::read_to_string(path).unwrap(), &*result.after);
        });
    }

    #[test]
    fn stale_target_and_range_interior_changes_fail_closed() {
        smol::block_on(async {
            for (patch, current) in [
                ("PUT 2.=2:\n+new", "one\nchanged\nthree\nfour\n"),
                ("CUT 2.=4", "one\ntwo\nchanged\nfour\n"),
            ] {
                let dir = tempfile::TempDir::new().unwrap();
                let path = dir.path().join("file");
                let original = "one\ntwo\nthree\nfour\n";
                fs::write(&path, original).unwrap();
                let state = HashlineState::new();
                let snapshot = state.record(&path, original);
                fs::write(&path, current).unwrap();

                let error = state.edit(&path, &snapshot.tag, patch).await.unwrap_err();

                assert!(error.contains("stale tag"), "got: {error}");
                assert!(error.contains("Fresh tag:"), "got: {error}");
                assert_eq!(fs::read_to_string(path).unwrap(), current);
            }
        });
    }

    #[test]
    fn stale_duplicate_ambiguity_and_non_uniform_offsets_fail_closed() {
        smol::block_on(async {
            let cases = [
                (
                    "start\nduplicate\nend\n",
                    "start\nduplicate\nmiddle\nduplicate\nend\n",
                    "PUT 2.=2:\n+changed",
                    "anchor ambiguity or changed surrounding context",
                ),
                (
                    "a\nb\nc\nd\ne\nf\n",
                    "above\na\nb\nc\nd\nbetween\ne\nf\n",
                    "PUT 2.=2:\n+B\nPUT 5.=5:\n+E",
                    "non-uniform offsets",
                ),
            ];
            for (original, current, patch, reason) in cases {
                let dir = tempfile::TempDir::new().unwrap();
                let path = dir.path().join("file");
                fs::write(&path, original).unwrap();
                let state = HashlineState::new();
                let snapshot = state.record(&path, original);
                fs::write(&path, current).unwrap();

                let error = state.edit(&path, &snapshot.tag, patch).await.unwrap_err();

                assert!(error.contains("stale tag"), "got: {error}");
                assert!(error.contains(reason), "got: {error}");
                assert_eq!(fs::read_to_string(path).unwrap(), current);
            }
        });
    }

    #[test]
    fn stale_disjoint_edit_validates_unchanged_anchors_without_remap_wording() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            let original = "one\ntwo\nthree\nfour\n";
            let current = "one\ntwo\nthree\nexternal\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, original);
            fs::write(&path, current).unwrap();

            let result = state
                .edit(&path, &snapshot.tag, "PUT 2.=2:\n+changed")
                .await
                .unwrap();

            assert_eq!(&*result.after, "one\nchanged\nthree\nexternal\n");
            assert_eq!(fs::read_to_string(&path).unwrap(), &*result.after);
            assert_eq!(result.snapshot.tag, content_tag(&result.after));
            assert_eq!(
                result.warning.as_deref(),
                Some(
                    "warning: stale file drift detected; line anchors validated unchanged: 2; verify the diff matches your intent"
                )
            );
        });
    }

    #[test]
    fn stale_duplicate_anchor_with_unchanged_neighbors_remaps() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            let original = "left\nduplicate\nright\nother\nduplicate\ntail\n";
            let current = "above\nleft\nduplicate\nright\nother\nduplicate\ntail\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, original);
            fs::write(&path, current).unwrap();

            let result = state
                .edit(&path, &snapshot.tag, "PUT 2.=2:\n+changed")
                .await
                .unwrap();

            assert_eq!(
                &*result.after,
                "above\nleft\nchanged\nright\nother\nduplicate\ntail\n"
            );
            assert_eq!(
                result.warning.as_deref(),
                Some(
                    "warning: stale line anchors remapped: 2→3; verify the diff matches your intent"
                )
            );
        });
    }

    #[test]
    fn stale_head_tail_only_patch_is_position_stable_on_drift() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            let original = "one\ntwo\n";
            let current = "changed\ntwo\nexternal\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, original);
            fs::write(&path, current).unwrap();

            let result = state
                .edit(&path, &snapshot.tag, "PUT <1:\n+head\nPUT >$:\n+tail")
                .await
                .unwrap();

            assert_eq!(&*result.after, "head\nchanged\ntwo\nexternal\ntail\n");
            assert_eq!(result.warning.as_deref(), Some(STABLE_DRIFT_WARNING));
            assert_eq!(result.snapshot.tag, content_tag(&result.after));
        });
    }

    #[test]
    fn mixed_head_insert_still_requires_numbered_anchor_remap() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            let original = "one\ntwo\nthree\n";
            let current = "one\nchanged\nthree\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, original);
            fs::write(&path, current).unwrap();

            let error = state
                .edit(
                    &path,
                    &snapshot.tag,
                    "PUT <1:\n+head\nPUT 2.=2:\n+replacement",
                )
                .await
                .unwrap_err();

            assert!(error.contains("stale tag"), "got: {error}");
            assert_eq!(fs::read_to_string(path).unwrap(), current);
        });
    }

    #[test]
    fn three_file_failure_rolls_back_landed_files_byte_identically() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let paths = [
                dir.path().join("a"),
                dir.path().join("b"),
                dir.path().join("c"),
            ];
            let originals = [
                b"\xef\xbb\xbfone\r\n".as_slice(),
                b"two\n".as_slice(),
                b"three\n".as_slice(),
            ];
            for (path, content) in paths.iter().zip(originals) {
                fs::write(path, content).unwrap();
            }
            let writes = Arc::new(AtomicUsize::new(0));
            let write_count = Arc::clone(&writes);
            let state = HashlineState::with_writer(Arc::new(move |path, bytes| {
                let attempt = write_count.fetch_add(1, Ordering::SeqCst);
                if attempt == 2 {
                    return Err("injected third commit failure".into());
                }
                fs::write(path, bytes).map_err(|error| error.to_string())
            }));
            let tags = paths
                .iter()
                .zip(originals)
                .map(|(path, bytes)| state.record(path, str::from_utf8(bytes).unwrap()).tag)
                .collect::<Vec<_>>();
            let sections = paths
                .iter()
                .zip(&tags)
                .map(|(path, tag)| EditSection {
                    path,
                    tag,
                    patch: "PUT 1.=1:\n+changed",
                })
                .collect::<Vec<_>>();

            let error = state.edit_sections(&sections).await.unwrap_err();

            assert!(
                error.contains("rolled back 2 landed section(s)"),
                "got: {error}"
            );
            for ((path, expected), tag) in paths.iter().zip(originals).zip(tags) {
                assert_eq!(fs::read(path).unwrap(), expected);
                assert!(state.get(path, &tag).is_some());
                assert!(state.get(path, &content_tag("changed\n")).is_none());
            }
        });
    }

    #[test]
    fn batch_preflight_failure_writes_nothing_and_duplicate_canonical_path_rejects() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let first = dir.path().join("first");
            let second = dir.path().join("second");
            fs::write(&first, "one\n").unwrap();
            fs::write(&second, "two\n").unwrap();
            let writes = Arc::new(AtomicUsize::new(0));
            let write_count = Arc::clone(&writes);
            let state = HashlineState::with_writer(Arc::new(move |path, bytes| {
                write_count.fetch_add(1, Ordering::SeqCst);
                fs::write(path, bytes).map_err(|error| error.to_string())
            }));
            let first_tag = state.record(&first, "one\n").tag;
            let second_tag = state.record(&second, "two\n").tag;
            fs::write(&second, "changed\n").unwrap();
            let stale = [
                EditSection {
                    path: &first,
                    tag: &first_tag,
                    patch: "PUT 1.=1:\n+first",
                },
                EditSection {
                    path: &second,
                    tag: &second_tag,
                    patch: "PUT 1.=1:\n+second",
                },
            ];

            let error = state.edit_sections(&stale).await.unwrap_err();
            assert!(error.contains("section 2"), "got: {error}");
            assert!(
                error.contains(&canonical_path(&second).display().to_string()),
                "got: {error}"
            );
            assert!(error.contains("stale tag"), "got: {error}");
            let canonical_second = canonical_path(&second).display().to_string();
            assert_eq!(error.matches(&canonical_second).count(), 1, "got: {error}");
            assert_eq!(writes.load(Ordering::SeqCst), 0);
            assert_eq!(fs::read_to_string(&first).unwrap(), "one\n");

            let alias = dir.path().join("alias");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&first, &alias).unwrap();
            #[cfg(not(unix))]
            let alias = first.clone();
            let duplicate = [
                EditSection {
                    path: &first,
                    tag: &first_tag,
                    patch: "PUT 1.=1:\n+first",
                },
                EditSection {
                    path: &alias,
                    tag: &first_tag,
                    patch: "PUT 1.=1:\n+alias",
                },
            ];
            let error = state.edit_sections(&duplicate).await.unwrap_err();
            assert!(error.contains("duplicate canonical path"), "got: {error}");
            assert!(error.contains("merge"), "got: {error}");

            let malformed = [
                EditSection {
                    path: &first,
                    tag: &first_tag,
                    patch: "PUT 1.=1:\n+first",
                },
                EditSection {
                    path: &second,
                    tag: &second_tag,
                    patch: "not a patch",
                },
            ];
            let error = state.edit_sections(&malformed).await.unwrap_err();
            assert!(error.contains("section 2"), "got: {error}");
            assert!(
                error.contains(&canonical_path(&second).display().to_string()),
                "got: {error}"
            );
        });
    }

    #[test]
    fn unavailable_matching_snapshot_requires_reread_without_fake_fresh_tag() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("file");
            fs::write(&path, "one\ntwo\n").unwrap();
            let state = HashlineState::with_limits(0, DEFAULT_MAX_TOTAL_BYTES);
            let tag = state.record(&path, "one\ntwo\n").tag;

            let error = state
                .edit(&path, &tag, "PUT 2.=2:\n+changed")
                .await
                .unwrap_err();

            assert!(error.contains("snapshot is unavailable"), "got: {error}");
            assert!(
                error.contains("matches the live normalized content"),
                "got: {error}"
            );
            assert!(error.contains("Re-read"), "got: {error}");
            assert!(!error.contains("Fresh tag"), "got: {error}");
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

    #[test]
    fn block_operations_apply_exact_bytes_and_mix_with_numeric_edits() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("sample.rs");
            let original =
                "fn one() {} fn adjacent() {}\n\nfn two() {\n    old();\n}\n\nfn three() {}\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let tag = state.record(&path, original).tag;

            let replaced = state
                .edit(&path, &tag, "PUT 1*:\n+fn first() {}")
                .await
                .unwrap();
            assert_eq!(
                &*replaced.after,
                "fn first() {} fn adjacent() {}\n\nfn two() {\n    old();\n}\n\nfn three() {}\n"
            );

            let mixed = state
                .edit(
                    &path,
                    &replaced.snapshot.tag,
                    "CUT 3*\nPUT >7*:\n+fn four() {}\nPUT <1:\n+// header",
                )
                .await
                .unwrap();
            assert_eq!(
                &*mixed.after,
                "// header\nfn first() {} fn adjacent() {}\n\n\n\nfn three() {}\nfn four() {}\n"
            );
        });
    }

    #[test]
    fn block_preflight_failures_leave_every_section_byte_identical() {
        smol::block_on(async {
            for (patch, expected_error) in [
                ("PUT 1*:\n+fn changed() {}\nCUT 2.=2", "overlaps"),
                ("CUT 1*", "unsupported file"),
                ("CUT 1*", "no complete syntax block"),
            ] {
                let dir = tempfile::TempDir::new().unwrap();
                let first = dir.path().join("sample.rs");
                let second = dir.path().join(if expected_error == "unsupported file" {
                    "sample.txt"
                } else {
                    "other.rs"
                });
                let first_bytes = b"fn first() {\n    body();\n}\n";
                let second_bytes = if expected_error == "overlaps" {
                    b"fn second() {\n    body();\n}\n".as_slice()
                } else if expected_error == "no complete syntax block" {
                    b"use std::fmt;\n".as_slice()
                } else {
                    b"fn second() {}\n".as_slice()
                };
                fs::write(&first, first_bytes).unwrap();
                fs::write(&second, second_bytes).unwrap();
                let writes = Arc::new(AtomicUsize::new(0));
                let write_count = Arc::clone(&writes);
                let state = HashlineState::with_writer(Arc::new(move |path, bytes| {
                    write_count.fetch_add(1, Ordering::SeqCst);
                    fs::write(path, bytes).map_err(|error| error.to_string())
                }));
                let first_tag = state
                    .record(&first, str::from_utf8(first_bytes).unwrap())
                    .tag;
                let second_tag = state
                    .record(&second, str::from_utf8(second_bytes).unwrap())
                    .tag;
                let sections = [
                    EditSection {
                        path: &first,
                        tag: &first_tag,
                        patch: "PUT >$:\n+landed",
                    },
                    EditSection {
                        path: &second,
                        tag: &second_tag,
                        patch,
                    },
                ];

                let error = state.edit_sections(&sections).await.unwrap_err();

                assert!(error.contains(expected_error), "got: {error}");
                assert_eq!(writes.load(Ordering::SeqCst), 0);
                assert_eq!(fs::read(&first).unwrap(), first_bytes);
                assert_eq!(fs::read(&second).unwrap(), second_bytes);
            }
        });
    }

    #[test]
    fn stale_block_fails_closed_while_numeric_stale_remapping_still_works() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("sample.rs");
            let original = "fn first() {}\nfn second() {}\n";
            let current = "// new\nfn first() {}\nfn second() {}\n";
            fs::write(&path, original).unwrap();
            let state = HashlineState::new();
            let snapshot = state.record(&path, original);
            fs::write(&path, current).unwrap();

            let error = state
                .edit(&path, &snapshot.tag, "CUT 1*")
                .await
                .unwrap_err();
            assert!(
                error.contains("block targets cannot be remapped"),
                "got: {error}"
            );
            assert!(error.contains("Re-read the file"), "got: {error}");
            assert_eq!(fs::read_to_string(&path).unwrap(), current);

            let remapped = state
                .edit(&path, &snapshot.tag, "PUT 2.=2:\n+fn changed() {}")
                .await
                .unwrap();
            assert_eq!(&*remapped.after, "// new\nfn first() {}\nfn changed() {}\n");
            assert!(remapped.warning.is_some());
        });
    }

    #[test]
    fn block_edits_restore_bom_and_crlf() {
        smol::block_on(async {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("sample.rs");
            let raw = "\u{feff}fn first() {}\r\nfn second() {}\r\n";
            fs::write(&path, raw).unwrap();
            let state = HashlineState::new();
            let tag = state.record(&path, raw).tag;

            state
                .edit(&path, &tag, "PUT 1*:\n+fn changed() {}")
                .await
                .unwrap();

            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                "\u{feff}fn changed() {}\r\nfn second() {}\r\n"
            );
        });
    }
}
