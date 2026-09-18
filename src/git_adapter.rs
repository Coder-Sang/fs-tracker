use std::collections::HashMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::contracts::{FileState, Report, RunState};

#[derive(Debug)]
pub struct GitReceiptConfig {
    pub tracker_output: PathBuf,
    pub repository: PathBuf,
    pub receipt: PathBuf,
    pub run_id: String,
    pub workspace_id: String,
    pub report_ref: String,
    pub projects: HashMap<String, PathBuf>,
    pub allow_partial: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitReceiptV2 {
    pub schema_version: u8,
    pub run_id: String,
    pub workspace_id: String,
    pub baseline_commit_sha: String,
    pub report_commit_sha: String,
}

#[derive(Debug, Error)]
pub enum GitAdapterError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid tracker report: {0}")]
    Report(#[from] serde_json::Error),
    #[error("tracker report state is {0:?}; use --allow-partial to override")]
    Incomplete(RunState),
    #[error("missing --project mapping for root ID {0}")]
    MissingProject(String),
    #[error("invalid workspace ID; expected 64 lowercase hexadecimal characters")]
    InvalidWorkspaceId,
    #[error("run ID cannot be blank")]
    InvalidRunId,
    #[error("git command failed: {command}: {stderr}")]
    Git { command: String, stderr: String },
    #[error("git returned non-ASCII object ID")]
    InvalidObjectId,
}

pub fn create_receipt(config: GitReceiptConfig) -> Result<GitReceiptV2, GitAdapterError> {
    validate_identity(&config)?;
    let report: Report =
        serde_json::from_slice(&fs::read(config.tracker_output.join("report.json"))?)?;
    if report.state != RunState::Finished && !config.allow_partial {
        return Err(GitAdapterError::Incomplete(report.state));
    }
    if let Some(parent) = config.repository.parent() {
        fs::create_dir_all(parent)?;
    }
    if !config.repository.exists() {
        git(
            None,
            [
                OsStr::new("init"),
                OsStr::new("--bare"),
                config.repository.as_os_str(),
            ],
            None,
        )?;
    }
    git(
        Some(&config.repository),
        [
            OsStr::new("check-ref-format"),
            OsStr::new(&config.report_ref),
        ],
        None,
    )?;

    let baseline_index = config.tracker_output.join(".git-baseline-index");
    let report_index = config.tracker_output.join(".git-report-index");
    let baseline_tree = build_tree(&config, &report, &baseline_index, true)?;
    let report_tree = build_tree(&config, &report, &report_index, false)?;
    let _ = fs::remove_file(&baseline_index);
    let _ = fs::remove_file(&report_index);

    let baseline_commit = commit_tree(
        &config.repository,
        &baseline_tree,
        None,
        "fs-tracker baseline\n",
    )?;
    let report_message = format!(
        "fs-tracker report\n\nFs-Tracker-Run-Id: {}\n",
        config.run_id
    );
    let report_commit = commit_tree(
        &config.repository,
        &report_tree,
        Some(&baseline_commit),
        &report_message,
    )?;
    git(
        Some(&config.repository),
        [
            OsStr::new("update-ref"),
            OsStr::new(&config.report_ref),
            OsStr::new(&report_commit),
        ],
        None,
    )?;
    let receipt = GitReceiptV2 {
        schema_version: 2,
        run_id: config.run_id,
        workspace_id: config.workspace_id,
        baseline_commit_sha: baseline_commit,
        report_commit_sha: report_commit,
    };
    atomic_json(&config.receipt, &receipt)?;
    Ok(receipt)
}

fn validate_identity(config: &GitReceiptConfig) -> Result<(), GitAdapterError> {
    if config.run_id.trim().is_empty() {
        return Err(GitAdapterError::InvalidRunId);
    }
    if config.workspace_id.len() != 64
        || !config
            .workspace_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(GitAdapterError::InvalidWorkspaceId);
    }
    Ok(())
}

