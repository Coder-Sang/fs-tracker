#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::process::Command;
use std::time::{Duration, Instant};

use fs_tracker::contracts::{ChangeKind, Report, RunState};
use tempfile::tempdir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_fs-tracker")
}

fn git_adapter_binary() -> &'static str {
    env!("CARGO_BIN_EXE_fs-tracker-git-receipt")
}

#[test]
fn doctor_runs_a_real_listener_probe() {
    let output = Command::new(binary())
        .args(["doctor", "--json"])
        .output()
        .expect("doctor starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["supported"], true);
    assert_eq!(value["listener_probe"], "available");
}

#[test]
fn reports_net_changes_and_eliminates_transients() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("modified.txt"), b"before\n").unwrap();
    fs::write(root.join("deleted.txt"), b"deleted\n").unwrap();
    fs::write(root.join("reverted.txt"), b"same\n").unwrap();
    fs::write(root.join("mode.txt"), b"mode\n").unwrap();
    fs::write(root.join("replaced.txt"), b"old\n").unwrap();
    let script = r#"
printf 'after\n' > "$1/modified.txt"
printf 'added\n' > "$1/added.txt"
rm "$1/deleted.txt"
printf 'temporary\n' > "$1/transient.txt"
rm "$1/transient.txt"
printf 'other\n' > "$1/reverted.txt"
printf 'same\n' > "$1/reverted.txt"
printf 'replacement\n' > "$1/temp.txt"
mv "$1/temp.txt" "$1/replaced.txt"
chmod +x "$1/mode.txt"
"#;
    let status = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .arg("--output")
        .arg(&output)
        .args(["--", "/bin/sh", "-c", script, "test"])
        .arg(&root)
        .status()
        .expect("tracker starts");
    assert!(status.success());

    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.state, RunState::Finished);
    assert_eq!(report.changes.len(), 5);
    let changes: Vec<_> = report
        .changes
        .iter()
        .map(|change| (change.display_path.as_deref().unwrap(), &change.kind))
        .collect();
    assert!(changes
        .iter()
        .any(|item| item.0 == "added.txt" && matches!(item.1, ChangeKind::Added)));
    assert!(changes
        .iter()
        .any(|item| item.0 == "deleted.txt" && matches!(item.1, ChangeKind::Deleted)));
    assert!(changes
        .iter()
        .any(|item| item.0 == "modified.txt" && matches!(item.1, ChangeKind::Modified)));
    assert!(changes
        .iter()
        .any(|item| item.0 == "replaced.txt" && matches!(item.1, ChangeKind::Modified)));
    assert!(changes
        .iter()
        .any(|item| item.0 == "mode.txt" && matches!(item.1, ChangeKind::ModeChanged)));
    let mode = report
        .changes
        .iter()
        .find(|change| change.display_path.as_deref() == Some("mode.txt"))
        .unwrap();
    assert_eq!(mode.diff.kind, "metadata");
    assert_eq!(mode.diff.patch_bytes, None);
    assert!(!changes
        .iter()
        .any(|item| item.0 == "reverted.txt" || item.0 == "transient.txt"));
    assert!(output.join("changes.patch").is_file());
    assert!(output.join("journal.jsonl").is_file());
}

