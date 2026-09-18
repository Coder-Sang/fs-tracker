use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::ptr;

pub const AT_FDCWD: i32 = -100;
pub const USER_NOTIF_FLAG_CONTINUE: u32 = 1;
const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_GET_NOTIF_SIZES: libc::c_uint = 3;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_PDEATHSIG: libc::c_int = 1;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;
const NOTIF_RECV: libc::c_ulong = 0xc050_2100;
const NOTIF_SEND: libc::c_ulong = 0xc018_2101;
const NOTIF_ID_VALID: libc::c_ulong = 0x4008_2102;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SeccompData {
    pub nr: i32,
    pub arch: u32,
    pub instruction_pointer: u64,
    pub args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Notification {
    pub id: u64,
    pub pid: u32,
    pub flags: u32,
    pub data: SeccompData,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Response {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct NotificationSizes {
    notification: u16,
    response: u16,
    data: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallKind {
    Open,
    OpenAt,
    OpenAt2,
    Truncate,
    Ftruncate,
    Unlink,
    UnlinkAt,
    Rename,
    RenameAt,
    RenameAt2,
    Chmod,
    Fchmod,
    FchmodAt,
    FchmodAt2,
    Symlink,
    SymlinkAt,
    Link,
    LinkAt,
    Mkdir,
    MkdirAt,
    Rmdir,
    IoUringSetup,
    Unknown,
}

pub fn syscall_kind(number: i32) -> SyscallKind {
    macro_rules! is_syscall {
        ($name:ident, $kind:ident) => {
            if number == libc::$name as i32 {
                return SyscallKind::$kind;
            }
        };
    }
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_open, Open);
    is_syscall!(SYS_openat, OpenAt);
    is_syscall!(SYS_openat2, OpenAt2);
    is_syscall!(SYS_truncate, Truncate);
    is_syscall!(SYS_ftruncate, Ftruncate);
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_unlink, Unlink);
    is_syscall!(SYS_unlinkat, UnlinkAt);
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_rename, Rename);
    is_syscall!(SYS_renameat, RenameAt);
    is_syscall!(SYS_renameat2, RenameAt2);
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_chmod, Chmod);
    is_syscall!(SYS_fchmod, Fchmod);
    is_syscall!(SYS_fchmodat, FchmodAt);
    if number == 452 {
        return SyscallKind::FchmodAt2;
    }
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_symlink, Symlink);
    is_syscall!(SYS_symlinkat, SymlinkAt);
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_link, Link);
    is_syscall!(SYS_linkat, LinkAt);
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_mkdir, Mkdir);
    is_syscall!(SYS_mkdirat, MkdirAt);
    #[cfg(target_arch = "x86_64")]
    is_syscall!(SYS_rmdir, Rmdir);
    is_syscall!(SYS_io_uring_setup, IoUringSetup);
    SyscallKind::Unknown
}

pub fn supported_syscall_numbers() -> Vec<i64> {
    #[allow(unused_mut)]
    let mut numbers = vec![
        libc::SYS_openat,
        libc::SYS_openat2,
        libc::SYS_truncate,
        libc::SYS_ftruncate,
        libc::SYS_unlinkat,
        libc::SYS_renameat,
        libc::SYS_renameat2,
        libc::SYS_fchmod,
        libc::SYS_fchmodat,
        452, // fchmodat2, not exposed by libc on every supported ABI.
        libc::SYS_symlinkat,
        libc::SYS_linkat,
        libc::SYS_mkdirat,
        libc::SYS_io_uring_setup,
    ];
    #[cfg(target_arch = "x86_64")]
    numbers.extend([
        libc::SYS_open,
        libc::SYS_unlink,
        libc::SYS_rename,
        libc::SYS_chmod,
        libc::SYS_symlink,
        libc::SYS_link,
        libc::SYS_mkdir,
        libc::SYS_rmdir,
    ]);
    numbers
}

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

pub fn notification_sizes() -> io::Result<(u16, u16, u16)> {
    let mut sizes = NotificationSizes::default();
    // SAFETY: sizes points to the kernel ABI structure and the syscall does not retain it.
    let result =
        unsafe { libc::syscall(libc::SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, &mut sizes) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((sizes.notification, sizes.response, sizes.data))
}

fn install_filter() -> io::Result<OwnedFd> {
    let mut filters = vec![
        stmt(BPF_LD | BPF_W | BPF_ABS, 4),
        jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH, 1, 0),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        stmt(BPF_LD | BPF_W | BPF_ABS, 0),
    ];
    for number in supported_syscall_numbers() {
        filters.push(jump(BPF_JMP | BPF_JEQ | BPF_K, number as u32, 0, 1));
        filters.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF));
    }
    filters.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    let program = SockFprog {
        len: filters.len() as u16,
        filter: filters.as_ptr(),
    };
    // SAFETY: prctl and seccomp receive scalar arguments and a valid program for this call.
    unsafe {
        if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &program,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd as RawFd))
    }
}