fn build_tree(
    config: &GitReceiptConfig,
    report: &Report,
    index: &Path,
    baseline: bool,
) -> Result<String, GitAdapterError> {
    let _ = fs::remove_file(index);
    git_with_index(
        &config.repository,
        index,
        [OsStr::new("read-tree"), OsStr::new("--empty")],
        None,
    )?;
    for change in &report.changes {
        let project = config
            .projects
            .get(&change.root_id)
            .ok_or_else(|| GitAdapterError::MissingProject(change.root_id.clone()))?;
        let project_key = format!(
            "{:x}",
            Sha256::digest(project.as_os_str().as_encoded_bytes())
        );
        let relative = base64::engine::general_purpose::STANDARD
            .decode(&change.path_bytes_base64)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let state = if baseline {
            &change.before
        } else {
            &change.after
        };
        let FileState::Present { object } = state else {
            continue;
        };
        let digest = object.object_id.strip_prefix("sha256:").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid tracker object ID")
        })?;
        let bytes = fs::read(config.tracker_output.join("objects").join(digest))?;
        let blob = git(
            Some(&config.repository),
            [
                OsStr::new("hash-object"),
                OsStr::new("-w"),
                OsStr::new("--stdin"),
            ],
            Some(&bytes),
        )?;
        let mut path = project_key.into_bytes();
        path.push(b'/');
        path.extend_from_slice(&relative);
        let path = OsString::from_vec(path);
        let mode = git_mode(&object.mode);
        git_with_index(
            &config.repository,
            index,
            [
                OsStr::new("update-index"),
                OsStr::new("--add"),
                OsStr::new("--cacheinfo"),
                OsStr::new(mode),
                OsStr::new(&blob),
                path.as_os_str(),
            ],
            None,
        )?;
    }
    git_with_index(&config.repository, index, [OsStr::new("write-tree")], None)
}

fn git_mode(mode: &str) -> &'static str {
    if mode.starts_with("120") {
        "120000"
    } else if mode
        .as_bytes()
        .last()
        .is_some_and(|byte| matches!(byte, b'1' | b'3' | b'5' | b'7'))
    {
        "100755"
    } else {
        "100644"
    }
}

fn commit_tree(
    repository: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> Result<String, GitAdapterError> {
    let mut args = vec![OsString::from("commit-tree"), OsString::from(tree)];
    if let Some(parent) = parent {
        args.extend([OsString::from("-p"), OsString::from(parent)]);
    }
    let refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    git(Some(repository), refs, Some(message.as_bytes()))
}

fn git_with_index<I, S>(
    repository: &Path,
    index: &Path,
    args: I,
    input: Option<&[u8]>,
) -> Result<String, GitAdapterError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_command(Some(repository), args, input, Some(index))
}

fn git<I, S>(
    repository: Option<&Path>,
    args: I,
    input: Option<&[u8]>,
) -> Result<String, GitAdapterError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_command(repository, args, input, None)
}

