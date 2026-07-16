mod common;

use chrono::Utc;
use codex_patcher::dispatch::{MANAGED_UPDATE_OVERRIDE, inject_managed_update_override};
use codex_patcher::state::StateStore;
use codex_patcher::types::{FailureRecord, ProbeKind};
use common::{DispatcherFixture, line_count, wait_for_file, write_state_snapshot};
use std::ffi::OsString;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn earlier_probe_result_is_observed_by_the_next_launch_only() {
    let fixture = DispatcherFixture::new("error", "error");
    let pending = fixture.pending_state("a patched update is pending");
    let replacement = fixture.root.join("pending-state.json");
    let marker = fixture.root.join("probe-finished");
    let run_count = fixture.root.join("active-runs");
    write_state_snapshot(&replacement, &pending);

    let first = fixture.run(["app-server"], |command| {
        command
            .env("CODEX_PATCHER_TEST_PROBE_STATE", &replacement)
            .env("CODEX_PATCHER_TEST_STATE_PATH", fixture.paths.state_file())
            .env("CODEX_PATCHER_TEST_PROBE_MARKER", &marker)
            .env("CODEX_PATCHER_TEST_RUN_COUNT", &run_count)
            .env("CODEX_PATCHER_TEST_STDOUT", "protocol-frame\n");
    });
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(first.stdout, b"protocol-frame\n");
    assert!(first.stderr.is_empty(), "{:?}", first.stderr);
    assert_eq!(line_count(&run_count), 1);

    wait_for_file(&marker);
    assert!(matches!(fixture.state().probe.kind, ProbeKind::Pending));

    let second = fixture.run(["app-server"], |command| {
        command
            .env("CODEX_PATCHER_TEST_PROBE_STATE", &replacement)
            .env("CODEX_PATCHER_TEST_STATE_PATH", fixture.paths.state_file())
            .env("CODEX_PATCHER_TEST_RUN_COUNT", &run_count)
            .env("CODEX_PATCHER_TEST_STDOUT", "must-not-run\n");
    });
    assert_eq!(second.status.code(), Some(75));
    assert!(second.stdout.is_empty());
    assert!(String::from_utf8_lossy(&second.stderr).contains("a patched update is pending"));
    assert_eq!(line_count(&run_count), 1, "launch B ran old Codex");

    let probe_log = fs::read_to_string(fixture.paths.logs_dir().join("probe.log"))
        .expect("read detached probe log");
    assert!(probe_log.contains("detached probe stdout"));
    assert!(probe_log.contains("detached probe stderr"));
}

#[test]
fn service_stdout_is_clean_and_override_is_injected_before_delimiter() {
    let fixture = DispatcherFixture::new("error", "error");
    let captured = fixture.root.join("arguments.txt");
    let output = fixture.run(["app-server", "--", "literal"], |command| {
        command
            .env("CODEX_PATCHER_TEST_ARGUMENTS", &captured)
            .env("CODEX_PATCHER_TEST_STDOUT", "json-rpc-only\n");
    });

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"json-rpc-only\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(captured).expect("read arguments"),
        format!("app-server\n-c\n{MANAGED_UPDATE_OVERRIDE}\n--\nliteral\n")
    );
}

#[test]
fn active_process_exit_status_is_propagated() {
    let fixture = DispatcherFixture::new("error", "error");
    let output = fixture.run(["exec", "task"], |command| {
        command.env("CODEX_PATCHER_TEST_EXIT", "37");
    });
    assert_eq!(output.status.code(), Some(37));
}

#[test]
fn detached_probe_work_never_blocks_the_active_launch() {
    let fixture = DispatcherFixture::new("error", "error");
    let marker = fixture.root.join("slow-probe-finished");
    let started = Instant::now();
    let output = fixture.run(["app-server"], |command| {
        command
            .env("CODEX_PATCHER_TEST_PROBE_SLEEP_MS", "3000")
            .env("CODEX_PATCHER_TEST_PROBE_MARKER", &marker)
            .env("CODEX_PATCHER_TEST_STDOUT", "ready\n");
    });
    let launch_time = started.elapsed();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"ready\n");
    assert!(
        launch_time < Duration::from_secs(2),
        "launch waited {launch_time:?} for its probe"
    );
    wait_for_file(&marker);
}

