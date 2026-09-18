use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::capture::{normalize, CaptureStore};
use crate::config::RunConfig;
use crate::contracts::{Gap, RunMetrics, RunState, Status, TargetResult};
use crate::linux::{self, Notification, SyscallKind, AT_FDCWD};
use crate::report;

const PATH_LIMIT: usize = 64 * 1024;
const O_TMPFILE_MASK: i32 = 0o20200000;
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_signal(signal: libc::c_int) {
    PENDING_SIGNAL.store(signal, Ordering::Relaxed);
}

#[repr(C)]
#[derive(Default)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

struct WorkerPool {
    sender: mpsc::SyncSender<Notification>,
    handles: Vec<JoinHandle<()>>,
}

enum ControlEvent {
    Finish,
    Disconnected,
    Invalid(String),
}

struct ControlChannel {
    fd: i32,
    buffer: Vec<u8>,
}

impl ControlChannel {
    fn new(fd: i32) -> Self {
        Self {
            fd,
            buffer: Vec::new(),
        }
    }

    fn poll(&mut self) -> io::Result<Option<ControlEvent>> {
        if let Some(event) = self.take_line() {
            return Ok(Some(event));
        }
        let mut descriptor = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and timeout is nonblocking.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error);
        }
        if result == 0 {
            return Ok(None);
        }
        let mut bytes = [0u8; 256];
        // SAFETY: bytes is a writable buffer and fd was supplied by the caller.
        let count = unsafe { libc::read(self.fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) {
                return Ok(None);
            }
            return Err(error);
        }
        if count == 0 {
            if self.buffer == b"finish" {
                self.buffer.clear();
                return Ok(Some(ControlEvent::Finish));
            }
            return Ok(Some(ControlEvent::Disconnected));
        }
        self.buffer.extend_from_slice(&bytes[..count as usize]);
        if self.buffer.len() > 1024 {
            self.buffer.clear();
            return Ok(Some(ControlEvent::Invalid(
                "control message exceeded 1024 bytes".into(),
            )));
        }
        Ok(self.take_line())
    }

    fn take_line(&mut self) -> Option<ControlEvent> {
        let newline = self.buffer.iter().position(|byte| *byte == b'\n')?;
        let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line == b"finish" {
            Some(ControlEvent::Finish)
        } else {
            Some(ControlEvent::Invalid(
                String::from_utf8_lossy(&line).into_owned(),
            ))
        }
    }
}

impl WorkerPool {
    fn new(
        worker_count: usize,
        queue_capacity: usize,
        listener: i32,
        store: Arc<Mutex<CaptureStore>>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Notification>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut handles = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let store = Arc::clone(&store);
            let handle = thread::Builder::new()
                .name(format!("fs-capture-{index}"))
                .spawn(move || loop {
                    let request = {
                        let Ok(receiver) = receiver.lock() else {
                            return;
                        };
                        match receiver.recv() {
                            Ok(request) => request,
                            Err(_) => return,
                        }
                    };
                    let Ok(mut store) = store.lock() else {
                        let _ = linux::continue_notification(listener, request.id);
                        return;
                    };
                    handle_notification(listener, &request, &mut store);
                })
                .expect("capture worker creation failed");
            handles.push(handle);
        }
        Self { sender, handles }
    }

    fn submit(&self, request: Notification) -> Result<(), mpsc::TrySendError<Notification>> {
        self.sender.try_send(request)
    }

    fn shutdown(self) -> bool {
        drop(self.sender);
        self.handles.into_iter().all(|handle| handle.join().is_ok())
    }
}

