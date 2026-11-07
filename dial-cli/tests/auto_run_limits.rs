#![cfg(unix)]
// These tests launch only a local fake worker; no model, server, or viewer runs.
use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    path: std::ffi::OsString,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let worker = bin.join("opencode");
        let body = match mode {
            "permission" => {
                r#"
cat > .dial/signal.json <<'SIGNAL'
{"signals":[{"type":"complete","summary":"must not be accepted"}]}
SIGNAL
printf '%s\n' '{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{"status":"error","input":{"command":"cd /tmp && node /repo/verify.mjs"},"error":"The user rejected permission to use this specific tool call."}}}'
"#
            }
            "complete" => r#"printf 'DIAL_COMPLETE: synthetic task finished\n'"#,
            "retry-success" => {
                r#"if [ "$(wc -l < calls)" -gt 1 ]; then printf 'DIAL_COMPLETE: recovered synthetic task\n'; fi"#
            }
            _ => "printf 'worker exited without a signal\n'",
        };
        fs::write(
            &worker,
            format!("#!/bin/sh\nprintf 'attempt\n' >> calls\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let fixture = Self {
            dir,
            path: std::env::join_paths(paths).unwrap(),
        };
        fixture
            .cmd()
            .args(["init", "--phase", "test", "--agents", "off"])
            .assert()
            .success();
        for (key, value) in [
            ("opencode_live", "false"),
            ("build_cmd", "true"),
            ("test_cmd", "true"),
        ] {
            fixture
                .cmd()
                .args(["config", "set", key, value])
                .assert()
                .success();
        }
        for name in ["first synthetic task", "second synthetic task"] {
            fixture.cmd().args(["task", "add", name]).assert().success();
        }
        fixture
    }
    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("dial").unwrap();
        cmd.current_dir(self.dir.path()).env("PATH", &self.path);
        cmd
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.path().join("calls"))
            .unwrap()
            .lines()
            .count()
    }
}

#[test]
fn unicode_validation_output_finalizes_success_and_failure() {
    for succeeds in [true, false] {
        let f = Fixture::new("complete");
        // Both preview limits fall inside a checkmark, as in Node test output.
        let output = format!("{}✔{}✔ done\n", "x".repeat(498), "y".repeat(497));
        fs::write(f.dir.path().join("verification.txt"), output).unwrap();
        let command = if succeeds {
            "cat verification.txt"
        } else {
            "cat verification.txt; exit 1"
        };
        f.cmd()
            .args(["config", "set", "test_cmd", command])
            .assert()
            .success();
        let result = f
            .cmd()
            .args([
                "auto-run",
                "--cli",
                "opencode",
                "--max",
                "1",
                "--max-attempts",
                "1",
            ])
            .assert();
        if succeeds {
            result.success();
        } else {
            result
                .failure()
                .stderr(predicates::str::contains("Stopped after 1 worker attempt"));
        }
        f.cmd()
            .args(["task", "show", "1"])
            .assert()
            .stdout(predicates::str::contains(if succeeds {
                "Status: completed"
            } else {
                "Status: pending"
            }));
        f.cmd()
            .args(["status"])
            .assert()
            .stdout(predicates::str::contains("No iteration in progress."));
    }
}

