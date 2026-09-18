use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use fs_tracker::config::{self, Limits, RunConfig};

#[derive(Debug, Parser)]
#[command(name = "fs-tracker", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Probe the kernel capabilities required by the tracker.
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a command and track file changes made by its process tree.
    Run(RunArgs),
    /// Print the unified patch produced by a completed run.
    Diff {
        /// Run output directory.
        output: PathBuf,
    },
    #[command(hide = true)]
    CaptureHelper,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Tracked root as ID=PATH. May be repeated.
    #[arg(long = "root", required = true)]
    roots: Vec<String>,
    /// New directory in which tracker artifacts will be written.
    #[arg(long)]
    output: PathBuf,
    /// Excluded subtree as ROOT_ID=RELATIVE_PATH. May be repeated.
    #[arg(long = "exclude")]
    exclusions: Vec<String>,
    #[arg(long, default_value_t = 10_000)]
    max_files: usize,
    #[arg(long, default_value_t = 33_554_432)]
    max_file_bytes: u64,
    #[arg(long, default_value_t = 268_435_456)]
    max_total_bytes: u64,
    #[arg(long, default_value_t = 2_097_152)]
    max_diff_bytes: usize,
    /// Number of notification-processing workers.
    #[arg(long, default_value_t = 4, value_parser = parse_worker_count)]
    capture_workers: usize,
    /// Maximum number of notifications waiting for a capture worker.
    #[arg(long, default_value_t = 256, value_parser = parse_positive_usize)]
    notification_queue: usize,
    /// Inherited supervisor-only FD accepting a single `finish` line.
    #[arg(long, value_parser = parse_control_fd)]
    finish_fd: Option<i32>,
    /// Seconds between TERM and KILL after a finish request.
    #[arg(long, default_value_t = 5)]
    termination_grace_seconds: u64,
    /// Maximum seconds allowed for one file capture helper.
    #[arg(long, default_value_t = 30, value_parser = parse_positive_u64)]
    capture_timeout_seconds: u64,
    #[arg(long, hide = true, default_value_t = 0)]
    capture_helper_delay_millis: u64,
    /// Command and arguments, normally preceded by --.
    #[arg(last = true, required = true, num_args = 1.., allow_hyphen_values = true)]
    command: Vec<OsString>,
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid positive integer: {error}"))?;
    if parsed == 0 {
        return Err("value must be greater than zero".into());
    }
    Ok(parsed)
}

fn parse_positive_u64(value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|error| format!("invalid positive integer: {error}"))?;
    if parsed == 0 {
        return Err("value must be greater than zero".into());
    }
    Ok(parsed)
}

fn parse_control_fd(value: &str) -> Result<i32, String> {
    let fd = value
        .parse::<i32>()
        .map_err(|error| format!("invalid file descriptor: {error}"))?;
    if fd < 3 {
        return Err("control FD must be 3 or greater".into());
    }
    // SAFETY: F_GETFD only checks whether the inherited descriptor is valid.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(format!("control FD {fd} is not open"));
    }
    Ok(fd)
}

fn parse_worker_count(value: &str) -> Result<usize, String> {
    let parsed = parse_positive_usize(value)?;
    if parsed > 64 {
        return Err("capture worker count cannot exceed 64".into());
    }
    Ok(parsed)
}

fn main() {
    let code = match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fs-tracker: {error}");
            125
        }
    };
    std::process::exit(code);
}

fn execute(cli: Cli) -> Result<i32, Box<dyn std::error::Error>> {
    match cli.command {
        Command::Doctor { json } => {
            let report = fs_tracker::doctor();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "supported: {}\nkernel: {}\narchitecture: {}\nlistener: {}\n/proc mem: {}",
                    report.supported,
                    report.kernel,
                    report.architecture,
                    report.listener_probe,
                    report.proc_mem
                );
            }
            Ok(if report.supported { 0 } else { 1 })
        }
        Command::Run(args) => {
            let roots = config::parse_roots(&args.roots)?;
            let exclusions = config::parse_exclusions(&args.exclusions, &roots)?;
            config::validate_output(&args.output, &roots)?;
            let result = fs_tracker::supervisor::run(RunConfig {
                roots,
                exclusions,
                output: args.output,
                command: args.command,
                limits: Limits {
                    max_files: args.max_files,
                    max_file_bytes: args.max_file_bytes,
                    max_total_bytes: args.max_total_bytes,
                    max_diff_bytes: args.max_diff_bytes,
                },
                capture_workers: args.capture_workers,
                notification_queue: args.notification_queue,
                finish_fd: args.finish_fd,
                termination_grace: std::time::Duration::from_secs(args.termination_grace_seconds),
                capture_timeout: std::time::Duration::from_secs(args.capture_timeout_seconds),
                capture_helper_delay: std::time::Duration::from_millis(
                    args.capture_helper_delay_millis,
                ),
            })?;
            Ok(result
                .exit_code
                .unwrap_or_else(|| 128 + result.signal.unwrap_or(1)))
        }
        Command::Diff { output } => {
            fs_tracker::report::print_diff(&output)?;
            Ok(0)
        }
        Command::CaptureHelper => {
            fs_tracker::capture::capture_helper_loop()?;
            Ok(0)
        }
    }
}