pub fn run(config: RunConfig) -> io::Result<TargetResult> {
    let started_at = Instant::now();
    let sizes = linux::notification_sizes()?;
    if sizes.0 as usize != std::mem::size_of::<Notification>() {
        return Err(io::Error::new(
            ErrorKind::Unsupported,
            format!("unsupported seccomp notification size {}", sizes.0),
        ));
    }
    // SAFETY: no worker threads exist yet and this only requests child reparenting.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    install_signal_handlers()?;
    let mut store = CaptureStore::new(
        config.output,
        config.roots,
        config.exclusions,
        config.recursive_exclusions,
        config.limits,
        config.capture_timeout,
        config.capture_helper_delay,
    )?;
    let output = store.output().to_path_buf();
    report::publish_status(&output, &Status::new(RunState::Starting, None, None))?;
    capture_writable_stdio(&mut store);
    let child = match linux::spawn(&config.command) {
        Ok(child) => child,
        Err(error) => {
            report::publish_status(
                &output,
                &Status::new(RunState::Failed, Some(error.to_string()), None),
            )?;
            return Err(error);
        }
    };
    report::publish_status(&output, &Status::new(RunState::Running, None, None))?;
    let listener = child.listener.as_raw_fd();
    let store = Arc::new(Mutex::new(store));
    let workers = WorkerPool::new(
        config.capture_workers,
        config.notification_queue,
        listener,
        Arc::clone(&store),
    );
    let mut pending_gaps = Vec::new();
    let mut metrics = RunMetrics::default();
    let mut control = config.finish_fd.map(ControlChannel::new);
    let mut stopping_at = None;
    let mut kill_sent = false;
    let mut main_status = None;
    let mut main_exited_at = None;
    loop {
        let signal = PENDING_SIGNAL.swap(0, Ordering::Relaxed);
        if signal != 0 {
            // SAFETY: the child created a process group whose ID is its PID.
            unsafe { libc::kill(-child.pid, signal) };
            stopping_at.get_or_insert_with(Instant::now);
        }
        if let Some(channel) = control.as_mut() {
            match channel.poll() {
                Ok(Some(ControlEvent::Finish)) => {
                    // SAFETY: the child created a process group whose ID is its PID.
                    unsafe { libc::kill(-child.pid, libc::SIGTERM) };
                    stopping_at.get_or_insert_with(Instant::now);
                    control = None;
                }
                Ok(Some(ControlEvent::Disconnected)) => {
                    pending_gaps.push(Gap {
                        kind: "control_disconnected".into(),
                        detail: "control FD closed before a finish request".into(),
                    });
                    // SAFETY: the child created a process group whose ID is its PID.
                    unsafe { libc::kill(-child.pid, libc::SIGTERM) };
                    stopping_at.get_or_insert_with(Instant::now);
                    control = None;
                }
                Ok(Some(ControlEvent::Invalid(message))) => pending_gaps.push(Gap {
                    kind: "control_protocol".into(),
                    detail: format!("unsupported control message: {message}"),
                }),
                Ok(None) => {}
                Err(error) => {
                    pending_gaps.push(Gap {
                        kind: "control_read".into(),
                        detail: error.to_string(),
                    });
                    // SAFETY: the child created a process group whose ID is its PID.
                    unsafe { libc::kill(-child.pid, libc::SIGTERM) };
                    stopping_at.get_or_insert_with(Instant::now);
                    control = None;
                }
            }
        }
        if !kill_sent
            && stopping_at.is_some_and(|instant| instant.elapsed() >= config.termination_grace)
            && main_status.is_none()
        {
            // SAFETY: the child created a process group whose ID is its PID.
            unsafe { libc::kill(-child.pid, libc::SIGKILL) };
            kill_sent = true;
        }
        if linux::poll_listener(listener, 50)? {
            match linux::receive_notification(listener) {
                Ok(request) => {
                    metrics.notifications_received += 1;
                    if let Err(error) = workers.submit(request) {
                        metrics.queue_bypassed += 1;
                        let request = match error {
                            mpsc::TrySendError::Full(request) => {
                                pending_gaps.push(Gap {
                                    kind: "worker_queue_full".into(),
                                    detail:
                                        "notification bypassed because the capture queue was full"
                                            .into(),
                                });
                                request
                            }
                            mpsc::TrySendError::Disconnected(request) => {
                                pending_gaps.push(Gap {
                                    kind: "worker_pool_stopped".into(),
                                    detail: "capture workers stopped before task completion".into(),
                                });
                                request
                            }
                        };
                        if linux::notification_valid(listener, request.id).is_ok() {
                            let _ = linux::continue_notification(listener, request.id);
                        }
                    }
                }
                Err(error)
                    if matches!(error.raw_os_error(), Some(libc::EINTR) | Some(libc::ENOENT)) => {}
                Err(error) => pending_gaps.push(Gap {
                    kind: "notification_receive".into(),
                    detail: error.to_string(),
                }),
            }
        }
        if main_status.is_none() {
            if let Some(status) = linux::wait_nohang(child.pid)? {
                main_status = Some(status);
                main_exited_at = Some(Instant::now());
            }
        } else {
            let helper_pid = store.lock().ok().and_then(|store| store.helper_pid());
            if !has_live_children(helper_pid)? {
                break;
            }
            if main_exited_at.is_some_and(|instant| instant.elapsed() > Duration::from_secs(5)) {
                pending_gaps.push(Gap {
                    kind: "descendant_timeout".into(),
                    detail: "descendants remained alive 5 seconds after the main process exited"
                        .into(),
                });
                terminate_process_group(child.pid);
                reap_for(Duration::from_secs(2), helper_pid);
                break;
            }
        }
    }
    if !workers.shutdown() {
        pending_gaps.push(Gap {
            kind: "worker_panic".into(),
            detail: "one or more capture workers panicked".into(),
        });
    }
    let mutex = Arc::try_unwrap(store)
        .map_err(|_| io::Error::other("capture store still has active owners"))?;
    let mut store = mutex
        .into_inner()
        .map_err(|_| io::Error::other("capture store mutex was poisoned"))?;
    for gap in pending_gaps {
        store.gap(gap.kind, gap.detail);
    }
    let status = main_status.unwrap_or(128 + libc::SIGKILL);
    let target = linux::target_result(status);
    metrics.elapsed_ms = started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    report::finish(&mut store, target.clone(), metrics)?;
    Ok(target)
}

