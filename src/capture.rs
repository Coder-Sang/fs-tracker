use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Exclusion, Limits, Root};
use crate::contracts::{FileState, Gap, ObjectRef};

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathKey {
    pub root_id: String,
    pub relative: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub before: FileState,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CaptureOutcome {
    pub state: FileState,
    pub captured_bytes: u64,
    pub gaps: Vec<Gap>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CaptureRequest {
    output_base64: String,
    path_base64: String,
    max_file_bytes: u64,
    max_total_bytes: u64,
    delay_millis: u64,
    journal: Option<PathKey>,
}

struct HelperProcess {
    pid: libc::pid_t,
    stdin: File,
    stdout: OwnedFd,
}

struct CaptureHelper {
    process: Option<HelperProcess>,
}

pub struct CaptureStore {
    output: PathBuf,
    roots: Vec<Root>,
    exclusions: Vec<Exclusion>,
    limits: Limits,
    capture_timeout: Duration,
    capture_helper_delay: Duration,
    helper: CaptureHelper,
    total_bytes: u64,
    pub candidates: HashMap<PathKey, Candidate>,
    pub gaps: Vec<Gap>,
    seen_gaps: HashSet<Gap>,
}

impl CaptureStore {
    pub fn new(
        output: PathBuf,
        roots: Vec<Root>,
        exclusions: Vec<Exclusion>,
        limits: Limits,
        capture_timeout: Duration,
        capture_helper_delay: Duration,
    ) -> io::Result<Self> {
        let mut output_builder = fs::DirBuilder::new();
        output_builder.mode(0o700).create(&output)?;
        fs::create_dir(output.join("objects"))?;
        for name in ["journal.jsonl", "diagnostics.jsonl"] {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(output.join(name))?;
        }
        Ok(Self {
            output,
            roots,
            exclusions,
            limits,
            capture_timeout,
            capture_helper_delay,
            helper: CaptureHelper::new(),
            total_bytes: 0,
            candidates: HashMap::new(),
            gaps: Vec::new(),
            seen_gaps: HashSet::new(),
        })
    }

    pub fn output(&self) -> &Path {
        &self.output
    }

    pub fn max_diff_bytes(&self) -> usize {
        self.limits.max_diff_bytes
    }

    pub fn captured_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn gap(&mut self, kind: impl Into<String>, detail: impl Into<String>) {
        let gap = Gap {
            kind: kind.into(),
            detail: detail.into(),
        };
        if self.seen_gaps.insert(gap.clone()) {
            self.gaps.push(gap);
        }
    }

    pub fn locate(&self, path: &Path) -> Option<(PathKey, PathBuf)> {
        let normalized = normalize(path);
        self.roots.iter().find_map(|root| {
            normalized
                .strip_prefix(&root.path)
                .ok()
                .and_then(|relative| {
                    if relative.as_os_str().is_empty() {
                        return None;
                    }
                    if self.exclusions.iter().any(|exclusion| {
                        exclusion.root_id == root.id && relative.starts_with(&exclusion.relative)
                    }) {
                        return None;
                    }
                    Some((
                        PathKey {
                            root_id: root.id.clone(),
                            relative: relative.as_os_str().as_bytes().to_vec(),
                        },
                        normalized.clone(),
                    ))
                })
        })
    }

    pub fn capture_before(&mut self, path: &Path) {
        let Some((key, normalized)) = self.locate(path) else {
            return;
        };
        if self.candidates.contains_key(&key) {
            return;
        }
        if self.candidates.len() >= self.limits.max_files {
            self.gap(
                "file_quota",
                format!("candidate limit {} exceeded", self.limits.max_files),
            );
            return;
        }
        let state = self.capture_state(&normalized, Some(&key));
        self.candidates.insert(
            key,
            Candidate {
                path: normalized,
                before: state,
            },
        );
    }

    pub fn capture_after(&mut self, path: &Path) -> FileState {
        self.capture_state(path, None)
    }

    fn capture_state(&mut self, path: &Path, journal: Option<&PathKey>) -> FileState {
        let remaining = self.limits.max_total_bytes.saturating_sub(self.total_bytes);
        let request = CaptureRequest::new(
            &self.output,
            path,
            self.limits.max_file_bytes,
            remaining,
            self.capture_helper_delay,
            journal.cloned(),
        );
        match self.helper.capture(&request, self.capture_timeout) {
            Ok(outcome) => {
                self.total_bytes = self.total_bytes.saturating_add(outcome.captured_bytes);
                for gap in outcome.gaps {
                    self.gap(gap.kind, gap.detail);
                }
                outcome.state
            }
            Err(error) => {
                let kind = if error.kind() == io::ErrorKind::TimedOut {
                    "capture_timeout"
                } else {
                    "capture_helper"
                };
                self.gap(kind, format!("{}: {error}", path.display()));
                FileState::Unavailable {
                    reason: kind.into(),
                }
            }
        }
    }

    pub fn exclusions(&self) -> &[Exclusion] {
        &self.exclusions
    }

    pub fn helper_pid(&self) -> Option<libc::pid_t> {
        self.helper.process.as_ref().map(|process| process.pid)
    }

    pub fn read_object(&self, state: &FileState) -> io::Result<Option<Vec<u8>>> {
        let FileState::Present { object } = state else {
            return Ok(None);
        };
        let digest = object.object_id.strip_prefix("sha256:").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid object identifier")
        })?;
        fs::read(self.output.join("objects").join(digest)).map(Some)
    }

    pub fn path_base64(key: &PathKey) -> String {
        base64::engine::general_purpose::STANDARD.encode(&key.relative)
    }
}

