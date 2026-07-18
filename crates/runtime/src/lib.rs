//! The container runtime seam (Principle #3, deep module).
//!
//! `veloslet` drives micro-VMs only through the [`ContainerRuntime`] trait, so the
//! Apple Containerization `container` CLI can be swapped for Tart, Linux, or a
//! fake without touching the worker's reconcile logic. Every instance is keyed by
//! its Velos container **uid**, which makes actuation idempotent: reconcile after a
//! crash matches existing instances by uid before launching.
//!
//! Backends today: [`AppleContainer`] (real) and [`FakeRuntime`] (tests). A Linux
//! backend (e.g. via `podman`/`runc` or a `libkrun` micro-VM) is the planned next
//! addition behind this same trait — tracked separately, not in this change.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime command failed: {0}")]
    Command(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("lock poisoned")]
    Lock,
}

/// The runtime-local identifier of a launched instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceId(pub String);

/// What `veloslet` asks the runtime to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    pub uid: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Observed liveness of an instance. There is no "assumed running": an instance
/// the runtime cannot account for simply isn't in `list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceState {
    Running,
    Exited { exit_code: i32 },
}

/// One instance the runtime is tracking, tagged with its Velos uid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub uid: String,
    pub id: InstanceId,
    pub state: InstanceState,
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Launch an instance tagged with `spec.uid`. Idempotent callers check
    /// [`list`](ContainerRuntime::list) first.
    async fn run(&self, spec: &RunSpec) -> Result<InstanceId, RuntimeError>;
    /// Stop the instance tagged with `uid` (no-op if already gone).
    async fn stop(&self, uid: &str) -> Result<(), RuntimeError>;
    /// Remove the instance tagged with `uid` (no-op if already gone).
    async fn remove(&self, uid: &str) -> Result<(), RuntimeError>;
    /// All instances the runtime knows about, by uid.
    async fn list(&self) -> Result<Vec<Instance>, RuntimeError>;
    /// Reported runtime version string (for `WorkerStatus`).
    async fn version(&self) -> Result<String, RuntimeError>;
}

// ---------------------------------------------------------------------------
// FakeRuntime — in-memory, for tests and the e2e harness.
// ---------------------------------------------------------------------------

/// An in-memory runtime used by tests and `velos-tests`. Exit can be simulated
/// with [`FakeRuntime::set_exited`].
#[derive(Default)]
pub struct FakeRuntime {
    instances: Mutex<HashMap<String, Instance>>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate the instance for `uid` exiting with `exit_code`.
    pub fn set_exited(&self, uid: &str, exit_code: i32) -> Result<(), RuntimeError> {
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        if let Some(inst) = g.get_mut(uid) {
            inst.state = InstanceState::Exited { exit_code };
        }
        Ok(())
    }
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    async fn run(&self, spec: &RunSpec) -> Result<InstanceId, RuntimeError> {
        let id = InstanceId(format!("fake-{}", spec.uid));
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        g.insert(
            spec.uid.clone(),
            Instance {
                uid: spec.uid.clone(),
                id: id.clone(),
                state: InstanceState::Running,
            },
        );
        Ok(id)
    }

    async fn stop(&self, uid: &str) -> Result<(), RuntimeError> {
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        if let Some(inst) = g.get_mut(uid) {
            inst.state = InstanceState::Exited { exit_code: 0 };
        }
        Ok(())
    }