fn handle_notification(listener: i32, request: &Notification, store: &mut CaptureStore) {
    let outcome = process_request(listener, request, store);
    if let Err(error) = outcome {
        let target_disappeared =
            matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ESRCH));
        if target_disappeared && linux::notification_valid(listener, request.id).is_err() {
            return;
        }
        store.gap(
            "notification_processing",
            format!("pid {} syscall {}: {error}", request.pid, request.data.nr),
        );
    }
    if linux::notification_valid(listener, request.id).is_ok() {
        if let Err(error) = linux::continue_notification(listener, request.id) {
            store.gap("notification_continue", error.to_string());
        }
    }
}

fn process_request(
    listener: i32,
    request: &Notification,
    store: &mut CaptureStore,
) -> io::Result<()> {
    linux::notification_valid(listener, request.id)?;
    let pid = request.pid;
    let args = request.data.args;
    match linux::syscall_kind(request.data.nr) {
        SyscallKind::Open => {
            if writable_flags(args[1] as i32) {
                capture_argument(pid, AT_FDCWD, args[0], true, store)?;
            }
        }
        SyscallKind::OpenAt => {
            if writable_flags(args[2] as i32) {
                capture_argument(pid, args[0] as i32, args[1], true, store)?;
            }
        }
        SyscallKind::OpenAt2 => {
            if args[3] < std::mem::size_of::<OpenHow>() as u64 {
                store.gap(
                    "openat2_size",
                    format!("open_how size {} is too small", args[3]),
                );
                return Ok(());
            }
            let how: OpenHow = read_value(pid, args[2])?;
            if how.resolve != 0 {
                store.gap(
                    "openat2_resolve",
                    format!("unsupported resolve flags {:#x}", how.resolve),
                );
            }
            if writable_flags(how.flags as i32) {
                capture_argument(pid, args[0] as i32, args[1], true, store)?;
            }
        }
        SyscallKind::Truncate | SyscallKind::Chmod => {
            capture_argument(pid, AT_FDCWD, args[0], true, store)?;
        }
        SyscallKind::Ftruncate | SyscallKind::Fchmod => {
            if let Some(path) = fd_path(pid, args[0] as i32)? {
                store.capture_before(&path);
            } else {
                store.gap(
                    "fd_path",
                    format!("cannot resolve pid {pid} fd {}", args[0]),
                );
            }
        }
        SyscallKind::FchmodAt => {
            capture_argument(pid, args[0] as i32, args[1], true, store)?;
        }
        SyscallKind::FchmodAt2 => {
            let follow = args[3] & libc::AT_SYMLINK_NOFOLLOW as u64 == 0;
            capture_argument(pid, args[0] as i32, args[1], follow, store)?;
        }
        SyscallKind::Unlink => {
            capture_argument(pid, AT_FDCWD, args[0], false, store)?;
        }
        SyscallKind::UnlinkAt => {
            capture_argument(pid, args[0] as i32, args[1], false, store)?;
        }
        SyscallKind::Rename => {
            if !capture_argument(pid, AT_FDCWD, args[0], false, store)? {
                capture_argument(pid, AT_FDCWD, args[1], false, store)?;
            }
        }
        SyscallKind::RenameAt | SyscallKind::RenameAt2 => {
            if !capture_argument(pid, args[0] as i32, args[1], false, store)? {
                capture_argument(pid, args[2] as i32, args[3], false, store)?;
            }
        }
        SyscallKind::Symlink => {
            capture_argument(pid, AT_FDCWD, args[1], false, store)?;
        }
        SyscallKind::SymlinkAt => {
            capture_argument(pid, args[1] as i32, args[2], false, store)?;
        }
        SyscallKind::Link => {
            capture_argument(pid, AT_FDCWD, args[1], false, store)?;
            store.gap(
                "hard_link",
                "link observed; aliases cannot be fully discovered",
            );
        }
        SyscallKind::LinkAt => {
            capture_argument(pid, args[2] as i32, args[3], false, store)?;
            store.gap(
                "hard_link",
                "linkat observed; aliases cannot be fully discovered",
            );
        }
        SyscallKind::Mkdir | SyscallKind::MkdirAt | SyscallKind::Rmdir => {}
        SyscallKind::IoUringSetup => {
            store.gap(
                "io_uring",
                "io_uring_setup observed; writes may bypass tracked entry points",
            );
        }
        SyscallKind::Unknown => {
            store.gap("unknown_syscall", request.data.nr.to_string());
        }
    }
    Ok(())
}

