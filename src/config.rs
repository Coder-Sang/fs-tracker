use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

pub const TRACKING_POLICY_SCHEMA_VERSION: u32 = 1;
const MAX_TRACKING_POLICY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Root {
    pub id: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    pub root_id: String,
    pub relative: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TrackingPolicy {
    pub roots: Vec<Root>,
    pub exclusions: Vec<Exclusion>,
    pub recursive_exclusions: Vec<String>,
    pub limits: Limits,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_diff_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_file_bytes: 32 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_diff_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct RunConfig {
    pub roots: Vec<Root>,
    pub exclusions: Vec<Exclusion>,
    pub recursive_exclusions: Vec<String>,
    pub output: PathBuf,
    pub command: Vec<OsString>,
    pub limits: Limits,
    pub capture_workers: usize,
    pub notification_queue: usize,
    pub finish_fd: Option<i32>,
    pub termination_grace: std::time::Duration,
    pub capture_timeout: std::time::Duration,
    pub capture_helper_delay: std::time::Duration,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("tracking policy cannot exceed {max_bytes} bytes: {path}")]
    PolicyTooLarge { path: PathBuf, max_bytes: usize },
    #[error("cannot read tracking policy {path}: {source}")]
    PolicyIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid tracking policy JSON {path}: {source}")]
    PolicyJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("unsupported tracking policy schema version: {0}")]
    UnsupportedPolicyVersion(u32),
    #[error("at least one tracked root is required")]
    MissingRoots,
    #[error("root must use ID=PATH syntax: {0}")]
    InvalidRoot(String),
    #[error("root ID must contain only ASCII letters, digits, '_' or '-': {0}")]
    InvalidRootId(String),
    #[error("duplicate root ID: {0}")]
    DuplicateRootId(String),
    #[error("exclusion must use ROOT_ID=RELATIVE_PATH syntax: {0}")]
    InvalidExclusion(String),
    #[error("exclusion references unknown root ID: {0}")]
    UnknownExclusionRoot(String),
    #[error("exclusion path must be a non-empty normalized relative path: {0}")]
    InvalidExclusionPath(String),
    #[error("recursive exclusion must be one non-empty path component: {0}")]
    InvalidRecursiveExclusion(String),
    #[error("cannot resolve root {path}: {source}")]
    RootIo {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("roots overlap: {0} and {1}")]
    OverlappingRoots(PathBuf, PathBuf),
    #[error("output already exists: {0}")]
    OutputExists(PathBuf),
    #[error("output cannot be inside a tracked root: {0}")]
    OutputInsideRoot(PathBuf),
    #[error("no command provided after --")]
    MissingCommand,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrackingPolicyDocument {
    schema_version: u32,
    roots: Vec<RootDocument>,
    #[serde(default)]
    exclusions: Vec<ExclusionDocument>,
    #[serde(default)]
    recursive_exclusions: Vec<String>,
    #[serde(default)]
    limits: LimitsDocument,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootDocument {
    id: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExclusionDocument {
    root_id: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct LimitsDocument {
    max_files: usize,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_diff_bytes: usize,
}

impl Default for LimitsDocument {
    fn default() -> Self {
        let limits = Limits::default();
        Self {
            max_files: limits.max_files,
            max_file_bytes: limits.max_file_bytes,
            max_total_bytes: limits.max_total_bytes,
            max_diff_bytes: limits.max_diff_bytes,
        }
    }
}

impl From<LimitsDocument> for Limits {
    fn from(document: LimitsDocument) -> Self {
        Self {
            max_files: document.max_files,
            max_file_bytes: document.max_file_bytes,
            max_total_bytes: document.max_total_bytes,
            max_diff_bytes: document.max_diff_bytes,
        }
    }
}

pub fn load_tracking_policy(path: &Path) -> Result<TrackingPolicy, ConfigError> {
    let metadata = fs::metadata(path).map_err(|source| ConfigError::PolicyIo {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_TRACKING_POLICY_BYTES as u64 {
        return Err(ConfigError::PolicyTooLarge {
            path: path.to_path_buf(),
            max_bytes: MAX_TRACKING_POLICY_BYTES,
        });
    }
    let bytes = fs::read(path).map_err(|source| ConfigError::PolicyIo {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.len() > MAX_TRACKING_POLICY_BYTES {
        return Err(ConfigError::PolicyTooLarge {
            path: path.to_path_buf(),
            max_bytes: MAX_TRACKING_POLICY_BYTES,
        });
    }
    let document: TrackingPolicyDocument =
        serde_json::from_slice(&bytes).map_err(|source| ConfigError::PolicyJson {
            path: path.to_path_buf(),
            source,
        })?;
    if document.schema_version != TRACKING_POLICY_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedPolicyVersion(
            document.schema_version,
        ));
    }
    let roots = resolve_roots(
        document
            .roots
            .into_iter()
            .map(|root| (root.id, PathBuf::from(root.path))),
    )?;
    let exclusions = resolve_exclusions(
        document
            .exclusions
            .into_iter()
            .map(|exclusion| (exclusion.root_id, exclusion.path)),
        &roots,
    )?;
    let recursive_exclusions = parse_recursive_exclusions(&document.recursive_exclusions)?;
    Ok(TrackingPolicy {
        roots,
        exclusions,
        recursive_exclusions,
        limits: document.limits.into(),
    })
}

pub fn parse_roots(values: &[String]) -> Result<Vec<Root>, ConfigError> {
    let parsed = values
        .iter()
        .map(|value| {
            let (id, path) = value
                .split_once('=')
                .ok_or_else(|| ConfigError::InvalidRoot(value.clone()))?;
            Ok((id.to_owned(), PathBuf::from(path)))
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;
    resolve_roots(parsed)
}

fn resolve_roots(
    values: impl IntoIterator<Item = (String, PathBuf)>,
) -> Result<Vec<Root>, ConfigError> {
    let mut roots = Vec::new();
    let mut ids = HashSet::new();
    for (id, raw_path) in values {
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(ConfigError::InvalidRootId(id));
        }
        if !ids.insert(id.clone()) {
            return Err(ConfigError::DuplicateRootId(id));
        }
        let path = fs::canonicalize(&raw_path).map_err(|source| ConfigError::RootIo {
            path: raw_path,
            source,
        })?;
        roots.push(Root { id, path });
    }
    if roots.is_empty() {
        return Err(ConfigError::MissingRoots);
    }
    roots.sort_by(|a, b| a.path.cmp(&b.path));
    for pair in roots.windows(2) {
        if pair[1].path.starts_with(&pair[0].path) {
            return Err(ConfigError::OverlappingRoots(
                pair[0].path.clone(),
                pair[1].path.clone(),
            ));
        }
    }
    Ok(roots)
}

pub fn parse_exclusions(values: &[String], roots: &[Root]) -> Result<Vec<Exclusion>, ConfigError> {
    let parsed = values
        .iter()
        .map(|value| {
            let (root_id, path) = value
                .split_once('=')
                .ok_or_else(|| ConfigError::InvalidExclusion(value.clone()))?;
            Ok((root_id.to_owned(), path.to_owned()))
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;
    resolve_exclusions(parsed, roots)
}

fn resolve_exclusions(
    values: impl IntoIterator<Item = (String, String)>,
    roots: &[Root],
) -> Result<Vec<Exclusion>, ConfigError> {
    let root_ids: HashSet<&str> = roots.iter().map(|root| root.id.as_str()).collect();
    let mut exclusions = Vec::new();
    for (root_id, raw_path) in values {
        if !root_ids.contains(root_id.as_str()) {
            return Err(ConfigError::UnknownExclusionRoot(root_id));
        }
        let path = Path::new(&raw_path);
        if path.is_absolute() || raw_path.is_empty() {
            return Err(ConfigError::InvalidExclusionPath(raw_path));
        }
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => relative.push(part),
                _ => return Err(ConfigError::InvalidExclusionPath(raw_path)),
            }
        }
        if relative.as_os_str().is_empty() {
            return Err(ConfigError::InvalidExclusionPath(raw_path));
        }
        exclusions.push(Exclusion { root_id, relative });
    }
    exclusions.sort_by(|left, right| {
        left.root_id
            .cmp(&right.root_id)
            .then(left.relative.cmp(&right.relative))
    });
    let mut reduced: Vec<Exclusion> = Vec::new();
    for exclusion in exclusions {
        if reduced.iter().any(|existing| {
            existing.root_id == exclusion.root_id
                && exclusion.relative.starts_with(&existing.relative)
        }) {
            continue;
        }
        reduced.push(exclusion);
    }
    Ok(reduced)
}

pub fn parse_recursive_exclusions(values: &[String]) -> Result<Vec<String>, ConfigError> {
    let mut exclusions = Vec::with_capacity(values.len());
    for value in values {
        let mut components = Path::new(value).components();
        if value.is_empty()
            || value.contains(['\0', '\n'])
            || !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(ConfigError::InvalidRecursiveExclusion(value.clone()));
        }
        exclusions.push(value.clone());
    }
    exclusions.sort();
    exclusions.dedup();
    Ok(exclusions)
}

pub fn validate_output(output: &Path, roots: &[Root]) -> Result<(), ConfigError> {
    if output.exists() {
        return Err(ConfigError::OutputExists(output.to_path_buf()));
    }
    let absolute = if output.is_absolute() {
        output.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(output)
    };
    let parent = absolute.parent().unwrap_or(Path::new("/"));
    let canonical_parent = fs::canonicalize(parent).map_err(|source| ConfigError::RootIo {
        path: parent.to_path_buf(),
        source,
    })?;
    let normalized = canonical_parent.join(absolute.file_name().unwrap_or_default());
    if roots.iter().any(|root| normalized.starts_with(&root.path)) {
        return Err(ConfigError::OutputInsideRoot(normalized));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_are_bounded() {
        let limits = Limits::default();
        assert!(limits.max_files > 0);
        assert!(limits.max_total_bytes >= limits.max_file_bytes);
    }

    #[test]
    fn rejects_bad_root_id_before_io() {
        let error = parse_roots(&["bad/id=/tmp".into()]).unwrap_err();
        assert!(matches!(error, ConfigError::InvalidRootId(_)));
    }

    #[test]
    fn rejects_empty_root_set() {
        assert!(matches!(parse_roots(&[]), Err(ConfigError::MissingRoots)));
    }

    #[test]
    fn validates_and_reduces_exclusions() {
        let roots = vec![Root {
            id: "main".into(),
            path: PathBuf::from("/workspace"),
        }];
        let exclusions = parse_exclusions(
            &[
                "main=node_modules/pkg".into(),
                "main=node_modules".into(),
                "main=.venv".into(),
                "main=.venv".into(),
            ],
            &roots,
        )
        .unwrap();
        assert_eq!(
            exclusions,
            vec![
                Exclusion {
                    root_id: "main".into(),
                    relative: PathBuf::from(".venv"),
                },
                Exclusion {
                    root_id: "main".into(),
                    relative: PathBuf::from("node_modules"),
                },
            ]
        );
        assert!(matches!(
            parse_exclusions(&["other=.venv".into()], &roots),
            Err(ConfigError::UnknownExclusionRoot(_))
        ));
        assert!(matches!(
            parse_exclusions(&["main=../outside".into()], &roots),
            Err(ConfigError::InvalidExclusionPath(_))
        ));
    }

    #[test]
    fn validates_recursive_component_exclusions() {
        assert_eq!(
            parse_recursive_exclusions(&[
                "node_modules".into(),
                ".git".into(),
                "node_modules".into(),
            ])
            .unwrap(),
            vec![".git", "node_modules"]
        );
        assert!(matches!(
            parse_recursive_exclusions(&["nested/cache".into()]),
            Err(ConfigError::InvalidRecursiveExclusion(_))
        ));
    }

    #[test]
    fn loads_versioned_tracking_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let policy_path = temporary.path().join("policy.json");
        let document = serde_json::json!({
            "schemaVersion": 1,
            "roots": [
                {"id": "main", "path": first},
                {"id": "other", "path": second},
            ],
            "exclusions": [{"rootId": "main", "path": "generated"}],
            "recursiveExclusions": [".git", "node_modules"],
            "limits": {"maxFiles": 42},
        });
        fs::write(&policy_path, serde_json::to_vec(&document).unwrap()).unwrap();

        let policy = load_tracking_policy(&policy_path).unwrap();

        assert_eq!(policy.roots.len(), 2);
        assert_eq!(policy.exclusions.len(), 1);
        assert_eq!(policy.recursive_exclusions, vec![".git", "node_modules"]);
        assert_eq!(policy.limits.max_files, 42);
        assert_eq!(
            policy.limits.max_total_bytes,
            Limits::default().max_total_bytes
        );
    }

    #[test]
    fn rejects_unknown_policy_fields_and_versions() {
        let temporary = tempfile::tempdir().unwrap();
        let policy_path = temporary.path().join("policy.json");
        fs::write(
            &policy_path,
            br#"{"schemaVersion":1,"roots":[],"unexpected":true}"#,
        )
        .unwrap();
        assert!(matches!(
            load_tracking_policy(&policy_path),
            Err(ConfigError::PolicyJson { .. })
        ));

        fs::write(&policy_path, br#"{"schemaVersion":2,"roots":[]}"#).unwrap();
        assert!(matches!(
            load_tracking_policy(&policy_path),
            Err(ConfigError::UnsupportedPolicyVersion(2))
        ));
    }

    #[test]
    fn rejects_oversized_tracking_policy_before_reading_it() {
        let temporary = tempfile::tempdir().unwrap();
        let policy_path = temporary.path().join("policy.json");
        let file = fs::File::create(&policy_path).unwrap();
        file.set_len(MAX_TRACKING_POLICY_BYTES as u64 + 1).unwrap();

        assert!(matches!(
            load_tracking_policy(&policy_path),
            Err(ConfigError::PolicyTooLarge { .. })
        ));
    }
}