fn git_command<I, S>(
    repository: Option<&Path>,
    args: I,
    input: Option<&[u8]>,
    index: Option<&Path>,
) -> Result<String, GitAdapterError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let mut argv = vec![CString::new("git").expect("literal has no NUL")];
    if let Some(repository) = repository {
        let mut argument = b"--git-dir=".to_vec();
        argument.extend_from_slice(repository.as_os_str().as_bytes());
        argv.push(CString::new(argument).map_err(invalid_nul)?);
    }
    for argument in &args {
        argv.push(CString::new(argument.as_bytes()).map_err(invalid_nul)?);
    }
    let pointers: Vec<*const libc::c_char> = argv
        .iter()
        .map(|argument| argument.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let (stdin_read, stdin_write) = pipe_cloexec()?;
    let (stdout_read, stdout_write) = pipe_cloexec()?;
    let (stderr_read, stderr_write) = pipe_cloexec()?;
    // SAFETY: the adapter CLI is single-threaded at this boundary.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        // SAFETY: child setup only uses scalar syscalls and prebuilt C strings before exec.
        unsafe {
            libc::dup2(stdin_read.as_raw_fd(), libc::STDIN_FILENO);
            libc::dup2(stdout_write.as_raw_fd(), libc::STDOUT_FILENO);
            libc::dup2(stderr_write.as_raw_fd(), libc::STDERR_FILENO);
            for fd in [
                stdin_read.as_raw_fd(),
                stdin_write.as_raw_fd(),
                stdout_read.as_raw_fd(),
                stdout_write.as_raw_fd(),
                stderr_read.as_raw_fd(),
                stderr_write.as_raw_fd(),
            ] {
                if fd > libc::STDERR_FILENO {
                    libc::close(fd);
                }
            }
            for (name, value) in [
                ("GIT_AUTHOR_NAME", "fs-tracker"),
                ("GIT_AUTHOR_EMAIL", "fs-tracker@localhost"),
                ("GIT_COMMITTER_NAME", "fs-tracker"),
                ("GIT_COMMITTER_EMAIL", "fs-tracker@localhost"),
                ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
                ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
            ] {
                let name = CString::new(name).expect("literal has no NUL");
                let value = CString::new(value).expect("literal has no NUL");
                libc::setenv(name.as_ptr(), value.as_ptr(), 1);
            }
            if let Some(index) = index {
                let name = CString::new("GIT_INDEX_FILE").expect("literal has no NUL");
                let value =
                    CString::new(index.as_os_str().as_bytes()).unwrap_or_else(|_| libc::_exit(127));
                libc::setenv(name.as_ptr(), value.as_ptr(), 1);
            }
            libc::execvp(argv[0].as_ptr(), pointers.as_ptr());
            libc::_exit(127);
        }
    }
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_write);
    let (status, stdout, stderr) = pump_child_io(
        pid,
        input,
        File::from(stdin_write),
        File::from(stdout_read),
        File::from(stderr_read),
    )?;
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(GitAdapterError::Git {
            command: format!(
                "git {}",
                args.iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        });
    }
    String::from_utf8(stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| GitAdapterError::InvalidObjectId)
}

fn pump_child_io(
    pid: libc::pid_t,
    input: Option<&[u8]>,
    mut child_stdin: File,
    child_stdout: File,
    child_stderr: File,
) -> io::Result<(i32, Vec<u8>, Vec<u8>)> {
    std::thread::scope(|scope| {
        let stdin_task = scope.spawn(move || -> io::Result<()> {
            if let Some(input) = input {
                if let Err(error) = child_stdin.write_all(input) {
                    if error.kind() != io::ErrorKind::BrokenPipe {
                        return Err(error);
                    }
                }
            }
            Ok(())
        });
        let stdout_task = scope.spawn(move || drain_bounded(child_stdout, 64 * 1024, false));
        let stderr_task = scope.spawn(move || drain_bounded(child_stderr, 1024 * 1024, true));
        let status = waitpid_retry(pid);
        let stdin_result = stdin_task
            .join()
            .map_err(|_| io::Error::other("git stdin pump panicked"))?;
        let stdout_result = stdout_task
            .join()
            .map_err(|_| io::Error::other("git stdout pump panicked"))?;
        let stderr_result = stderr_task
            .join()
            .map_err(|_| io::Error::other("git stderr pump panicked"))?;
        stdin_result?;
        let (stdout, stdout_truncated) = stdout_result?;
        let (stderr, _) = stderr_result?;
        if stdout_truncated {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "git stdout exceeded 64 KiB",
            ));
        }
        Ok((status?, stdout, stderr))
    })
}

fn drain_bounded(mut file: File, limit: usize, keep_tail: bool) -> io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if keep_tail {
            kept.extend_from_slice(&buffer[..count]);
            if kept.len() > limit {
                let overflow = kept.len() - limit;
                kept.drain(..overflow);
                truncated = true;
            }
        } else if kept.len() < limit {
            let available = limit - kept.len();
            kept.extend_from_slice(&buffer[..count.min(available)]);
            truncated |= count > available;
        } else {
            truncated = true;
        }
    }
    Ok((kept, truncated))
}