fn writable_flags(flags: i32) -> bool {
    flags & (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) != 0
        || flags & O_TMPFILE_MASK == O_TMPFILE_MASK
}

fn capture_argument(
    pid: u32,
    dirfd: i32,
    address: u64,
    follow_final: bool,
    store: &mut CaptureStore,
) -> io::Result<bool> {
    let raw = read_c_string(pid, address)?;
    let path = resolve_path(pid, dirfd, &raw, follow_final)?;
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
        store.gap(
            "directory_operation",
            format!("directory mutation at {}", path.display()),
        );
        return Ok(true);
    }
    store.capture_before(&path);
    Ok(false)
}

fn read_c_string(pid: u32, address: u64) -> io::Result<Vec<u8>> {
    let memory = File::open(format!("/proc/{pid}/mem"))?;
    let mut result = Vec::new();
    let mut offset = 0usize;
    while result.len() < PATH_LIMIT {
        let mut chunk = [0u8; 256];
        let count = memory.read_at(&mut chunk, address + offset as u64)?;
        if count == 0 {
            break;
        }
        if let Some(end) = chunk[..count].iter().position(|byte| *byte == 0) {
            result.extend_from_slice(&chunk[..end]);
            return Ok(result);
        }
        result.extend_from_slice(&chunk[..count]);
        offset += count;
    }
    Err(io::Error::new(
        ErrorKind::InvalidData,
        "pathname is missing NUL terminator or exceeds 64 KiB",
    ))
}