#[test]
fn exclusions_skip_subtrees_and_preserve_cross_boundary_renames() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    fs::create_dir_all(root.join(".venv")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::create_dir_all(root.join(".venv-old")).unwrap();
    fs::write(root.join(".venv/ignored.txt"), b"before\n").unwrap();
    fs::write(root.join(".venv/source.txt"), b"move in\n").unwrap();
    fs::write(root.join("node_modules/pkg/ignored.js"), b"before\n").unwrap();
    fs::write(root.join(".venv-old/tracked.txt"), b"before\n").unwrap();
    fs::write(root.join("tracked.txt"), b"before\n").unwrap();
    fs::write(root.join("move-out.txt"), b"move out\n").unwrap();
    let script = r#"
printf 'ignored\n' > "$1/.venv/ignored.txt"
printf 'ignored\n' > "$1/node_modules/pkg/ignored.js"
printf 'after\n' > "$1/.venv-old/tracked.txt"
printf 'after\n' > "$1/tracked.txt"
mv "$1/.venv/source.txt" "$1/from-excluded.txt"
mv "$1/move-out.txt" "$1/.venv/moved-out.txt"
"#;
    let status = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .args(["--exclude", "main=.venv", "--exclude", "main=node_modules"])
        .arg("--output")
        .arg(&output)
        .args(["--", "/bin/sh", "-c", script, "test"])
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success());

    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.state, RunState::Finished);
    assert_eq!(report.changes.len(), 4);
    let changes: Vec<_> = report
        .changes
        .iter()
        .map(|change| (change.display_path.as_deref().unwrap(), &change.kind))
        .collect();
    assert!(changes
        .iter()
        .any(|item| item.0 == "tracked.txt" && matches!(item.1, ChangeKind::Modified)));
    assert!(changes
        .iter()
        .any(|item| item.0 == ".venv-old/tracked.txt" && matches!(item.1, ChangeKind::Modified)));
    assert!(changes
        .iter()
        .any(|item| item.0 == "from-excluded.txt" && matches!(item.1, ChangeKind::Added)));
    assert!(changes
        .iter()
        .any(|item| item.0 == "move-out.txt" && matches!(item.1, ChangeKind::Deleted)));
    assert!(!changes
        .iter()
        .any(|item| { item.0 == ".venv/ignored.txt" || item.0 == "node_modules/pkg/ignored.js" }));
    let exclusions: Vec<_> = report
        .coverage
        .configured_exclusions
        .iter()
        .map(|exclusion| (exclusion.root_id.as_str(), exclusion.display_path.as_str()))
        .collect();
    assert_eq!(
        exclusions,
        vec![("main", ".venv"), ("main", "node_modules")]
    );
}

#[test]
fn policy_file_tracks_multiple_roots_and_recursive_exclusions() {
    let parent = tempdir().unwrap();
    let first = parent.path().join("first");
    let second = parent.path().join("second");
    let output = parent.path().join("result");
    let policy = parent.path().join("policy.json");
    fs::create_dir_all(first.join("nested/.git")).unwrap();
    fs::create_dir_all(second.join("packages/app/node_modules/pkg")).unwrap();
    fs::write(first.join("nested/.git/index"), b"before\n").unwrap();
    fs::write(
        second.join("packages/app/node_modules/pkg/index.js"),
        b"before\n",
    )
    .unwrap();
    fs::write(second.join("tracked.txt"), b"before\n").unwrap();
    fs::write(
        &policy,
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "roots": [
                {"id": "first", "path": first},
                {"id": "second", "path": second},
            ],
            "recursiveExclusions": [".git", "node_modules"],
        }))
        .unwrap(),
    )
    .unwrap();
    let script = r#"
printf 'ignored\n' > "$1/nested/.git/index"
printf 'ignored\n' > "$2/packages/app/node_modules/pkg/index.js"
printf 'after\n' > "$2/tracked.txt"
"#;

    let status = Command::new(binary())
        .arg("run")
        .arg("--config")
        .arg(&policy)
        .arg("--output")
        .arg(&output)
        .args(["--", "/bin/sh", "-c", script, "test"])
        .arg(&first)
        .arg(&second)
        .status()
        .unwrap();

    assert!(status.success());
    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.changes.len(), 1);
    assert_eq!(report.changes[0].root_id, "second");
    assert_eq!(
        report.changes[0].display_path.as_deref(),
        Some("tracked.txt")
    );
    assert_eq!(
        report.coverage.configured_recursive_exclusions,
        vec![".git", "node_modules"]
    );

    let adapted = Command::new(git_adapter_binary())
        .arg("--config")
        .arg(&policy)
        .arg("--tracker-output")
        .arg(&output)
        .arg("--repository")
        .arg(parent.path().join("report.git"))
        .arg("--receipt")
        .arg(parent.path().join("receipt.json"))
        .args(["--run-id", "run-policy", "--workspace-id"])
        .arg("a".repeat(64))
        .args(["--report-ref", "refs/fs-tracker/reports/run-policy"])
        .status()
        .unwrap();
    assert!(adapted.success());
}