#[test]
fn no_signal_stops_after_one_attempt_and_preserves_pending_task() {
    let f = Fixture::new("no-signal");
    f.cmd()
        .args([
            "auto-run",
            "--cli",
            "opencode",
            "--max",
            "1",
            "--max-attempts",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Stopped after 1 worker attempt"));
    assert_eq!(f.calls(), 1);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: pending"));
}

#[test]
fn attempt_limit_is_independent_of_completed_task_limit() {
    let f = Fixture::new("no-signal");
    f.cmd()
        .args([
            "auto-run",
            "--cli",
            "opencode",
            "--max",
            "1",
            "--max-attempts",
            "2",
        ])
        .assert()
        .failure();
    assert_eq!(f.calls(), 2);
}

#[test]
fn validation_failure_does_not_dispatch_another_attempt() {
    let f = Fixture::new("complete");
    f.cmd()
        .args(["config", "set", "test_cmd", "false"])
        .assert()
        .success();
    f.cmd()
        .args(["auto-run", "--cli", "opencode", "--max-attempts", "1"])
        .assert()
        .failure();
    assert_eq!(f.calls(), 1);
}

#[test]
fn completed_task_limit_still_stops_successful_work() {
    let f = Fixture::new("complete");
    f.cmd()
        .args([
            "auto-run",
            "--cli",
            "opencode",
            "--max",
            "1",
            "--max-attempts",
            "2",
        ])
        .assert()
        .success();
    assert_eq!(f.calls(), 1);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: completed"));
    f.cmd()
        .args(["task", "show", "2"])
        .assert()
        .stdout(predicates::str::contains("Status: pending"));
}

#[test]
fn permission_rejection_blocks_first_task_and_never_dispatches_second() {
    let f = Fixture::new("permission");
    // No attempt cap: permission rejection itself must stop the loop.
    f.cmd()
        .args(["auto-run", "--cli", "opencode"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "OpenCode permission rejected for bash",
        ));
    assert_eq!(f.calls(), 1);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: blocked"));
    f.cmd()
        .args(["task", "show", "2"])
        .assert()
        .stdout(predicates::str::contains("Status: pending"));
    assert!(!f.dir.path().join(".dial/signal.json").exists());
    f.cmd()
        .args(["history"])
        .assert()
        .stdout(predicates::str::contains("failed"));
}

#[test]
fn zero_attempt_limit_is_rejected_before_dispatch() {
    let f = Fixture::new("complete");
    f.cmd()
        .args(["auto-run", "--max-attempts", "0"])
        .assert()
        .failure();
    assert!(!f.dir.path().join("calls").exists());
}

#[test]
fn live_permission_rejection_stops_a_quiet_worker_without_waiting_for_exit() {
    let f = Fixture::new("no-signal");
    let bin = f.dir.path().join("bin");
    // Fake the documented local OpenCode endpoints and viewer. The worker
    // emits no output and would wait 30s without the permission polling path.
    let scripts = [
        (
            "opencode",
            r#"#!/bin/sh
if [ "$1" = run ]; then printf 'attempt\n' >> calls; fi
exec python3 -c 'import time; time.sleep(30)'
"#,
        ),
        ("konsole", "#!/bin/sh\nexit 0\n"),
        (
            "curl",
            r#"#!/bin/sh
for arg do endpoint="$arg"; done
case "$endpoint" in
  */global/health)
    if [ ! -f health-seen ]; then touch health-seen; exit 22; fi
    printf '{"healthy":true}' ;;
  */session\?*) printf '{"id":"ses_fake"}' ;;
  */session/ses_fake/message\?*)
    printf '%s' '[{"parts":[{"type":"tool","tool":"bash","state":{"status":"error","input":{"command":"cd /tmp && node /repo/verify.mjs"},"error":"The user rejected permission to use this specific tool call."}}]}]' ;;
  *) exit 23 ;;
esac
"#,
        ),
    ];
    for (name, body) in scripts {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.cmd()
        .args(["config", "set", "opencode_live", "true"])
        .assert()
        .success();
    let start = std::time::Instant::now();
    f.cmd()
        .timeout(std::time::Duration::from_secs(10))
        .args(["auto-run", "--cli", "opencode"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "OpenCode permission rejected for bash",
        ));
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(f.calls(), 1);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: blocked"));
}

#[cfg(target_os = "linux")]
#[test]
fn live_viewer_closes_after_completion_blocker_and_timeout() {
    for mode in ["complete", "blocked", "timeout"] {
        let f = Fixture::new("no-signal");
        let bin = f.dir.path().join("bin");
        let worker = match mode {
            "complete" => "printf 'DIAL_COMPLETE: synthetic task finished\\n'",
            "blocked" => "printf 'DIAL_BLOCKED: synthetic blocker\\n'",
            _ => "exec sleep 30",
        };
        let scripts = [
            ("opencode", format!("#!/bin/sh\nif [ \"$1\" = serve ]; then exec sleep 30; fi\nwhile [ ! -f viewer.pid ]; do sleep 0.01; done\n{worker}\n")),
            ("konsole", "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nprintf '%s' $$ > viewer.pid\nexec sleep 30\n".to_owned()),
            ("curl", r#"#!/bin/sh
for arg do endpoint="$arg"; done
case "$endpoint" in
  */global/health) if [ ! -f health-seen ]; then touch health-seen; exit 22; fi; printf '{"healthy":true}' ;;
  */session\?*) printf '{"id":"ses_fake"}' ;;
  */session/ses_fake/message\?*) printf '[]' ;;
  *) exit 23 ;;
esac
"#.to_owned()),
        ];
        for (name, body) in scripts {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for (key, value) in [
            ("opencode_live", "true"),
            ("subagent_timeout", "1"),
            // A successful task must retain its viewer throughout validation.
            (
                "build_cmd",
                "test -f viewer.pid && kill -0 $(cat viewer.pid)",
            ),
        ] {
            f.cmd()
                .args(["config", "set", key, value])
                .assert()
                .success();
        }
        let result = f
            .cmd()
            .timeout(std::time::Duration::from_secs(10))
            .args([
                "auto-run",
                "--cli",
                "opencode",
                "--max",
                "1",
                "--max-attempts",
                "1",
            ])
            .assert();
        if mode == "complete" {
            result.success();
        } else {
            result.failure();
        }
        let pid: u32 = fs::read_to_string(f.dir.path().join("viewer.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "viewer survived {mode}"
        );
        f.cmd()
            .args(["task", "show", "1"])
            .assert()
            .stdout(predicates::str::contains(if mode == "complete" {
                "Status: completed"
            } else {
                "Status: blocked"
            }));
    }
}

