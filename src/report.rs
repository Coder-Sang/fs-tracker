use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use base64::Engine;
use similar::TextDiff;

use crate::capture::{CaptureStore, PathKey};
use crate::contracts::{
    Change, ChangeKind, ConfiguredExclusion, Coverage, DiffRef, FileState, Report, RunMetrics,
    RunState, Status, TargetResult, POLICY_VERSION, SCHEMA_VERSION,
};

pub fn publish_status(output: &Path, status: &Status) -> io::Result<()> {
    atomic_json(output, "status.json", status)
}

pub fn finish(
    store: &mut CaptureStore,
    target: TargetResult,
    mut metrics: RunMetrics,
) -> io::Result<Report> {
    metrics.candidate_paths = store.candidates.len();
    let mut candidates: Vec<(PathKey, _, _)> = store
        .candidates
        .iter()
        .map(|(key, candidate)| {
            (
                key.clone(),
                candidate.path.clone(),
                candidate.before.clone(),
            )
        })
        .collect();
    candidates.sort_by(|left, right| {
        left.0
            .root_id
            .cmp(&right.0.root_id)
            .then(left.0.relative.cmp(&right.0.relative))
    });
    let mut changes = Vec::new();
    let mut patch = Vec::new();
    for (key, path, before) in candidates {
        let after = store.capture_after(&path);
        let Some(kind) = classify(&before, &after) else {
            continue;
        };
        let offset = patch.len() as u64;
        let diff = create_diff(store, &key, &before, &after, &mut patch)?;
        let diff = DiffRef {
            patch_offset: diff.0.then_some(offset),
            patch_bytes: diff.0.then_some((patch.len() as u64) - offset),
            kind: diff.1,
            truncated: diff.2,
            reason: diff.3,
        };
        changes.push(Change {
            root_id: key.root_id.clone(),
            path_bytes_base64: CaptureStore::path_base64(&key),
            display_path: std::str::from_utf8(&key.relative).ok().map(str::to_owned),
            kind,
            before,
            after,
            diff,
        });
    }
    changes.sort_by(|a, b| {
        a.root_id
            .cmp(&b.root_id)
            .then(a.path_bytes_base64.cmp(&b.path_bytes_base64))
    });
    atomic_bytes(store.output(), "changes.patch", &patch)?;
    let state = if store.gaps.is_empty()
        && changes.iter().all(|change| {
            !matches!(change.before, FileState::Unavailable { .. })
                && !matches!(change.after, FileState::Unavailable { .. })
        }) {
        RunState::Finished
    } else {
        RunState::Partial
    };
    metrics.captured_bytes = store.captured_bytes();
    metrics.final_changes = changes.len();
    let report = Report {
        schema_version: SCHEMA_VERSION,
        state: state.clone(),
        assurance: "best_effort".into(),
        target: target.clone(),
        coverage: Coverage {
            policy_version: POLICY_VERSION,
            external_writers: "not_observed_or_controlled".into(),
            supported_operations: vec![
                "open/openat/openat2".into(),
                "truncate/ftruncate".into(),
                "unlink/unlinkat".into(),
                "rename/renameat/renameat2".into(),
                "chmod/fchmod/fchmodat".into(),
                "symlink/symlinkat".into(),
            ],
            unsupported_features: vec![
                "external writable FD passing".into(),
                "external or remote writers".into(),
                "directory rename subtree expansion".into(),
                "hard-link alias discovery".into(),
                "special filesystem ioctls".into(),
            ],
            configured_exclusions: store
                .exclusions()
                .iter()
                .map(|exclusion| ConfiguredExclusion {
                    root_id: exclusion.root_id.clone(),
                    path_bytes_base64: base64::engine::general_purpose::STANDARD
                        .encode(exclusion.relative.as_os_str().as_bytes()),
                    display_path: exclusion.relative.to_string_lossy().into_owned(),
                })
                .collect(),
            configured_recursive_exclusions: store.recursive_exclusions().to_vec(),
            gaps: store.gaps.clone(),
        },
        metrics,
        changes,
    };
    atomic_json(store.output(), "report.json", &report)?;
    let mut diagnostics = Vec::new();
    for gap in &report.coverage.gaps {
        serde_json::to_writer(&mut diagnostics, gap).map_err(io::Error::other)?;
        diagnostics.push(b'\n');
    }
    atomic_bytes(store.output(), "diagnostics.jsonl", &diagnostics)?;
    publish_status(store.output(), &Status::new(state, None, Some(target)))?;
    Ok(report)
}

