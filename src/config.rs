use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

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

pub fn parse_roots(values: &[String]) -> Result<Vec<Root>, ConfigError> {
    let mut roots = Vec::new();
    let mut ids = HashSet::new();
    for value in values {
        let (id, path) = value
            .split_once('=')
            .ok_or_else(|| ConfigError::InvalidRoot(value.clone()))?;
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(ConfigError::InvalidRootId(id.into()));
        }
        if !ids.insert(id.to_owned()) {
            return Err(ConfigError::DuplicateRootId(id.into()));
        }
        let path = fs::canonicalize(path).map_err(|source| ConfigError::RootIo {
            path: PathBuf::from(path),
            source,
        })?;
        roots.push(Root {
            id: id.into(),
            path,
        });
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
    let root_ids: HashSet<&str> = roots.iter().map(|root| root.id.as_str()).collect();
    let mut exclusions = Vec::new();
    for value in values {
        let (root_id, raw_path) = value
            .split_once('=')
            .ok_or_else(|| ConfigError::InvalidExclusion(value.clone()))?;
        if !root_ids.contains(root_id) {
            return Err(ConfigError::UnknownExclusionRoot(root_id.into()));
        }
        let path = Path::new(raw_path);
        if path.is_absolute() || raw_path.is_empty() {
            return Err(ConfigError::InvalidExclusionPath(raw_path.into()));
        }
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => relative.push(part),
                _ => return Err(ConfigError::InvalidExclusionPath(raw_path.into())),
            }
        }
        if relative.as_os_str().is_empty() {
            return Err(ConfigError::InvalidExclusionPath(raw_path.into()));
        }
        exclusions.push(Exclusion {
            root_id: root_id.into(),
            relative,
        });
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
}