pub fn capture_one(
    output: &Path,
    path: &Path,
    max_file_bytes: u64,
    max_total_bytes: u64,
) -> CaptureOutcome {
    let mut gaps = Vec::new();
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return CaptureOutcome {
                state: FileState::Absent,
                captured_bytes: 0,
                gaps,
            };
        }
        Err(error) => {
            return unavailable(error.to_string(), gaps);
        }
    };
    let mode = format!("{:06o}", metadata.mode() & 0o177777);
    let bytes = if metadata.file_type().is_symlink() {
        match fs::read_link(path) {
            Ok(target) => target.into_os_string().into_vec(),
            Err(error) => return unavailable(error.to_string(), gaps),
        }
    } else if metadata.is_file() {
        if metadata.len() > max_file_bytes {
            gaps.push(Gap {
                kind: "file_size_quota".into(),
                detail: format!("{} is {} bytes", path.display(), metadata.len()),
            });
            return unavailable("file_size_quota".into(), gaps);
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) => return unavailable(error.to_string(), gaps),
        };
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if let Err(error) = file.read_to_end(&mut bytes) {
            return unavailable(error.to_string(), gaps);
        }
        bytes
    } else if metadata.is_dir() {
        return unavailable("directory_not_reported".into(), gaps);
    } else {
        gaps.push(Gap {
            kind: "special_file".into(),
            detail: path.display().to_string(),
        });
        return unavailable("special_file_not_captured".into(), gaps);
    };
    if bytes.len() as u64 > max_total_bytes {
        gaps.push(Gap {
            kind: "total_byte_quota".into(),
            detail: "capture byte limit exceeded".into(),
        });
        return unavailable("total_byte_quota".into(), gaps);
    }
    match put_object(output, &bytes) {
        Ok(object_id) => CaptureOutcome {
            state: FileState::Present {
                object: ObjectRef {
                    object_id,
                    size: bytes.len() as u64,
                    mode,
                },
            },
            captured_bytes: bytes.len() as u64,
            gaps,
        },
        Err(error) => {
            gaps.push(Gap {
                kind: "object_write".into(),
                detail: format!("{}: {error}", path.display()),
            });
            unavailable(error.to_string(), gaps)
        }
    }
}

fn unavailable(reason: String, gaps: Vec<Gap>) -> CaptureOutcome {
    CaptureOutcome {
        state: FileState::Unavailable { reason },
        captured_bytes: 0,
        gaps,
    }
}

