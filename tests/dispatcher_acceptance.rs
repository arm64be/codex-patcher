mod common;

use chrono::Utc;
use codex_patcher::dispatch::{MANAGED_UPDATE_OVERRIDE, inject_managed_update_override};
use codex_patcher::probe;
use codex_patcher::types::{FailureRecord, ProbeKind};
use common::{DispatcherFixture, line_count, source_version};
use std::ffi::OsString;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn patch_change_is_observed_by_the_same_launch() {
    let fixture = DispatcherFixture::new("error", "error");
    fixture.set_auto_rebuild(false);
    let desired = fixture.changed_patch_desired();
    let run_count = fixture.root.join("active-runs");
    let output = fixture.run(["app-server"], |command| {
        command.env("CODEX_PATCHER_TEST_RUN_COUNT", &run_count);
    });
    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("a patched update is pending"));
    assert_eq!(line_count(&run_count), 0);
    let state = fixture.state();
    assert!(matches!(state.probe.kind, ProbeKind::Pending));
    assert_eq!(state.probe.desired.unwrap(), desired);
}

#[test]
fn upstream_change_is_observed_by_the_same_launch() {
    let fixture = DispatcherFixture::new("error", "error");
    let source = source_version("1.2.4", '3', '4');
    fixture.publish_upstream_source(source.clone());
    let run_count = fixture.root.join("active-runs");
    let output = fixture.run(["app-server"], |command| {
        command.env("CODEX_PATCHER_TEST_RUN_COUNT", &run_count);
    });

    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert_eq!(line_count(&run_count), 0);
    let state = fixture.state();
    assert!(matches!(state.probe.kind, ProbeKind::Pending));
    assert_eq!(state.probe.desired.unwrap().source, source);
}

#[test]
fn same_version_patch_change_auto_activates_before_launch() {
    let fixture = DispatcherFixture::new("error", "error");
    let desired = fixture.changed_patch_desired();
    fixture.install_prebuilt_generation(&desired);
    let run_count = fixture.root.join("active-runs");
    let output = fixture.run(["app-server"], |command| {
        command
            .env("CODEX_PATCHER_TEST_RUN_COUNT", &run_count)
            .env("CODEX_PATCHER_TEST_STDOUT", "rebuilt-generation\n");
    });

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"rebuilt-generation\n");
    assert_eq!(line_count(&run_count), 1);
    let state = fixture.state();
    assert_eq!(state.active.unwrap().source_key, desired.source_key);
    assert!(matches!(state.probe.kind, ProbeKind::Current));
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
fn cached_synchronous_freshness_check_keeps_launch_fast() {
    let fixture = DispatcherFixture::new("error", "error");
    fs::write(fixture.paths.remote_cache_file(), b"not valid JSON")
        .expect("poison remote cache that a warm launch must not parse");
    let state_before = fs::read(fixture.paths.state_file()).expect("read warm state");

    let started = Instant::now();
    let refreshed = probe::refresh(&fixture.paths).expect("run cached freshness check");
    let refresh_time = started.elapsed();

    assert!(matches!(refreshed.probe.kind, ProbeKind::Current));
    assert_eq!(
        refreshed.active.as_ref().map(|active| &active.source_key),
        Some(&fixture.active.source_key)
    );
    assert!(
        refresh_time < Duration::from_millis(50),
        "cached freshness work took {refresh_time:?}"
    );
    assert_eq!(
        fs::read(fixture.paths.state_file()).expect("reread warm state"),
        state_before,
        "a cached launch should not rewrite durable state"
    );

    let output = fixture.run(["app-server"], |command| {
        command.env("CODEX_PATCHER_TEST_STDOUT", "ready\n");
    });

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"ready\n");
    assert_eq!(
        fs::read(fixture.paths.state_file()).expect("reread post-launch state"),
        state_before,
        "the wrapper should preserve cached durable state"
    );
}

#[test]
fn invalid_local_patch_input_blocks_the_same_launch() {
    let fixture = DispatcherFixture::new("error", "error");
    fs::write(fixture.patch_dir.join("series"), "missing.patch\n")
        .expect("write invalid patch series");
    let output = fixture.run(["app-server"], |command| {
        command.env("CODEX_PATCHER_TEST_STDOUT", "must-not-run\n");
    });

    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("local patch input is invalid"));
}

#[test]
fn noninteractive_error_policy_refuses_pending_generation() {
    let fixture = DispatcherFixture::new("last-good", "error");
    fixture.set_auto_rebuild(false);
    fixture.changed_patch_desired();
    let runs = fixture.root.join("runs");
    let output = fixture.run(["exec", "task"], |command| {
        command.env("CODEX_PATCHER_TEST_RUN_COUNT", &runs);
    });

    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("a patched update is pending"));
    assert_eq!(line_count(&runs), 0);
}

#[test]
fn noninteractive_failure_prints_log_and_retry_command_without_protocol_output() {
    let fixture = DispatcherFixture::new("error", "error");
    fixture.set_auto_rebuild(false);
    let desired = fixture.changed_patch_desired();
    let mut state = fixture.state();
    let log_path = fixture.paths.logs_dir().join("failed-build.log");
    state.failure = Some(FailureRecord {
        id: "failure-123".to_owned(),
        desired: desired.clone(),
        phase: "patch".to_owned(),
        summary: "patch 2 no longer applies".to_owned(),
        failed_patch_index: Some(2),
        failed_patch: Some("feature.patch".to_owned()),
        log_path: log_path.clone(),
        created_at: Utc::now(),
    });
    state.probe.kind = ProbeKind::Failed;
    state.probe.desired = Some(desired);
    state.probe.message = Some("patch 2 no longer applies".to_owned());
    fixture.save_state(&state);

    let output = fixture.run(["app-server"], |_| {});
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(75));
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("patch 2 no longer applies"));
    assert!(stderr.contains(&log_path.display().to_string()));
    assert!(stderr.contains("codex-patcher update --retry"));
}

#[test]
fn noninteractive_warn_run_policy_runs_validated_last_good() {
    let fixture = DispatcherFixture::new("error", "warn-run");
    fixture.set_auto_rebuild(false);
    fixture.changed_patch_desired();
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
        fixture.set_auto_rebuild(false);
        fixture.changed_patch_desired();
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