fn classify(before: &FileState, after: &FileState) -> Option<ChangeKind> {
    match (before, after) {
        (FileState::Absent, FileState::Absent) => None,
        (FileState::Absent, FileState::Present { .. }) => Some(ChangeKind::Added),
        (FileState::Present { .. }, FileState::Absent) => Some(ChangeKind::Deleted),
        (FileState::Present { object: left }, FileState::Present { object: right }) => {
            if left.object_id != right.object_id {
                Some(ChangeKind::Modified)
            } else if left.mode != right.mode {
                Some(ChangeKind::ModeChanged)
            } else {
                None
            }
        }
        (FileState::Unavailable { .. }, _) | (_, FileState::Unavailable { .. }) => {
            Some(ChangeKind::Modified)
        }
    }
}

fn create_diff(
    store: &CaptureStore,
    key: &PathKey,
    before: &FileState,
    after: &FileState,
    output: &mut Vec<u8>,
) -> io::Result<(bool, String, bool, Option<String>)> {
    if let (FileState::Present { object: left }, FileState::Present { object: right }) =
        (before, after)
    {
        if left.object_id == right.object_id {
            return Ok((false, "metadata".into(), false, None));
        }
    }
    let before_bytes = store.read_object(before)?;
    let after_bytes = store.read_object(after)?;
    let before_bytes = before_bytes.as_deref().unwrap_or_default();
    let after_bytes = after_bytes.as_deref().unwrap_or_default();
    let (Ok(before_text), Ok(after_text)) = (
        std::str::from_utf8(before_bytes),
        std::str::from_utf8(after_bytes),
    ) else {
        return Ok((false, "binary".into(), false, Some("not_valid_utf8".into())));
    };
    if before_bytes.contains(&0) || after_bytes.contains(&0) {
        return Ok((false, "binary".into(), false, Some("nul_byte".into())));
    }
    let display = String::from_utf8_lossy(&key.relative);
    let text = TextDiff::from_lines(before_text, after_text)
        .unified_diff()
        .header(&format!("a/{display}"), &format!("b/{display}"))
        .to_string();
    let limit = store.max_diff_bytes();
    let truncated = text.len() > limit;
    if truncated {
        output.extend_from_slice(&text.as_bytes()[..limit]);
    } else {
        output.extend_from_slice(text.as_bytes());
    }
    Ok((true, "unified".into(), truncated, None))
}

pub fn atomic_json<T: serde::Serialize>(output: &Path, name: &str, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_bytes(output, name, &bytes)
}

fn atomic_bytes(output: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = output.join(format!(".{name}.tmp"));
    let final_path = output.join(name);
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, final_path)
}

pub fn print_diff(output: &Path) -> io::Result<()> {
    let bytes = fs::read(output.join("changes.patch"))?;
    io::stdout().write_all(&bytes)
}

pub fn encoded_path(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::ObjectRef;

    fn present(id: &str, mode: &str) -> FileState {
        FileState::Present {
            object: ObjectRef {
                object_id: id.into(),
                size: 1,
                mode: mode.into(),
            },
        }
    }

    #[test]
    fn eliminates_no_change() {
        assert!(classify(&present("x", "100644"), &present("x", "100644")).is_none());
        assert!(matches!(
            classify(&present("x", "100644"), &present("x", "100755")),
            Some(ChangeKind::ModeChanged)
        ));
    }
}