    async fn remove(&self, uid: &str) -> Result<(), RuntimeError> {
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        g.remove(uid);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Instance>, RuntimeError> {
        let g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        Ok(g.values().cloned().collect())
    }

    async fn version(&self) -> Result<String, RuntimeError> {
        Ok("fake-runtime/1.0".to_string())
    }
}

// ---------------------------------------------------------------------------
// AppleContainer — wraps the `container` CLI (Apple Containerization).
// ---------------------------------------------------------------------------
//
// Every instance is addressed by a derived **name** `velos-<uid>` (Apple's
// `container` supports `--name` and name-based addressing universally, so this
// avoids depending on label support). All `container` CLI assumptions are
// gathered in the constants below so they can be matched to the installed
// version in one place:
//
//   run     : `container run --detach --name velos-<uid> [--env K=V ...] [--entrypoint <cmd[0]>] <image> [cmd[1..]...]`
//   stop    : `container stop velos-<uid>`
//   remove  : `container delete --force velos-<uid>`
//   list    : `container list --all --format json`
//   version : `container --version`
//
// These match the apple/container 1.0 command reference (`delete` has alias
// `rm`, `list` has alias `ls`). If your installed version differs, this is the
// one place to adjust.

const SUBCMD_RUN: &str = "run";
const SUBCMD_STOP: &str = "stop";
const SUBCMD_REMOVE: &str = "delete";
const SUBCMD_LIST: &str = "list";
/// Prefix applied to a uid to form the runtime instance name.
const NAME_PREFIX: &str = "velos-";

fn instance_name(uid: &str) -> String {
    format!("{NAME_PREFIX}{uid}")
}

/// Build the `container run …` argv for `spec` (pure, so it's unit-testable).
///
/// `spec.command` follows **Kubernetes `command` semantics**: it overrides the
/// image's `ENTRYPOINT` rather than being appended to it (OCI/Docker CMD-append,
/// which is what bare trailing args do). So `command[0]` becomes the
/// `--entrypoint` override and `command[1..]` are the args after the image. When
/// `command` is empty we don't override — the image's own entrypoint/cmd runs.
///
/// Without this, any image carrying an `ENTRYPOINT` would run `ENTRYPOINT +
/// command` instead of `command` (see issue #47).
fn build_run_args(name: &str, spec: &RunSpec) -> Vec<String> {
    let mut args = vec![
        SUBCMD_RUN.to_string(),
        "--detach".to_string(),
        "--name".to_string(),
        name.to_string(),
    ];
    for (k, v) in &spec.env {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }
    match spec.command.split_first() {
        Some((entrypoint, rest)) => {
            args.push("--entrypoint".to_string());
            args.push(entrypoint.clone());
            args.push(spec.image.clone());
            args.extend(rest.iter().cloned());
        }
        None => args.push(spec.image.clone()),
    }
    args
}

/// Real backend: shells out to the `container` CLI via `tokio::process`.
pub struct AppleContainer {
    bin: String,
}

impl Default for AppleContainer {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleContainer {
    pub fn new() -> Self {
        Self {
            bin: "container".to_string(),
        }
    }