#[test]
fn finish_control_fd_stops_a_long_running_target() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    fs::create_dir(&root).unwrap();
    let mut pipe = [0; 2];
    // SAFETY: pipe initializes both descriptors on success.
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let mut child = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .arg("--output")
        .arg(&output)
        .arg("--finish-fd")
        .arg(pipe[0].to_string())
        .arg("--termination-grace-seconds")
        .arg("1")
        .args(["--", "/bin/sleep", "30"])
        .spawn()
        .unwrap();
    // SAFETY: the tracker inherited its own copy of the read end.
    unsafe { libc::close(pipe[0]) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !output.join("status.json").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: this test uniquely owns the write end after from_raw_fd.
    let mut control = unsafe { std::fs::File::from_raw_fd(pipe[1]) };
    control.write_all(b"finish\n").unwrap();
    drop(control);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.target.signal, Some(libc::SIGTERM));
    assert_eq!(
        report.state,
        RunState::Finished,
        "unexpected gaps: {:?}",
        report.coverage.gaps
    );
}

#[test]
fn git_adapter_writes_receipt_v2_and_private_commit_pair() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    let repository = parent.path().join("report.git");
    let receipt = parent.path().join("receipt.json");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file.txt"), b"before\n").unwrap();
    let tracked = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .arg("--output")
        .arg(&output)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "printf 'after\\n' > \"$1/file.txt\"",
            "test",
        ])
        .arg(&root)
        .status()
        .unwrap();
    assert!(tracked.success());
    let adapted = Command::new(git_adapter_binary())
        .arg("--tracker-output")
        .arg(&output)
        .arg("--repository")
        .arg(&repository)
        .arg("--receipt")
        .arg(&receipt)
        .args(["--run-id", "run-1", "--workspace-id"])
        .arg("a".repeat(64))
        .arg("--project")
        .arg(format!("main={}", root.display()))
        .args(["--report-ref", "refs/fs-tracker/reports/run-1"])
        .status()
        .unwrap();
    assert!(adapted.success());
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    assert_eq!(value["schemaVersion"], 2);
    let baseline = value["baselineCommitSha"].as_str().unwrap();
    let report = value["reportCommitSha"].as_str().unwrap();
    let changed = Command::new("git")
        .arg(format!("--git-dir={}", repository.display()))
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            baseline,
            report,
        ])
        .output()
        .unwrap();
    assert!(changed.status.success());
    assert!(String::from_utf8(changed.stdout)
        .unwrap()
        .ends_with("/file.txt\n"));
}

#[test]
fn quota_failure_marks_report_partial() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("large.txt"), b"original\n").unwrap();
    let status = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .arg("--output")
        .arg(&output)
        .args(["--max-file-bytes", "1", "--", "/bin/sh", "-c"])
        .arg("printf 'changed\\n' > \"$1/large.txt\"")
        .arg("test")
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success());
    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.state, RunState::Partial);
    assert!(report
        .coverage
        .gaps
        .iter()
        .any(|gap| gap.kind == "file_size_quota"));
}

#[test]
fn capture_timeout_kills_helper_and_releases_target() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file.txt"), b"before\n").unwrap();
    let started = Instant::now();
    let status = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .arg("--output")
        .arg(&output)
        .args([
            "--capture-timeout-seconds",
            "1",
            "--capture-helper-delay-millis",
            "2000",
            "--",
            "/bin/sh",
            "-c",
            "printf 'after\\n' > \"$1/file.txt\"",
            "test",
        ])
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"after\n");
    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.state, RunState::Partial);
    assert!(report
        .coverage
        .gaps
        .iter()
        .any(|gap| gap.kind == "capture_timeout"));
}

#[test]
fn preserves_target_exit_code_and_writes_report() {
    let parent = tempdir().unwrap();
    let root = parent.path().join("root");
    let output = parent.path().join("result");
    fs::create_dir(&root).unwrap();
    let status = Command::new(binary())
        .args(["run", "--root"])
        .arg(format!("main={}", root.display()))
        .arg("--output")
        .arg(&output)
        .args([
            "--",
            "/bin/sh",
            "-c",
            "printf x > \"$1/file\"; exit 7",
            "test",
        ])
        .arg(&root)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(7));
    let report: Report =
        serde_json::from_slice(&fs::read(output.join("report.json")).unwrap()).unwrap();
    assert_eq!(report.target.exit_code, Some(7));
    assert_eq!(report.changes.len(), 1);
}