#[test]
fn probe_spawn_failure_is_logged_but_does_not_block_launch() {
    let fixture = DispatcherFixture::new("error", "error");
    fs::remove_file(&fixture.paths.manager).expect("remove fixture manager");
    let output = fixture.run(["app-server"], |command| {
        command.env("CODEX_PATCHER_TEST_STDOUT", "still-running\n");
    });

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"still-running\n");
    assert!(output.stderr.is_empty());
    let runtime_log =
        fs::read_to_string(fixture.paths.logs_dir().join("runtime.log")).expect("read runtime log");
    assert!(runtime_log.contains("could not spawn freshness probe"));
}

#[test]
fn noninteractive_error_policy_refuses_pending_generation() {
    let fixture = DispatcherFixture::new("last-good", "error");
    fixture.save_state(&fixture.pending_state("confirmed update pending"));
    let runs = fixture.root.join("runs");
    let output = fixture.run(["exec", "task"], |command| {
        command.env("CODEX_PATCHER_TEST_RUN_COUNT", &runs);
    });

    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("confirmed update pending"));
    assert_eq!(line_count(&runs), 0);
}

#[test]
fn noninteractive_failure_prints_log_and_repair_command_without_protocol_output() {
    let fixture = DispatcherFixture::new("error", "error");
    let mut state = fixture.pending_state("patch 2 no longer applies");
    let log_path = fixture.paths.logs_dir().join("failed-build.log");
    state.failure = Some(FailureRecord {
        id: "failure-123".to_owned(),
        desired: state.probe.desired.clone().unwrap(),
        phase: "patch".to_owned(),
        summary: "patch 2 no longer applies".to_owned(),
        failed_patch_index: Some(2),
        failed_patch: Some("feature.patch".to_owned()),
        log_path: log_path.clone(),
        created_at: Utc::now(),
        repair_worktree: None,
    });
    state.probe.kind = ProbeKind::Failed;
    fixture.save_state(&state);

    let output = fixture.run(["app-server"], |_| {});
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("patch 2 no longer applies"));
    assert!(stderr.contains(&log_path.display().to_string()));
    assert!(stderr.contains("codex-patcher repair failure-123"));
}

#[test]
fn noninteractive_warn_run_policy_runs_validated_last_good() {
    let fixture = DispatcherFixture::new("error", "warn-run");
    fixture.save_state(&fixture.pending_state("confirmed update pending"));
    let runs = fixture.root.join("runs");
    let output = fixture.run(["app-server"], |command| {
        command
            .env("CODEX_PATCHER_TEST_RUN_COUNT", &runs)
            .env("CODEX_PATCHER_TEST_STDOUT", "service-output\n");
    });

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"service-output\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("running last-good patched Codex"));
    assert_eq!(line_count(&runs), 1);
}

#[test]
fn noninteractive_auto_policy_derives_from_failure_mode() {
    for (failure_mode, expected_status, expected_runs) in [("error", 75, 0), ("last-good", 0, 1)] {
        let fixture = DispatcherFixture::new(failure_mode, "auto");
        fixture.save_state(&fixture.pending_state("confirmed update pending"));
        let runs = fixture.root.join("runs");
        let output = fixture.run(["exec", "task"], |command| {
            command.env("CODEX_PATCHER_TEST_RUN_COUNT", &runs);
        });
        assert_eq!(
            output.status.code(),
            Some(expected_status),
            "{failure_mode}"
        );
        assert_eq!(line_count(&runs), expected_runs, "{failure_mode}");
    }
}

#[test]
fn probe_lock_coalesces_concurrent_probes() {
    let fixture = DispatcherFixture::new("error", "error");
    let store = StateStore::new(fixture.paths.clone());
    let first = store
        .try_probe_lock()
        .expect("acquire first probe lock")
        .expect("first probe owns lock");
    assert!(
        store
            .try_probe_lock()
            .expect("attempt coalesced probe lock")
            .is_none()
    );
    drop(first);
    assert!(
        store
            .try_probe_lock()
            .expect("reacquire released probe lock")
            .is_some()
    );
}

#[test]
fn managed_override_preserves_all_caller_arguments() {
    let input: Vec<OsString> = ["-c", "x=y", "exec", "--", "-literal"]
        .into_iter()
        .map(OsString::from)
        .collect();
    let output = inject_managed_update_override(&input);
    let expected: Vec<OsString> = [
        "-c",
        "x=y",
        "exec",
        "-c",
        MANAGED_UPDATE_OVERRIDE,
        "--",
        "-literal",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    assert_eq!(output, expected);
}
