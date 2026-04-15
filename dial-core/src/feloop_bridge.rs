use crate::config::config_get;
use crate::db::{get_current_phase, get_db, get_db_path, get_dial_dir};
use crate::engine::PipelineStepConfig;
use crate::errors::{DialError, Result};
use crate::task::models::Task;
use chrono::Local;
use rusqlite::Connection;
use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FeloopDatabaseOwnership {
    Owned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FeloopDatabaseConfig {
    path: PathBuf,
    ownership: FeloopDatabaseOwnership,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FeloopCommandValidationStep {
    name: String,
    command: String,
    required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FeloopPolicy {
    max_attempts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u64>,
    create_checkpoint: bool,
    restore_checkpoint_on_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FeloopConfigSnapshot {
    policy: FeloopPolicy,
    database: Option<FeloopDatabaseConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    command_validation_steps: Vec<FeloopCommandValidationStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FeloopTask {
    id: String,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FeloopWorkflowFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<FeloopConfigSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<FeloopTask>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TaskSelectionSource {
    Explicit,
    CurrentIteration,
    NextPending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeloopValidationLane {
    pub lane_root: PathBuf,
    pub project_root: PathBuf,
    pub phase: String,
    pub live_dial_dir: PathBuf,
    pub isolated_dial_dir: PathBuf,
    pub live_db_path: PathBuf,
    pub isolated_db_path: PathBuf,
    pub workflow_path: PathBuf,
    pub metadata_path: PathBuf,
    pub task_id: i64,
    pub task_description: String,
    pub task_source: String,
    pub validation_steps: Vec<FeloopValidationOutput>,
    pub protects_live_dial_state: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeloopValidationOutput {
    pub name: String,
    pub command: String,
    pub required: bool,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeloopExecutionPreference {
    pub feloop_bin: Option<PathBuf>,
    pub feloop_manifest: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeloopReplayReport {
    pub lane: FeloopValidationLane,
    pub executor: String,
    pub inspect: FeloopCommandReport,
    pub validate: FeloopCommandReport,
    pub replay: FeloopCommandReport,
    pub result_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeloopCommandReport {
    pub command: Vec<String>,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub status: i32,
}

#[derive(Debug, Clone)]
struct PreparedTask {
    task: Task,
    source: TaskSelectionSource,
}

#[derive(Debug, Clone)]
struct FeloopExecutor {
    label: String,
    program: String,
    base_args: Vec<String>,
}

pub fn prepare_feloop_validation_lane(
    task_id: Option<i64>,
    lane_root: Option<&Path>,
) -> Result<FeloopValidationLane> {
    let project_root = env::current_dir()?;
    let live_dial_dir = get_dial_dir();
    if !live_dial_dir.is_dir() {
        return Err(DialError::NotInitialized);
    }

    let phase = get_current_phase()?;
    let live_db_path = get_db_path(Some(&phase));
    let conn = get_db(Some(&phase))?;
    let prepared_task = resolve_task(&conn, task_id)?;
    let validation_steps = resolve_validation_steps(&conn)?;
    if validation_steps.is_empty() {
        return Err(DialError::UserError(
            "No validation pipeline is configured. Set validation steps or build/test commands before using the FeLoop lane.".to_string(),
        ));
    }

    let lane_root = resolve_lane_root(&project_root, &live_dial_dir, lane_root)?;
    let isolated_dial_dir = lane_root.join(".dial");
    copy_dir_recursive(&live_dial_dir, &isolated_dial_dir)?;
    let isolated_db_path = isolated_dial_dir.join("feloop-validation.db");
    let workflow_path = lane_root.join("workflow.toml");
    let metadata_path = lane_root.join("lane.json");

    let workflow = FeloopWorkflowFile {
        name: Some(format!("dial-feloop-task-{}", prepared_task.task.id)),
        description: Some(format!(
            "Safe FeLoop validation lane for DIAL task #{}",
            prepared_task.task.id
        )),
        config: Some(FeloopConfigSnapshot {
            policy: FeloopPolicy {
                max_attempts: 1,
                timeout: None,
                create_checkpoint: false,
                restore_checkpoint_on_failure: false,
            },
            database: Some(FeloopDatabaseConfig {
                path: isolated_db_path.clone(),
                ownership: FeloopDatabaseOwnership::Owned,
            }),
            command_validation_steps: validation_steps
                .iter()
                .map(|step| FeloopCommandValidationStep {
                    name: step.name.clone(),
                    command: step.command.clone(),
                    required: step.required,
                    timeout_secs: step.timeout_secs,
                })
                .collect(),
        }),
        task: Some(FeloopTask {
            id: format!("dial-task-{}", prepared_task.task.id),
            summary: prepared_task.task.description.clone(),
            details: Some(render_task_details(&prepared_task.task, &phase)),
        }),
    };

    let workflow_contents = toml::to_string_pretty(&workflow).map_err(|error| {
        DialError::UserError(format!("failed to write FeLoop workflow: {error}"))
    })?;
    fs::write(&workflow_path, workflow_contents)?;

    let lane = FeloopValidationLane {
        lane_root,
        project_root,
        phase,
        live_dial_dir,
        isolated_dial_dir,
        live_db_path,
        isolated_db_path,
        workflow_path,
        metadata_path,
        task_id: prepared_task.task.id,
        task_description: prepared_task.task.description,
        task_source: task_source_label(prepared_task.source).to_string(),
        validation_steps: validation_steps
            .into_iter()
            .map(|step| FeloopValidationOutput {
                name: step.name,
                command: step.command,
                required: step.required,
                timeout_secs: step.timeout_secs,
            })
            .collect(),
        protects_live_dial_state: true,
    };

    fs::write(
        &lane.metadata_path,
        serde_json::to_string_pretty(&lane).map_err(|error| {
            DialError::UserError(format!("failed to serialize lane metadata: {error}"))
        })?,
    )?;

    Ok(lane)
}

pub fn replay_feloop_validation_lane(
    task_id: Option<i64>,
    lane_root: Option<&Path>,
    preference: &FeloopExecutionPreference,
) -> Result<FeloopReplayReport> {
    let lane = prepare_feloop_validation_lane(task_id, lane_root)?;
    let executor = resolve_feloop_executor(preference)?;

    let inspect = run_feloop_command(&executor, "inspect", &lane)?;
    let validate = run_feloop_command(&executor, "validate", &lane)?;
    let replay = run_feloop_command(&executor, "replay", &lane)?;

    let report = FeloopReplayReport {
        lane,
        executor: executor.label,
        inspect,
        validate,
        replay,
        result_path: PathBuf::new(),
    };

    let result_path = report.lane.lane_root.join("result.json");
    let mut finalized = report;
    finalized.result_path = result_path.clone();
    fs::write(
        &result_path,
        serde_json::to_string_pretty(&finalized).map_err(|error| {
            DialError::UserError(format!("failed to serialize replay report: {error}"))
        })?,
    )?;
    Ok(finalized)
}

fn resolve_task(conn: &Connection, task_id: Option<i64>) -> Result<PreparedTask> {
    if let Some(task_id) = task_id {
        return Ok(PreparedTask {
            task: get_task_by_id(conn, task_id)?,
            source: TaskSelectionSource::Explicit,
        });
    }

    if let Some(task) = current_iteration_task(conn)? {
        return Ok(PreparedTask {
            task,
            source: TaskSelectionSource::CurrentIteration,
        });
    }

    if let Some(task) = next_pending_task(conn)? {
        return Ok(PreparedTask {
            task,
            source: TaskSelectionSource::NextPending,
        });
    }

    Err(DialError::NoPendingTasks)
}

fn get_task_by_id(conn: &Connection, task_id: i64) -> Result<Task> {
    let mut stmt = conn.prepare(
        "SELECT id, description, status, priority, blocked_by, spec_section_id, created_at, started_at, completed_at,
                prd_section_id, total_attempts, total_failures, last_failure_at,
                acceptance_criteria_json, requires_browser_verification
         FROM tasks WHERE id = ?1",
    )?;

    stmt.query_row([task_id], Task::from_row)
        .map_err(|_| DialError::TaskNotFound(task_id))
}

fn current_iteration_task(conn: &Connection) -> Result<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.description, t.status, t.priority, t.blocked_by, t.spec_section_id, t.created_at, t.started_at, t.completed_at,
                t.prd_section_id, t.total_attempts, t.total_failures, t.last_failure_at,
                t.acceptance_criteria_json, t.requires_browser_verification
         FROM iterations i
         INNER JOIN tasks t ON t.id = i.task_id
         WHERE i.status IN ('in_progress', 'awaiting_approval')
         ORDER BY i.id DESC LIMIT 1",
    )?;

    Ok(stmt.query_row([], Task::from_row).ok())
}

fn next_pending_task(conn: &Connection) -> Result<Option<Task>> {
    let mut stmt = conn.prepare(
        "SELECT id, description, status, priority, blocked_by, spec_section_id, created_at, started_at, completed_at,
                prd_section_id, total_attempts, total_failures, last_failure_at,
                acceptance_criteria_json, requires_browser_verification
         FROM tasks WHERE status = 'pending'
         AND id NOT IN (
             SELECT td.task_id FROM task_dependencies td
             INNER JOIN tasks dep ON dep.id = td.depends_on_id
             WHERE dep.status != 'completed'
         )
         ORDER BY priority, id LIMIT 1",
    )?;

    Ok(stmt.query_row([], Task::from_row).ok())
}

fn resolve_validation_steps(conn: &Connection) -> Result<Vec<PipelineStepConfig>> {
    let table_exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='validation_steps'",
        [],
        |row| row.get(0),
    )?;

    if table_exists {
        let mut stmt = conn.prepare(
            "SELECT id, name, command, sort_order, required, timeout_secs
             FROM validation_steps ORDER BY sort_order, id",
        )?;

        let steps: Vec<PipelineStepConfig> = stmt
            .query_map([], |row| {
                Ok(PipelineStepConfig {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    command: row.get(2)?,
                    sort_order: row.get(3)?,
                    required: row.get::<_, i64>(4)? != 0,
                    timeout_secs: row.get::<_, Option<i64>>(5)?.map(|timeout| timeout as u64),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if !steps.is_empty() {
            return Ok(steps);
        }
    }

    let build_cmd = config_get("build_cmd")?.unwrap_or_default();
    let test_cmd = config_get("test_cmd")?.unwrap_or_default();
    let build_timeout = config_get("build_timeout")?
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(crate::DEFAULT_TIMEOUT_SECS);
    let test_timeout = config_get("test_timeout")?
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(crate::DEFAULT_TIMEOUT_SECS);

    let mut steps = Vec::new();
    if !build_cmd.trim().is_empty() {
        steps.push(PipelineStepConfig {
            id: 1,
            name: String::from("build"),
            command: build_cmd,
            sort_order: 0,
            required: true,
            timeout_secs: Some(build_timeout),
        });
    }
    if !test_cmd.trim().is_empty() {
        steps.push(PipelineStepConfig {
            id: 2,
            name: String::from("test"),
            command: test_cmd,
            sort_order: 1,
            required: true,
            timeout_secs: Some(test_timeout),
        });
    }
    Ok(steps)
}

fn resolve_lane_root(
    project_root: &Path,
    live_dial_dir: &Path,
    requested: Option<&Path>,
) -> Result<PathBuf> {
    let lane_root = match requested {
        Some(path) => absolutize_path(path, project_root),
        None => {
            let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
            env::temp_dir().join(format!(
                "dial-feloop-validation-{}-{}",
                timestamp,
                std::process::id()
            ))
        }
    };

    if lane_root.starts_with(live_dial_dir) || live_dial_dir.starts_with(&lane_root) {
        return Err(DialError::UserError(format!(
            "FeLoop lane root {} must stay outside the live .dial directory",
            lane_root.display()
        )));
    }

    if lane_root.exists() {
        let mut entries = fs::read_dir(&lane_root)?;
        if entries.next().is_some() {
            return Err(DialError::UserError(format!(
                "FeLoop lane root {} already exists and is not empty",
                lane_root.display()
            )));
        }
    } else {
        fs::create_dir_all(&lane_root)?;
    }

    Ok(lane_root)
}

fn absolutize_path(path: &Path, project_root: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source) {
        let entry = entry.map_err(|error| DialError::UserError(error.to_string()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(source)
            .map_err(|error| DialError::UserError(error.to_string()))?;
        let target = destination.join(relative);

        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target)?;
        }
    }
    Ok(())
}

fn render_task_details(task: &Task, phase: &str) -> String {
    let mut lines = vec![
        format!("source=dial"),
        format!("phase={phase}"),
        format!("task_id={}", task.id),
        format!("status={}", task.status),
        format!("priority={}", task.priority),
    ];

    if let Some(prd_section_id) = &task.prd_section_id {
        lines.push(format!("prd_section_id={prd_section_id}"));
    }
    if let Some(spec_section_id) = task.spec_section_id {
        lines.push(format!("spec_section_id={spec_section_id}"));
    }
    if !task.acceptance_criteria.is_empty() {
        lines.push(String::from("acceptance_criteria:"));
        for criterion in &task.acceptance_criteria {
            lines.push(format!("- {criterion}"));
        }
    }
    if task.requires_browser_verification {
        lines.push(String::from("requires_browser_verification=true"));
    }

    lines.join("\n")
}

fn task_source_label(source: TaskSelectionSource) -> &'static str {
    match source {
        TaskSelectionSource::Explicit => "explicit",
        TaskSelectionSource::CurrentIteration => "current_iteration",
        TaskSelectionSource::NextPending => "next_pending",
    }
}

fn resolve_feloop_executor(preference: &FeloopExecutionPreference) -> Result<FeloopExecutor> {
    if let Some(path) = preference.feloop_bin.as_ref() {
        return Ok(FeloopExecutor {
            label: format!("binary:{}", path.display()),
            program: path.to_string_lossy().to_string(),
            base_args: Vec::new(),
        });
    }

    if let Some(path) = preference.feloop_manifest.as_ref() {
        return Ok(FeloopExecutor {
            label: format!("cargo-manifest:{}", path.display()),
            program: String::from("cargo"),
            base_args: vec![
                String::from("run"),
                String::from("--manifest-path"),
                path.to_string_lossy().to_string(),
                String::from("--bin"),
                String::from("feloop"),
                String::from("--"),
            ],
        });
    }

    if let Some(path) = config_get("feloop_bin")? {
        return Ok(FeloopExecutor {
            label: format!("binary:{path}"),
            program: path,
            base_args: Vec::new(),
        });
    }

    if let Some(path) = find_on_path("feloop") {
        return Ok(FeloopExecutor {
            label: format!("binary:{}", path.display()),
            program: path.to_string_lossy().to_string(),
            base_args: Vec::new(),
        });
    }

    if let Some(path) = config_get("feloop_manifest")? {
        return Ok(FeloopExecutor {
            label: format!("cargo-manifest:{path}"),
            program: String::from("cargo"),
            base_args: vec![
                String::from("run"),
                String::from("--manifest-path"),
                path,
                String::from("--bin"),
                String::from("feloop"),
                String::from("--"),
            ],
        });
    }

    Err(DialError::UserError(
        "No FeLoop runtime was found. Pass --feloop-bin, pass --feloop-manifest, or set the feloop_bin/feloop_manifest config key.".to_string(),
    ))
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }

            #[cfg(windows)]
            {
                let candidate = dir.join(format!("{binary}.exe"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }

            None
        })
    })
}

fn run_feloop_command(
    executor: &FeloopExecutor,
    subcommand: &str,
    lane: &FeloopValidationLane,
) -> Result<FeloopCommandReport> {
    let mut args = executor.base_args.clone();
    args.push(subcommand.to_string());
    args.push(String::from("--workflow"));
    args.push(lane.workflow_path.to_string_lossy().to_string());

    let output = Command::new(&executor.program)
        .args(&args)
        .current_dir(&lane.project_root)
        .output()
        .map_err(|error| {
            DialError::CommandFailed(format!(
                "unable to run {} {}: {}",
                executor.program, subcommand, error
            ))
        })?;

    let stdout_path = lane
        .lane_root
        .join(format!("feloop-{}.stdout.log", subcommand));
    let stderr_path = lane
        .lane_root
        .join(format!("feloop-{}.stderr.log", subcommand));
    fs::write(&stdout_path, &output.stdout)?;
    fs::write(&stderr_path, &output.stderr)?;

    if !output.status.success() {
        let error_output = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).to_string()
        } else {
            String::from_utf8_lossy(&output.stderr).to_string()
        };
        return Err(DialError::CommandFailed(format!(
            "FeLoop {} failed. See {} and {}. {}",
            subcommand,
            stdout_path.display(),
            stderr_path.display(),
            error_output.trim()
        )));
    }

    Ok(FeloopCommandReport {
        command: std::iter::once(executor.program.clone())
            .chain(args)
            .collect(),
        stdout_path,
        stderr_path,
        status: output.status.code().unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{init_db_with_agents_mode, AgentsMode};
    use crate::task::task_add_with_metadata;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    #[serial_test::serial]
    fn prepare_lane_uses_isolated_dial_state_and_workflow_snapshot() {
        let tempdir = tempdir().expect("tempdir should exist");
        let project_root = tempdir.path();
        let _guard = CwdGuard::change_to(project_root);
        init_db_with_agents_mode("default", None, AgentsMode::Off).expect("dial should init");
        crate::config::config_set("build_cmd", "printf 'build\\n'").expect("build should set");
        crate::config::config_set("test_cmd", "printf 'test\\n'").expect("test should set");
        task_add_with_metadata(
            "Bridge FeLoop validation lane",
            1,
            None,
            &[String::from("lane is exported")],
            false,
        )
        .expect("task should insert");

        let lane = prepare_feloop_validation_lane(None, Some(project_root.join("lane").as_path()))
            .expect("lane should prepare");

        assert!(lane.isolated_dial_dir.is_dir());
        assert!(lane.workflow_path.is_file());
        assert!(lane.metadata_path.is_file());
        assert_eq!(lane.validation_steps.len(), 2);
        let workflow = fs::read_to_string(&lane.workflow_path).expect("workflow should exist");
        assert!(workflow.contains("command_validation_steps"));
        assert!(workflow.contains("printf 'build"));
        assert!(workflow.contains("dial-task-1"));
        assert!(lane.protects_live_dial_state);
    }

    #[test]
    #[serial_test::serial]
    fn replay_lane_runs_explicit_executor_without_touching_live_dial() {
        let tempdir = tempdir().expect("tempdir should exist");
        let project_root = tempdir.path();
        let _guard = CwdGuard::change_to(project_root);
        init_db_with_agents_mode("default", None, AgentsMode::Off).expect("dial should init");
        crate::config::config_set("build_cmd", "printf 'build\\n'").expect("build should set");
        task_add_with_metadata("Replay through FeLoop", 1, None, &[], false)
            .expect("task should insert");

        let stub = write_stub_feloop(project_root).expect("stub should be written");
        let report = replay_feloop_validation_lane(
            None,
            Some(project_root.join("lane").as_path()),
            &FeloopExecutionPreference {
                feloop_bin: Some(stub),
                feloop_manifest: None,
            },
        )
        .expect("replay should succeed");

        assert!(report.inspect.stdout_path.is_file());
        assert!(report.validate.stdout_path.is_file());
        assert!(report.replay.stdout_path.is_file());
        assert!(report.result_path.is_file());
        let log = fs::read_to_string(report.replay.stdout_path).expect("stdout should exist");
        assert!(log.contains("replay ok"));
    }

    fn write_stub_feloop(project_root: &Path) -> Result<PathBuf> {
        #[cfg(windows)]
        {
            let script_path = project_root.join("stub-feloop.cmd");
            fs::write(
                &script_path,
                "@echo off\r\nif \"%1\"==\"inspect\" echo inspect ok & exit /b 0\r\nif \"%1\"==\"validate\" echo validate ok & exit /b 0\r\nif \"%1\"==\"replay\" echo replay ok & exit /b 0\r\necho bad command %1\r\nexit /b 1\r\n",
            )?;
            Ok(script_path)
        }

        #[cfg(not(windows))]
        {
            let script_path = project_root.join("stub-feloop.sh");
            let mut file = fs::File::create(&script_path)?;
            writeln!(file, "#!/bin/sh")?;
            writeln!(file, "case \"$1\" in")?;
            writeln!(file, "  inspect) echo inspect ok ;;")?;
            writeln!(file, "  validate) echo validate ok ;;")?;
            writeln!(file, "  replay) echo replay ok ;;")?;
            writeln!(file, "  *) echo bad command \"$1\" >&2; exit 1 ;;")?;
            writeln!(file, "esac")?;
            let mut permissions = fs::metadata(&script_path)?.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(0o755);
            }
            fs::set_permissions(&script_path, permissions)?;
            Ok(script_path)
        }
    }

    struct CwdGuard(PathBuf);

    impl CwdGuard {
        fn change_to(path: &Path) -> Self {
            let original = env::current_dir().expect("cwd should exist");
            env::set_current_dir(path).expect("cwd should change");
            Self(original)
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.0);
        }
    }
}