fn write_journal(output: &Path, key: &PathKey, state: &FileState) -> io::Result<()> {
    let entry = serde_json::json!({
        "event": "before_captured",
        "root_id": key.root_id,
        "path_bytes_base64": CaptureStore::path_base64(key),
        "before": state,
    });
    let mut file = OpenOptions::new()
        .append(true)
        .open(output.join("journal.jsonl"))?;
    serde_json::to_writer(&mut file, &entry).map_err(io::Error::other)?;
    file.write_all(b"\n")
}

fn put_object(output: &Path, bytes: &[u8]) -> io::Result<String> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let final_path = output.join("objects").join(&digest);
    if !final_path.exists() {
        let temporary = output
            .join("objects")
            .join(format!(".{digest}.{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(&temporary)?;
        let result = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&temporary, &final_path));
        if let Err(error) = result {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    Ok(format!("sha256:{digest}"))
}

impl CaptureRequest {
    fn new(
        output: &Path,
        path: &Path,
        max_file_bytes: u64,
        max_total_bytes: u64,
        delay: Duration,
        journal: Option<PathKey>,
    ) -> Self {
        let encode = |value: &Path| {
            base64::engine::general_purpose::STANDARD.encode(value.as_os_str().as_bytes())
        };
        Self {
            output_base64: encode(output),
            path_base64: encode(path),
            max_file_bytes,
            max_total_bytes,
            delay_millis: delay.as_millis().min(u128::from(u64::MAX)) as u64,
            journal,
        }
    }

    fn execute(&self) -> io::Result<CaptureOutcome> {
        let decode = |value: &str| -> io::Result<PathBuf> {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value)
                .map_err(io::Error::other)?;
            Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
        };
        if self.delay_millis > 0 {
            std::thread::sleep(Duration::from_millis(self.delay_millis));
        }
        let output = decode(&self.output_base64)?;
        let mut outcome = capture_one(
            &output,
            &decode(&self.path_base64)?,
            self.max_file_bytes,
            self.max_total_bytes,
        );
        if let Some(key) = &self.journal {
            if let Err(error) = write_journal(&output, key, &outcome.state) {
                outcome.gaps.push(Gap {
                    kind: "journal_write".into(),
                    detail: error.to_string(),
                });
            }
        }
        Ok(outcome)
    }
}

pub fn capture_helper_loop() -> io::Result<()> {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    while input.read_line(&mut line)? != 0 {
        let request: CaptureRequest = serde_json::from_str(&line).map_err(io::Error::other)?;
        let outcome = request.execute()?;
        serde_json::to_writer(&mut output, &outcome).map_err(io::Error::other)?;
        output.write_all(b"\n")?;
        output.flush()?;
        line.clear();
    }
    Ok(())
}

impl CaptureHelper {
    fn new() -> Self {
        Self { process: None }
    }

    fn capture(
        &mut self,
        request: &CaptureRequest,
        timeout: Duration,
    ) -> io::Result<CaptureOutcome> {
        if self.process.is_none() {
            self.process = Some(spawn_helper()?);
        }
        let result = capture_with_process(
            self.process.as_mut().expect("helper initialized above"),
            request,
            timeout,
        );
        if result.is_err() {
            self.terminate();
        }
        result
    }

    fn terminate(&mut self) {
        if let Some(process) = self.process.take() {
            kill_and_reap(process.pid);
        }
    }
}

