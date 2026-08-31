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
    /// The instance's address on the worker's container network, when the
    /// runtime reports one. `None` for anything not currently running, and for
    /// a runtime that does not give instances their own address — so a caller
    /// that needs to dial the instance must handle its absence rather than
    /// assume a default. This is what the worker-side service proxy forwards to.
    pub ip: Option<String>,
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Launch an instance tagged with `spec.uid`. Idempotent callers check
    /// [`list`](ContainerRuntime::list) first.
    async fn run(&self, spec: &RunSpec) -> Result<InstanceId, RuntimeError>;
    /// Shut the instance tagged with `uid` down, **keeping** it (and its disk)
    /// so [`start`](ContainerRuntime::start) can bring it back. No-op if already
    /// gone. Removal is a separate call — see [`remove`](ContainerRuntime::remove).
    async fn stop(&self, uid: &str) -> Result<(), RuntimeError>;
    /// Boot a stopped instance tagged with `uid` back up, preserving its disk
    /// state. Errors when the runtime has no such instance: waking a container
    /// whose instance is gone means launching a fresh one, which is
    /// [`run`](ContainerRuntime::run)'s job, not a silent fallback here.
    async fn start(&self, uid: &str) -> Result<InstanceId, RuntimeError>;
    /// Remove the instance tagged with `uid` (no-op if already gone).
    async fn remove(&self, uid: &str) -> Result<(), RuntimeError>;
    /// All instances the runtime knows about, by uid.
    async fn list(&self) -> Result<Vec<Instance>, RuntimeError>;
    /// Reported runtime version string (for `WorkerStatus`).
    async fn version(&self) -> Result<String, RuntimeError>;

    /// Whether this runtime can run containers right now — bringing it back up
    /// first if this backend knows how.
    ///
    /// The worker calls this before every heartbeat, because the heartbeat is a
    /// claim that this machine can run containers. Being *installed* is not that
    /// claim: a runtime is typically a daemon that can be stopped, crash, or
    /// never start after a reboot while its CLI sits on `PATH` answering
    /// `--version` perfectly happily.
    ///
    /// The default is the honest minimum for a backend with nothing to restart:
    /// ask it to `list`, the one call every other method depends on. A backend
    /// that owns a restartable service overrides this to restart it.
    async fn ensure_ready(&self) -> Result<(), RuntimeError> {
        self.list().await.map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// FakeRuntime — in-memory, for tests and the e2e harness.
// ---------------------------------------------------------------------------

/// An in-memory runtime used by tests and `velos-tests`. Exit can be simulated
/// with [`FakeRuntime::set_exited`].
pub struct FakeRuntime {
    instances: Mutex<HashMap<String, Instance>>,
    runs: Mutex<usize>,
    instance_ip: String,
    /// Images whose launch fails, mapped to the message it fails with. Keyed by
    /// image rather than by uid because the failure a worker cannot recover
    /// from — an unpullable image — is a property of the image, and a test
    /// should be able to break one container while leaving its neighbours on
    /// the same worker launchable.
    run_failures: Mutex<HashMap<String, String>>,
    /// The runtime service being down, and whether `ensure_ready` can bring it
    /// back. `None` is up. See [`FakeRuntime::take_down`].
    down: Mutex<Option<Down>>,
}

/// A downed fake runtime: down, and either repairable or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Down {
    repairable: bool,
}

