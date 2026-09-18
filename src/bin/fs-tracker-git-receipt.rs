use std::path::PathBuf;

use clap::Parser;
use fs_tracker::config;
use fs_tracker::git_adapter::{self, GitReceiptConfig};

#[derive(Debug, Parser)]
#[command(name = "fs-tracker-git-receipt", version, about)]
struct Cli {
    /// Completed tracker output directory.
    #[arg(long)]
    tracker_output: PathBuf,
    /// New or existing dedicated bare Git repository.
    #[arg(long)]
    repository: PathBuf,
    /// Receipt V2 destination path.
    #[arg(long)]
    receipt: PathBuf,
    #[arg(long)]
    run_id: String,
    /// Workspace SHA-256 identity.
    #[arg(long)]
    workspace_id: String,
    /// Root mapping as ID=SANDBOX_PROJECT_PATH. May be repeated.
    #[arg(long = "project", required = true)]
    projects: Vec<String>,
    /// Run-unique ref updated to the report commit.
    #[arg(long)]
    report_ref: String,
    /// Permit publication of a partial tracker report.
    #[arg(long)]
    allow_partial: bool,
}

fn main() {
    if let Err(error) = execute(Cli::parse()) {
        eprintln!("fs-tracker-git-receipt: {error}");
        std::process::exit(1);
    }
}

fn execute(args: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let projects = config::parse_roots(&args.projects)?
        .into_iter()
        .map(|root| (root.id, root.path))
        .collect();
    let receipt = git_adapter::create_receipt(GitReceiptConfig {
        tracker_output: args.tracker_output,
        repository: args.repository,
        receipt: args.receipt,
        run_id: args.run_id,
        workspace_id: args.workspace_id,
        report_ref: args.report_ref,
        projects,
        allow_partial: args.allow_partial,
    })?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}
