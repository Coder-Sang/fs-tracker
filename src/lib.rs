pub mod capture;
pub mod config;
pub mod contracts;
pub mod git_adapter;
pub mod linux;
pub mod report;
pub mod supervisor;

use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub supported: bool,
    pub kernel: String,
    pub architecture: String,
    pub notification_sizes: Option<(u16, u16, u16)>,
    pub listener_probe: String,
    pub proc_mem: String,
}

pub fn doctor() -> DoctorReport {
    let kernel = linux::kernel_release().unwrap_or_else(|error| format!("unknown: {error}"));
    let sizes = linux::notification_sizes().ok();
    let proc_mem = std::fs::File::open(format!("/proc/{}/mem", std::process::id()))
        .map(|_| "available".to_owned())
        .unwrap_or_else(|error| format!("unavailable: {error}"));
    let command = vec![OsString::from("/bin/true")];
    let listener_result = linux::spawn(&command).and_then(|child| loop {
        if linux::poll_listener(child.listener.as_raw_fd(), 20)? {
            let request = linux::receive_notification(child.listener.as_raw_fd())?;
            linux::continue_notification(child.listener.as_raw_fd(), request.id)?;
        }
        if let Some(status) = linux::wait_nohang(child.pid)? {
            if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                return Ok("available".to_owned());
            }
            return Err(io::Error::other(format!(
                "probe exited with status {status}"
            )));
        }
    });
    let listener_probe = listener_result.unwrap_or_else(|error| format!("unavailable: {error}"));
    let supported = sizes.is_some()
        && listener_probe == "available"
        && proc_mem == "available"
        && matches!(std::env::consts::ARCH, "aarch64" | "x86_64");
    DoctorReport {
        supported,
        kernel,
        architecture: std::env::consts::ARCH.into(),
        notification_sizes: sizes,
        listener_probe,
        proc_mem,
    }
}