impl Default for FakeRuntime {
    fn default() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            runs: Mutex::new(0),
            // Loopback, so a test can put a real listener behind a fake
            // instance and drive traffic through the worker's service proxy.
            instance_ip: "127.0.0.1".to_string(),
            run_failures: Mutex::new(HashMap::new()),
            down: Mutex::new(None),
        }
    }
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use `ip` as the address of every instance this runtime launches.
    pub fn with_instance_ip(ip: impl Into<String>) -> Self {
        Self {
            instance_ip: ip.into(),
            ..Self::default()
        }
    }

    /// How many times [`run`](ContainerRuntime::run) launched a *fresh*
    /// instance. Lets a test tell "resumed the same micro-VM" apart from
    /// "silently recreated it", which `list` alone cannot show.
    pub fn run_count(&self) -> Result<usize, RuntimeError> {
        self.runs.lock().map(|g| *g).map_err(|_| RuntimeError::Lock)
    }

    /// Make every launch of `image` fail with `message`, the way a bad image
    /// reference does on a real runtime.
    pub fn fail_image(&self, image: &str, message: &str) -> Result<(), RuntimeError> {
        self.run_failures
            .lock()
            .map_err(|_| RuntimeError::Lock)?
            .insert(image.to_string(), message.to_string());
        Ok(())
    }

    /// Take the runtime service down, the way a stopped or crashed container
    /// daemon goes down: every call that needs it fails, while `version` keeps
    /// answering — an installed CLI does not stop existing.
    ///
    /// `repairable` is the difference between the two cases a worker must tell
    /// apart: a service `ensure_ready` can restart, and one it cannot, where the
    /// only correct move is to stop claiming this machine can run containers.
    pub fn take_down(&self, repairable: bool) -> Result<(), RuntimeError> {
        *self.down.lock().map_err(|_| RuntimeError::Lock)? = Some(Down { repairable });
        Ok(())
    }

    /// Fail if the runtime service is down.
    fn check_up(&self) -> Result<(), RuntimeError> {
        match *self.down.lock().map_err(|_| RuntimeError::Lock)? {
            Some(_) => Err(RuntimeError::Command(
                "the runtime service is not running".to_string(),
            )),
            None => Ok(()),
        }
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
        self.check_up()?;
        if let Some(message) = self
            .run_failures
            .lock()
            .map_err(|_| RuntimeError::Lock)?
            .get(&spec.image)
        {
            return Err(RuntimeError::Command(message.clone()));
        }
        let id = InstanceId(format!("fake-{}", spec.uid));
        *self.runs.lock().map_err(|_| RuntimeError::Lock)? += 1;
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        g.insert(
            spec.uid.clone(),
            Instance {
                uid: spec.uid.clone(),
                id: id.clone(),
                state: InstanceState::Running,
                ip: Some(self.instance_ip.clone()),
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

    async fn start(&self, uid: &str) -> Result<InstanceId, RuntimeError> {
        self.check_up()?;
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        match g.get_mut(uid) {
            Some(inst) => {
                inst.state = InstanceState::Running;
                Ok(inst.id.clone())
            }
            None => Err(RuntimeError::Command(format!("no such instance: {uid}"))),
        }
    }

    async fn remove(&self, uid: &str) -> Result<(), RuntimeError> {
        let mut g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        g.remove(uid);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Instance>, RuntimeError> {
        self.check_up()?;
        let g = self.instances.lock().map_err(|_| RuntimeError::Lock)?;
        Ok(g.values()
            .cloned()
            .map(|mut i| {
                // Mirror the real backend: an address belongs to a *running*
                // instance. Derived here rather than cleared at each transition,
                // so no future state change can forget to do it.
                if i.state != InstanceState::Running {
                    i.ip = None;
                }
                i
            })
            .collect())
    }

    /// Answers even while the service is down: `container --version` does too,
    /// which is exactly why it is not a health check.
    async fn version(&self) -> Result<String, RuntimeError> {
        Ok("fake-runtime/1.0".to_string())
    }

    async fn ensure_ready(&self) -> Result<(), RuntimeError> {
        {
            let mut down = self.down.lock().map_err(|_| RuntimeError::Lock)?;
            if down.map(|d| d.repairable).unwrap_or(false) {
                *down = None;
            }
        }
        self.list().await.map(|_| ())
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
//   start   : `container start velos-<uid>`
//   remove  : `container delete --force velos-<uid>`
//   list    : `container list --all --format json`
//   version : `container --version`
//   repair  : `container system start --disable-kernel-install --timeout <secs>`
//
// These match the apple/container 1.0 command reference (`delete` has alias
// `rm`, `list` has alias `ls`). If your installed version differs, this is the
// one place to adjust.
//
// Hibernation relies on `run` **not** passing `--rm`: without it a stopped
// container survives, keeps its disk, still appears in `list --all`, and
// `start` boots it again. Adding `--rm` here would silently turn every
// hibernate into a destroy.

const SUBCMD_RUN: &str = "run";
const SUBCMD_STOP: &str = "stop";
const SUBCMD_START: &str = "start";
const SUBCMD_REMOVE: &str = "delete";
const SUBCMD_LIST: &str = "list";
const SUBCMD_SYSTEM: &str = "system";
/// Seconds `container system start` may wait for the API service to answer.
/// Bounded because it runs on the worker's heartbeat path: a service that is
/// wedged rather than stopped must not hold the heartbeat open indefinitely.
const SYSTEM_START_TIMEOUT_SECS: u32 = 15;
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

/// Build the `container system start …` argv (pure, so it's unit-testable).
///
/// `--disable-kernel-install` because this is a *repair*, not an install. The
/// flag suppresses a prompt that, under launchd, has nobody to answer it, and a
/// machine whose default kernel was never installed needs an operator — not a
/// worker quietly downloading one in the middle of a heartbeat.
fn build_system_start_args() -> Vec<String> {
    vec![
        SUBCMD_SYSTEM.to_string(),
        SUBCMD_START.to_string(),
        "--disable-kernel-install".to_string(),
        "--timeout".to_string(),
        SYSTEM_START_TIMEOUT_SECS.to_string(),
    ]
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
            // A silent failure is the common case for `container run` against an
            // image that is not a runnable image at all. Reporting the exit
            // status rather than an empty string keeps the error from arriving
            // as `runtime command failed: `, which names no cause whatsoever.
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(RuntimeError::Command(if stderr.is_empty() {
                format!("`{} {}` {}", self.bin, args.join(" "), out.status)
            } else {
                stderr
            }));
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

    async fn start(&self, uid: &str) -> Result<InstanceId, RuntimeError> {
        let name = instance_name(uid);
        self.output(&[SUBCMD_START.to_string(), name.clone()])
            .await?;
        Ok(InstanceId(name))
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

    /// Probe, and restart Apple's `container` services if the probe fails.
    ///
    /// `list` is the probe rather than `container system status`: it is the call
    /// the worker actually depends on, so it answers the only question that
    /// matters — not "does a service claim to be up" but "can this machine run
    /// containers". A restart that leaves it still unable to list is a failure,
    /// and the worker withholds its heartbeat on the strength of that.
    async fn ensure_ready(&self) -> Result<(), RuntimeError> {
        let Err(down) = self.list().await else {
            return Ok(());
        };
        tracing::warn!("container runtime is not answering ({down}); restarting its services");
        self.output(&build_system_start_args()).await.map_err(|e| {
            RuntimeError::Command(format!("{down}; `{} system start` failed: {e}", self.bin))
        })?;
        self.list().await.map(|_| ()).map_err(|e| {
            RuntimeError::Command(format!(
                "`{} system start` returned but the runtime still does not answer: {e}",
                self.bin
            ))
        })
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

/// Read the instance's container-network IPv4 address. apple/container reports
/// it as `status.networks[].ipv4Address` in CIDR form (`192.168.64.68/24`), so
/// the prefix length is trimmed off. Absent for a stopped instance.
fn address_str(entry: &serde_json::Value) -> Option<String> {
    let addr = entry
        .get("status")?
        .get("networks")?
        .as_array()?
        .iter()
        .find_map(|n| n.get("ipv4Address").and_then(|v| v.as_str()))?;
    Some(addr.split('/').next().unwrap_or(addr).to_string())
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
            // apple/container reports no exit code anywhere (see issue #28), so
            // on that runtime this fallback makes every crash read as a clean
            // exit. Other runtimes that do report one are picked up here.
            let exit_code = entry
                .get("exitCode")
                .or_else(|| entry.get("exit_code"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            InstanceState::Exited { exit_code }
        };
        // Only a running instance has a usable address: apple/container keeps
        // the last-known `networks` entry on a stopped one, and forwarding to it
        // would hand traffic to whatever picks the address up next.
        let ip = if running { address_str(&entry) } else { None };
        out.push(Instance {
            uid: uid.to_string(),
            id: InstanceId(name.to_string()),
            state,
            ip,
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

    #[tokio::test]
    async fn fake_runtime_stop_keeps_the_instance_so_start_can_wake_it() {
        // Hibernation is stop-then-start on the *same* instance: `stop` must not
        // remove it, and `start` must not count as a fresh `run`.
        let rt = FakeRuntime::new();
        let id = rt.run(&spec("u1")).await.unwrap();

        rt.stop("u1").await.unwrap();
        let list = rt.list().await.unwrap();
        assert_eq!(list.len(), 1, "stop must keep the instance");
        assert_eq!(list[0].state, InstanceState::Exited { exit_code: 0 });

        let woken = rt.start("u1").await.unwrap();
        assert_eq!(woken, id, "start resumes the same instance");
        assert_eq!(rt.list().await.unwrap()[0].state, InstanceState::Running);
        assert_eq!(
            rt.run_count().unwrap(),
            1,
            "start must not re-run the image"
        );
    }

    #[tokio::test]
    async fn fake_runtime_start_of_a_removed_instance_errors() {
        // `start` never conjures an instance — a caller whose instance is gone
        // has to `run` a fresh one, and must see that it is doing so.
        let rt = FakeRuntime::new();
        rt.run(&spec("u1")).await.unwrap();
        rt.remove("u1").await.unwrap();
        assert!(rt.start("u1").await.is_err());
    }

    #[test]
    fn parse_list_reports_a_hibernated_instance_as_stopped_not_missing() {
        // A hibernated micro-VM must still appear in `list --all`, or the worker
        // would read it as gone and launch a fresh one instead of waking it.
        // Shape captured from apple/container 1.0.0.
        let raw = r#"[
            {"id":"velos-u1","status":{"state":"stopped","startedDate":"2026-08-24T00:33:55Z"}}
        ]"#;
        let got = parse_list(raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uid, "u1");
        assert_eq!(got[0].state, InstanceState::Exited { exit_code: 0 });
    }

    #[test]
    fn parse_list_reads_the_container_network_address() {
        // Shape captured from a live `container list --all --format json` on
        // apple/container 1.0.0: the address is nested under status.networks and
        // carries a prefix length, which must not reach a `connect()`.
        let raw = r#"[
            {"id":"velos-u1","status":{"state":"running","networks":[
                {"hostname":"velos-u1","ipv4Address":"192.168.64.68/24","ipv4Gateway":"192.168.64.1"}
            ]}},
            {"id":"velos-u2","status":{"state":"stopped","networks":[
                {"hostname":"velos-u2","ipv4Address":"192.168.64.66/24"}
            ]}}
        ]"#;
        let got = parse_list(raw).unwrap();
        assert_eq!(got[0].ip.as_deref(), Some("192.168.64.68"));
        assert_eq!(
            got[1].ip, None,
            "a stopped instance keeps its last address in the CLI output, but \
             nothing is listening on it — reporting it would send traffic to \
             whichever instance takes the address next"
        );
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

    #[tokio::test]
    async fn a_down_runtime_is_not_ready_and_a_repairable_one_comes_back() {
        let rt = FakeRuntime::new();
        assert!(rt.ensure_ready().await.is_ok());

        // Down but restartable: `ensure_ready` brings it back and the worker
        // carries on, which is the whole point of asking before heartbeating.
        rt.take_down(true).unwrap();
        assert!(rt.list().await.is_err());
        assert!(rt.ensure_ready().await.is_ok());
        assert!(rt.list().await.is_ok());

        // Down for good: no verdict but "not ready".
        rt.take_down(false).unwrap();
        assert!(rt.ensure_ready().await.is_err());
        assert!(rt.ensure_ready().await.is_err(), "and it stays not ready");
    }

    #[tokio::test]
    async fn a_down_runtime_still_reports_its_version() {
        // The trap the health check exists for: the CLI answers `--version` from
        // its own binary, so a version string is never evidence that anything
        // can run.
        let rt = FakeRuntime::new();
        rt.take_down(false).unwrap();
        assert!(rt.version().await.is_ok());
        assert!(rt.ensure_ready().await.is_err());
    }

    #[test]
    fn system_start_never_prompts_and_never_waits_forever() {
        let args = build_system_start_args();
        assert_eq!(args[0..2], ["system", "start"]);
        // Under launchd there is nobody to answer a kernel-install prompt, and
        // this runs on the heartbeat path, so an unbounded wait would hold the
        // worker's own liveness signal open.
        assert!(
            args.contains(&"--disable-kernel-install".to_string()),
            "{args:?}"
        );
        assert!(args.contains(&"--timeout".to_string()), "{args:?}");
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