fn send_fd(socket: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = [b'L'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) as usize };
    let mut control = vec![0u8; space];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    // SAFETY: control has CMSG_SPACE bytes and all pointers remain valid for sendmsg.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as usize;
        ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(header),
            mem::size_of::<RawFd>(),
        );
        if libc::sendmsg(socket, &message, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn receive_fd(socket: RawFd) -> io::Result<OwnedFd> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) as usize };
    let mut control = vec![0u8; space];
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    // SAFETY: receive buffers are valid and ancillary data is validated before extraction.
    unsafe {
        if libc::recvmsg(socket, &mut message, 0) <= 0 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "listener FD missing",
            ));
        }
        let fd = ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>());
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

pub struct Child {
    pub pid: libc::pid_t,
    pub listener: OwnedFd,
}

pub fn spawn(command: &[std::ffi::OsString]) -> io::Result<Child> {
    let arguments: Vec<CString> = command
        .iter()
        .map(|value| CString::new(OsStr::new(value).as_bytes()))
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argv contains NUL"))?;
    let pointers: Vec<*const libc::c_char> = arguments
        .iter()
        .map(|value| value.as_ptr())
        .chain(std::iter::once(ptr::null()))
        .collect();
    let mut sockets = [0; 2];
    // SAFETY: socketpair initializes both array entries.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: no other Rust threads exist when this library is invoked by the CLI.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(sockets[0]);
            libc::close(sockets[1]);
        }
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        unsafe {
            libc::close(sockets[0]);
            libc::setpgid(0, 0);
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(signal, libc::SIG_DFL);
            }
            libc::prctl(PR_SET_PDEATHSIG, libc::SIGKILL);
        }
        let outcome = install_filter().and_then(|listener| {
            send_fd(sockets[1], listener.as_raw_fd())?;
            let mut ack = [0u8; 1];
            // SAFETY: ack is writable and socket is valid in the child.
            if unsafe { libc::read(sockets[1], ack.as_mut_ptr().cast(), 1) } != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "supervisor handshake failed",
                ));
            }
            Ok(())
        });
        if outcome.is_err() {
            unsafe { libc::_exit(125) };
        }
        unsafe {
            libc::close(sockets[1]);
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) < 0 {
                for fd in 3..1024 {
                    libc::close(fd);
                }
            }
            libc::execvp(arguments[0].as_ptr(), pointers.as_ptr());
            libc::_exit(126);
        }
    }
    unsafe { libc::close(sockets[1]) };
    let listener = receive_fd(sockets[0]);
    if listener.is_err() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, ptr::null_mut(), 0);
            libc::close(sockets[0]);
        }
        return listener.map(|listener| Child { pid, listener });
    }
    let ack = [b'R'];
    // SAFETY: one-byte handshake over a valid socket.
    let wrote = unsafe { libc::write(sockets[0], ack.as_ptr().cast(), 1) };
    unsafe { libc::close(sockets[0]) };
    if wrote != 1 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        return Err(io::Error::last_os_error());
    }
    Ok(Child {
        pid,
        listener: listener.unwrap(),
    })
}

pub fn receive_notification(listener: RawFd) -> io::Result<Notification> {
    let mut request = Notification::default();
    // SAFETY: request has the kernel-reported ABI layout checked by doctor/run.
    if unsafe { libc::ioctl(listener, NOTIF_RECV, &mut request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(request)
}

pub fn notification_valid(listener: RawFd, id: u64) -> io::Result<()> {
    let identifier = id;
    // SAFETY: ioctl reads a u64 notification identifier.
    if unsafe { libc::ioctl(listener, NOTIF_ID_VALID, &identifier) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn continue_notification(listener: RawFd, id: u64) -> io::Result<()> {
    let response = Response {
        id,
        val: 0,
        error: 0,
        flags: USER_NOTIF_FLAG_CONTINUE,
    };
    // SAFETY: response is initialized according to the seccomp notification ABI.
    if unsafe { libc::ioctl(listener, NOTIF_SEND, &response) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn poll_listener(listener: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor points to one initialized pollfd.
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(result > 0 && descriptor.revents & libc::POLLIN != 0)
}

pub fn wait_nohang(pid: libc::pid_t) -> io::Result<Option<i32>> {
    let mut status = 0;
    // SAFETY: status is valid and pid is a child process.
    let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((result == pid).then_some(status))
}

pub fn target_result(status: i32) -> crate::contracts::TargetResult {
    if libc::WIFEXITED(status) {
        crate::contracts::TargetResult {
            exit_code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else {
        crate::contracts::TargetResult {
            exit_code: None,
            signal: libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status)),
        }
    }
}

pub fn kernel_release() -> io::Result<String> {
    let mut name = MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: uname initializes the complete structure on success.
    if unsafe { libc::uname(name.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let name = unsafe { name.assume_init() };
    let release = unsafe { CStr::from_ptr(name.release.as_ptr()) };
    Ok(release.to_string_lossy().into_owned())
}