fn waitpid_retry(pid: libc::pid_t) -> io::Result<i32> {
    let mut status = 0;
    loop {
        // SAFETY: pid is the child created by git_command and status is writable.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid {
            return Ok(status);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [0; 2];
    // SAFETY: pipe2 initializes both descriptors on success.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 returned two uniquely owned descriptors.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn invalid_nul(error: std::ffi::NulError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "receipt has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_filesystem_modes_to_git_modes() {
        assert_eq!(git_mode("100644"), "100644");
        assert_eq!(git_mode("100755"), "100755");
        assert_eq!(git_mode("120777"), "120000");
    }

    #[test]
    fn bounded_drain_consumes_excess_and_keeps_requested_side() {
        use std::io::{Seek, SeekFrom};

        let mut head_file = tempfile::tempfile().unwrap();
        head_file.write_all(b"abcdefghijkl").unwrap();
        head_file.seek(SeekFrom::Start(0)).unwrap();
        let (head, head_truncated) = drain_bounded(head_file, 5, false).unwrap();
        assert_eq!(head, b"abcde");
        assert!(head_truncated);

        let mut tail_file = tempfile::tempfile().unwrap();
        tail_file.write_all(b"abcdefghijkl").unwrap();
        tail_file.seek(SeekFrom::Start(0)).unwrap();
        let (tail, tail_truncated) = drain_bounded(tail_file, 5, true).unwrap();
        assert_eq!(tail, b"hijkl");
        assert!(tail_truncated);
    }

    #[test]
    fn child_io_pumps_large_stdin_stdout_and_stderr_concurrently() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (stdin_read, stdin_write) = pipe_cloexec().unwrap();
        let (stdout_read, stdout_write) = pipe_cloexec().unwrap();
        let (stderr_read, stderr_write) = pipe_cloexec().unwrap();
        // SAFETY: the child branch only invokes async-signal-safe libc operations before _exit.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: descriptors are valid and buffers below are stack-owned.
            unsafe {
                libc::dup2(stdin_read.as_raw_fd(), libc::STDIN_FILENO);
                libc::dup2(stdout_write.as_raw_fd(), libc::STDOUT_FILENO);
                libc::dup2(stderr_write.as_raw_fd(), libc::STDERR_FILENO);
                for fd in [
                    stdin_read.as_raw_fd(),
                    stdin_write.as_raw_fd(),
                    stdout_read.as_raw_fd(),
                    stdout_write.as_raw_fd(),
                    stderr_read.as_raw_fd(),
                    stderr_write.as_raw_fd(),
                ] {
                    if fd > libc::STDERR_FILENO {
                        libc::close(fd);
                    }
                }
                let mut input = [0u8; 8192];
                while libc::read(libc::STDIN_FILENO, input.as_mut_ptr().cast(), input.len()) > 0 {}
                write_repeated(libc::STDOUT_FILENO, b'o', 256 * 1024);
                write_repeated(libc::STDERR_FILENO, b'e', 256 * 1024);
                libc::_exit(0);
            }
        }
        drop(stdin_read);
        drop(stdout_write);
        drop(stderr_write);
        let (done_tx, done_rx) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if done_rx.recv_timeout(Duration::from_secs(3)).is_err() {
                // SAFETY: pid identifies the test child until pump_child_io reaps it.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                true
            } else {
                false
            }
        });
        let input = vec![b'i'; 256 * 1024];
        let result = pump_child_io(
            pid,
            Some(&input),
            File::from(stdin_write),
            File::from(stdout_read),
            File::from(stderr_read),
        );
        let _ = done_tx.send(());
        assert!(
            !watchdog.join().unwrap(),
            "concurrent pipe pumping deadlocked"
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    unsafe fn write_repeated(fd: i32, byte: u8, mut remaining: usize) {
        let buffer = [byte; 8192];
        while remaining > 0 {
            let length = remaining.min(buffer.len());
            let written = libc::write(fd, buffer.as_ptr().cast(), length);
            if written <= 0 {
                libc::_exit(2);
            }
            remaining -= written as usize;
        }
    }
}