impl Drop for CaptureHelper {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn spawn_helper() -> io::Result<HelperProcess> {
    let arguments = ["/proc/self/exe", "capture-helper"];
    let arguments: Vec<CString> = arguments
        .iter()
        .map(|argument| CString::new(*argument).expect("literal has no NUL"))
        .collect();
    let argv: Vec<*mut libc::c_char> = arguments
        .iter()
        .map(|argument| argument.as_ptr().cast_mut())
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect();
    let (stdin_read, stdin_write) = pipe_cloexec()?;
    let (stdout_read, stdout_write) = pipe_cloexec()?;
    let dev_null = OpenOptions::new().write(true).open("/dev/null")?;
    let mut actions = std::mem::MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
    // SAFETY: actions points to writable storage for the libc initialization routine.
    let init_result = unsafe { libc::posix_spawn_file_actions_init(actions.as_mut_ptr()) };
    if init_result != 0 {
        return Err(io::Error::from_raw_os_error(init_result));
    }
    // SAFETY: initialization above succeeded and every descriptor is valid here.
    let mut actions = unsafe { actions.assume_init() };
    let mappings = [
        (stdin_read.as_raw_fd(), libc::STDIN_FILENO),
        (stdout_write.as_raw_fd(), libc::STDOUT_FILENO),
        (dev_null.as_raw_fd(), libc::STDERR_FILENO),
    ];
    let mut configure_result = 0;
    for (source, destination) in mappings {
        if configure_result == 0 {
            // SAFETY: actions is initialized and source remains open through posix_spawn.
            configure_result = unsafe {
                libc::posix_spawn_file_actions_adddup2(&mut actions, source, destination)
            };
        }
    }
    if configure_result == 0 {
        // SAFETY: mapped standard streams are below 3; all other inherited FDs are private.
        configure_result =
            unsafe { libc::posix_spawn_file_actions_addclosefrom_np(&mut actions, 3) };
    }
    if configure_result != 0 {
        // SAFETY: actions remains initialized until destroyed once.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) };
        return Err(io::Error::from_raw_os_error(configure_result));
    }
    let mut pid = 0;
    unsafe extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }
    // SAFETY: C strings and argv remain alive for the duration of posix_spawn.
    let spawn_result = unsafe {
        libc::posix_spawn(
            &mut pid,
            arguments[0].as_ptr(),
            &actions,
            std::ptr::null(),
            argv.as_ptr(),
            environ,
        )
    };
    // SAFETY: actions was initialized and is no longer needed after posix_spawn returns.
    unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) };
    if spawn_result != 0 {
        return Err(io::Error::from_raw_os_error(spawn_result));
    }
    drop(stdin_read);
    drop(stdout_write);
    drop(dev_null);
    if let Err(error) = set_nonblocking(stdout_read.as_raw_fd()) {
        kill_and_reap(pid);
        return Err(error);
    }
    Ok(HelperProcess {
        pid,
        stdin: File::from(stdin_write),
        stdout: stdout_read,
    })
}

fn capture_with_process(
    process: &mut HelperProcess,
    request: &CaptureRequest,
    timeout: Duration,
) -> io::Result<CaptureOutcome> {
    serde_json::to_writer(&mut process.stdin, request).map_err(io::Error::other)?;
    process.stdin.write_all(b"\n")?;
    process.stdin.flush()?;
    let started = Instant::now();
    let mut response = Vec::new();
    loop {
        if drain_response(process.stdout.as_raw_fd(), &mut response)? {
            return serde_json::from_slice(&response).map_err(io::Error::other);
        }
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "capture helper timed out",
            ));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let timeout_ms = remaining.as_millis().clamp(1, 50) as i32;
        let mut pollfd = libc::pollfd {
            fd: process.stdout.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: pollfd points to one initialized descriptor.
        let polled = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

fn drain_response(fd: i32, response: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0u8; 8192];
    loop {
        // SAFETY: buffer is writable and fd belongs to the helper stdout pipe.
        let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count > 0 {
            response.extend_from_slice(&buffer[..count as usize]);
            if response.len() > 1024 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "capture helper response too large",
                ));
            }
            if response.last() == Some(&b'\n') {
                response.pop();
                return Ok(true);
            }
            continue;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "capture helper closed stdout",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL only inspect and update flags for the supplied descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn kill_and_reap(pid: libc::pid_t) {
    // SAFETY: pid is a live or recently exited helper child.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        while libc::waitpid(pid, std::ptr::null_mut(), 0) < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
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

pub fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => result.push(prefix.as_os_str()),
            Component::RootDir => result.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::Normal(part) => result.push(part),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_capture_output_with_private_mode() {
        let temporary = tempfile::tempdir().unwrap();
        let output = temporary.path().join("capture");
        let store = CaptureStore::new(
            output.clone(),
            Vec::new(),
            Vec::new(),
            Limits::default(),
            Duration::from_secs(1),
            Duration::ZERO,
        )
        .unwrap();

        assert_eq!(fs::metadata(store.output()).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn normalizes_lexically() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }
}