    /// Override the CLI binary path (e.g. for an alternate install location).
    pub fn with_binary(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    /// Whether the configured `container` binary is callable. Used by tests and
    /// callers to skip gracefully when Apple Containerization isn't installed.
    pub async fn available(&self) -> bool {
        self.output(&["--version".to_string()]).await.is_ok()
    }

    async fn output(&self, args: &[String]) -> Result<String, RuntimeError> {
        let out = tokio::process::Command::new(&self.bin)
            .args(args)
            .output()
            .await
            .map_err(|e| RuntimeError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(RuntimeError::Command(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Run a command, swallowing failures (used for idempotent stop/remove where
    /// "no such container" is an acceptable outcome).
    async fn output_best_effort(&self, args: &[String]) {
        let _ = self.output(args).await;
    }
}

#[async_trait]
impl ContainerRuntime for AppleContainer {
    async fn run(&self, spec: &RunSpec) -> Result<InstanceId, RuntimeError> {
        let name = instance_name(&spec.uid);
        let args = build_run_args(&name, spec);
        self.output(&args).await?;
        Ok(InstanceId(name))
    }

    async fn stop(&self, uid: &str) -> Result<(), RuntimeError> {
        self.output_best_effort(&[SUBCMD_STOP.to_string(), instance_name(uid)])
            .await;
        Ok(())
    }

    async fn remove(&self, uid: &str) -> Result<(), RuntimeError> {
        self.output_best_effort(&[
            SUBCMD_REMOVE.to_string(),
            "--force".to_string(),
            instance_name(uid),
        ])
        .await;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Instance>, RuntimeError> {
        let raw = self
            .output(&[
                SUBCMD_LIST.to_string(),
                "--all".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ])
            .await?;
        parse_list(&raw)
    }

    async fn version(&self) -> Result<String, RuntimeError> {
        self.output(&["--version".to_string()]).await
    }
}

/// Read the first present string field among `keys`, descending one level into
/// an array's first element if the field is an array (e.g. `names: [..]`).
fn field_str<'a>(entry: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        match entry.get(k) {
            Some(serde_json::Value::String(s)) => return Some(s),
            Some(serde_json::Value::Array(a)) => {
                if let Some(serde_json::Value::String(s)) = a.first() {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read the instance state string. The real Apple `container` CLI nests it as
/// `status.state` (an object, not a flat string), so `field_str` alone misses
/// it; fall back to that shape when the flat lookup comes up empty.
fn status_str(entry: &serde_json::Value) -> &str {
    if let Some(s) = field_str(entry, &["status", "state"]) {
        return s;
    }
    if let Some(s) = entry
        .get("status")
        .and_then(|v| v.get("state"))
        .and_then(|v| v.as_str())
    {
        return s;
    }
    "unknown"
}

/// Parse `container list --format json` into our uid-keyed instances. Entries
/// whose name lacks the `velos-` prefix are ignored (not ours). Field names are
/// matched tolerantly to survive minor CLI schema differences.
fn parse_list(raw: &str) -> Result<Vec<Instance>, RuntimeError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| RuntimeError::Command(e.to_string()))?;
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for entry in arr {
        let Some(name) = field_str(&entry, &["name", "names", "id"]) else {
            continue;
        };
        let Some(uid) = name.strip_prefix(NAME_PREFIX) else {
            continue;
        };
        let status = status_str(&entry);
        let running = status.eq_ignore_ascii_case("running");
        let state = if running {
            InstanceState::Running
        } else {
            let exit_code = entry
                .get("exitCode")
                .or_else(|| entry.get("exit_code"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            InstanceState::Exited { exit_code }
        };
        out.push(Instance {
            uid: uid.to_string(),
            id: InstanceId(name.to_string()),
            state,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn spec(uid: &str) -> RunSpec {
        RunSpec {
            uid: uid.to_string(),
            image: "alpine".to_string(),
            command: vec![],
            env: vec![],
        }
    }

    #[tokio::test]
    async fn fake_runtime_run_list_exit_remove() {
        let rt = FakeRuntime::new();
        rt.run(&spec("u1")).await.unwrap();
        let list = rt.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, InstanceState::Running);

        rt.set_exited("u1", 3).unwrap();
        let list = rt.list().await.unwrap();
        assert_eq!(list[0].state, InstanceState::Exited { exit_code: 3 });

        rt.remove("u1").await.unwrap();
        assert!(rt.list().await.unwrap().is_empty());
    }

    #[test]
    fn run_args_without_command_use_image_entrypoint() {
        // No command → don't override; let the image's own ENTRYPOINT/CMD run.
        let got = build_run_args("velos-u1", &spec("u1"));
        assert_eq!(got, vec!["run", "--detach", "--name", "velos-u1", "alpine"]);
    }

    #[test]
    fn run_args_with_command_override_entrypoint() {
        // k8s `command` semantics: command[0] becomes the entrypoint override,
        // command[1..] are the args after the image. This makes velos schedule
        // an image regardless of whether it carries an ENTRYPOINT.
        let mut s = spec("u1");
        s.command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo HI".to_string(),
        ];
        let got = build_run_args("velos-u1", &s);
        assert_eq!(
            got,
            vec![
                "run",
                "--detach",
                "--name",
                "velos-u1",
                "--entrypoint",
                "/bin/sh",
                "alpine",
                "-c",
                "echo HI"
            ]
        );
    }

    #[test]
    fn run_args_single_element_command_overrides_entrypoint_with_no_trailing_args() {
        let mut s = spec("u1");
        s.command = vec!["sleep-forever".to_string()];
        let got = build_run_args("velos-u1", &s);
        assert_eq!(
            got,
            vec![
                "run",
                "--detach",
                "--name",
                "velos-u1",
                "--entrypoint",
                "sleep-forever",
                "alpine"
            ]
        );
    }

    #[test]
    fn run_args_place_env_before_entrypoint_and_image() {
        let mut s = spec("u1");
        s.env = vec![("K".to_string(), "V".to_string())];
        s.command = vec!["cmd".to_string()];
        let got = build_run_args("velos-u1", &s);
        assert_eq!(
            got,
            vec![
                "run",
                "--detach",
                "--name",
                "velos-u1",
                "--env",
                "K=V",
                "--entrypoint",
                "cmd",
                "alpine"
            ]
        );
    }

    #[test]
    fn parse_list_filters_to_velos_instances_by_name_prefix() {
        // Mixed schema shapes: `name` vs `names[]`, `status` vs `state`.
        let raw = r#"[
            {"name":"velos-u1","status":"running"},
            {"names":["velos-u2"],"state":"stopped","exitCode":2},
            {"name":"someone-elses","status":"running"}
        ]"#;
        let mut got = parse_list(raw).unwrap();
        got.sort_by(|a, b| a.uid.cmp(&b.uid));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].uid, "u1");
        assert_eq!(got[0].state, InstanceState::Running);
        assert_eq!(got[1].uid, "u2");
        assert_eq!(got[1].state, InstanceState::Exited { exit_code: 2 });
    }

    #[test]
    fn parse_list_reads_running_state_from_real_apple_container_shape() {
        // Actual `container list --all --format json` output (Apple `container`
        // CLI 1.0.0) nests the state under `status.state` rather than exposing a
        // flat `status`/`state` string — see issue where running containers were
        // reported as Succeeded.
        let raw = r#"[
            {"id":"velos-u1","status":{"state":"running","startedDate":"2026-07-18T17:45:28Z"}}
        ]"#;
        let got = parse_list(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, "u1");
        assert_eq!(got[0].state, InstanceState::Running);
    }

    #[test]
    fn parse_list_reads_stopped_state_from_real_apple_container_shape() {
        let raw = r#"[
            {"id":"velos-u1","status":{"state":"stopped","startedDate":"2026-07-18T17:45:28Z"}}
        ]"#;
        let got = parse_list(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, "u1");
        assert_eq!(got[0].state, InstanceState::Exited { exit_code: 0 });
    }
}