fn read_value<T: Default>(pid: u32, address: u64) -> io::Result<T> {
    let memory = File::open(format!("/proc/{pid}/mem"))?;
    let mut value = T::default();
    // SAFETY: value is initialized and exposed only as its exact byte-sized mutable buffer.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut value as *mut T).cast::<u8>(),
            std::mem::size_of::<T>(),
        )
    };
    memory.read_exact_at(bytes, address)?;
    Ok(value)
}

fn resolve_path(pid: u32, dirfd: i32, raw: &[u8], follow_final: bool) -> io::Result<PathBuf> {
    let raw_path = PathBuf::from(OsString::from_vec(raw.to_vec()));
    let path = if raw_path.is_absolute() {
        let root = fs::read_link(format!("/proc/{pid}/root"))?;
        root.join(raw_path.strip_prefix("/").unwrap_or(&raw_path))
    } else {
        let base = if dirfd == AT_FDCWD {
            fs::read_link(format!("/proc/{pid}/cwd"))?
        } else {
            fs::read_link(format!("/proc/{pid}/fd/{dirfd}"))?
        };
        base.join(raw_path)
    };
    let path = normalize(&path);
    if follow_final {
        if let Ok(canonical) = fs::canonicalize(&path) {
            return Ok(canonical);
        }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            if let Ok(parent) = fs::canonicalize(parent) {
                return Ok(parent.join(name));
            }
        }
    }
    Ok(path)
}

fn fd_path(pid: u32, fd: i32) -> io::Result<Option<PathBuf>> {
    let path = fs::read_link(format!("/proc/{pid}/fd/{fd}"))?;
    let bytes = path.as_os_str().as_bytes();
    if bytes.ends_with(b" (deleted)") {
        return Ok(None);
    }
    Ok(Some(path))
}

fn has_live_children(excluded: Option<libc::pid_t>) -> io::Result<bool> {
    let mut children = HashSet::new();
    for task in fs::read_dir("/proc/self/task")? {
        let task = task?;
        let path = task.path().join("children");
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        children.extend(
            contents
                .split_ascii_whitespace()
                .filter_map(|value| value.parse::<libc::pid_t>().ok()),
        );
    }
    let mut found = false;
    for pid in children {
        if Some(pid) == excluded {
            continue;
        }
        let mut status = 0;
        // SAFETY: pid came from this process's task children lists and status is writable.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        match result.cmp(&0) {
            std::cmp::Ordering::Equal => found = true,
            std::cmp::Ordering::Less => {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ECHILD) {
                    return Err(error);
                }
            }
            std::cmp::Ordering::Greater => {}
        }
    }
    Ok(found)
}

fn capture_writable_stdio(store: &mut CaptureStore) {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: F_GETFL does not modify userspace memory and fd is a standard descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags >= 0 && writable_flags(flags) {
            match fs::read_link(format!("/proc/self/fd/{fd}")) {
                Ok(path) if path.is_absolute() => store.capture_before(&path),
                Ok(_) => {}
                Err(error) => store.gap("stdio_fd", format!("cannot inspect fd {fd}: {error}")),
            }
        }
    }
}

fn install_signal_handlers() -> io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: record_signal only performs an atomic store, which is signal-safe.
        if unsafe { libc::signal(signal, record_signal as libc::sighandler_t) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn terminate_process_group(pid: libc::pid_t) {
    // SAFETY: negative pid addresses the process group created by the child.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(500));
        libc::kill(-pid, libc::SIGKILL);
    }
}

fn reap_for(duration: Duration, excluded: Option<libc::pid_t>) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if has_live_children(excluded).ok() == Some(false) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_open_flags_include_create_and_truncate() {
        assert!(writable_flags(libc::O_RDONLY | libc::O_CREAT));
        assert!(writable_flags(libc::O_WRONLY));
        assert!(!writable_flags(libc::O_RDONLY));
    }

    #[test]
    fn c_string_read_rejects_invalid_process() {
        assert!(read_c_string(u32::MAX, 0).is_err());
    }
}