#[test]
fn successful_retry_at_attempt_limit_returns_success() {
    let f = Fixture::new("retry-success");
    f.cmd()
        .args(["auto-run", "--cli", "opencode", "--max-attempts", "2"])
        .assert()
        .success();
    assert_eq!(f.calls(), 2);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: completed"));
}

#[test]
fn reviewed_task_can_run_a_fourth_attempt_without_erasing_failures() {
    let f = Fixture::new("no-signal");
    f.cmd()
        .args(["task", "depend", "2", "1"])
        .assert()
        .success();
    for _ in 0..3 {
        f.cmd()
            .args(["auto-run", "--cli", "opencode", "--max-attempts", "1"])
            .assert()
            .failure();
    }
    assert_eq!(f.calls(), 3);
    f.cmd()
        .args(["config", "set", "task_1_max_fix_attempts", "4"])
        .assert()
        .success();
    fs::write(f.dir.path().join("bin/opencode"),
        "#!/bin/sh\nprintf 'attempt\n' >> calls\nprintf 'DIAL_COMPLETE: reviewed task finished\n'\n").unwrap();
    f.cmd()
        .args([
            "auto-run",
            "--cli",
            "opencode",
            "--max",
            "1",
            "--max-attempts",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Attempt 4 of 4"));
    assert_eq!(f.calls(), 4);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: completed"))
        .stdout(predicates::str::contains("Attempts: 4"))
        .stdout(predicates::str::contains("Failures: 3"));
}

#[test]
fn task_attempt_override_stops_at_its_ceiling() {
    let f = Fixture::new("no-signal");
    f.cmd()
        .args(["task", "depend", "2", "1"])
        .assert()
        .success();
    f.cmd()
        .args(["config", "set", "task_1_max_fix_attempts", "4"])
        .assert()
        .success();
    for _ in 0..4 {
        f.cmd()
            .args(["auto-run", "--cli", "opencode", "--max-attempts", "1"])
            .assert()
            .failure();
    }
    f.cmd()
        .args(["auto-run", "--cli", "opencode", "--max-attempts", "1"])
        .assert()
        .stdout(predicates::str::contains("has failed 4 times"));
    assert_eq!(f.calls(), 4);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: blocked"))
        .stdout(predicates::str::contains("Failures: 4"));
}

#[test]
fn pinned_worker_never_runs_another_task() {
    let f = Fixture::new("complete");
    f.cmd()
        .args(["config", "set", "execution_task_id", "2"])
        .assert()
        .success();
    f.cmd()
        .args([
            "auto-run",
            "--cli",
            "opencode",
            "--max",
            "2",
            "--max-attempts",
            "2",
        ])
        .assert()
        .success();
    assert_eq!(f.calls(), 1);
    f.cmd()
        .args(["task", "show", "1"])
        .assert()
        .stdout(predicates::str::contains("Status: pending"));
    f.cmd()
        .args(["task", "show", "2"])
        .assert()
        .stdout(predicates::str::contains("Status: completed"));
}

#[test]
fn invalid_pin_fails_closed() {
    let f = Fixture::new("complete");
    for value in ["", "0", "-1", "oops", "9999"] {
        f.cmd()
            .args(["config", "set", "execution_task_id", "--", value])
            .assert()
            .success();
        f.cmd().args(["task", "next"]).assert().failure();
    }
    assert!(!f.dir.path().join("calls").exists());
}

#[test]
fn source_reservation_prevents_sequential_dispatch() {
    let f = Fixture::new("complete");
    fs::write(f.dir.path().join(".dial/batch-reservation.json"), "{}").unwrap();
    f.cmd()
        .args(["auto-run", "--cli", "opencode", "--max-attempts", "1"])
        .assert()
        .failure();
    assert!(!f.dir.path().join("calls").exists());
}

#[test]
fn pinned_dependency_and_dry_run_use_the_same_selection() {
    let f = Fixture::new("complete");
    f.cmd()
        .args(["config", "set", "execution_task_id", "2"])
        .assert()
        .success();
    f.cmd()
        .args(["auto-run", "--dry-run", "--format", "json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("second synthetic task"));
    f.cmd()
        .args(["task", "depend", "2", "1"])
        .assert()
        .success();
    f.cmd()
        .args(["auto-run", "--cli", "opencode", "--max-attempts", "1"])
        .assert()
        .success();
    assert!(!f.dir.path().join("calls").exists());
    f.cmd().args(["auto-run", "--dry-run"]).assert().failure();
}
