//! Opt-in, worker-owned foreground process runtime. Never adopts a saved PID.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    ContainerRuntime, Instance, InstanceId, InstanceState, RunSpec, RuntimeError, Workload,
    WorkloadHealth,
};

pub use velos_models::velos::{NativeProcessRef, NativeWorkload};

/// Loopback HTTP JSON contract: {"pid": <owned child pid>, "ready": true}.
/// No redirects, remote URLs or liveness-only responses qualify as readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadinessProbe {
    pub port: u16,
    pub path: String,
}

/// Local provisioning contract. No environment, argv or executable comes from
/// the remote workload. Profiles must run in the foreground without daemonizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeProfile {
    #[serde(default)]
    pub readiness: Option<ReadinessProbe>,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub env: HashMap<String, String>,
    pub stop_timeout_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NativeConfig {
    pub state_directory: PathBuf,
    pub profiles: HashMap<String, NativeProfile>,
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn failure(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Command(message.into())
}
fn io(error: std::io::Error) -> RuntimeError {
    RuntimeError::Io(error.to_string())
}

impl NativeConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if !self.state_directory.is_absolute() || self.profiles.is_empty() {
            return Err(failure(
                "native runtime requires an absolute stateDirectory and profiles",
            ));
        }
        for (name, profile) in &self.profiles {
            if profile.readiness.as_ref().is_some_and(|p| {
                p.port == 0 || !p.path.starts_with('/') || p.path.contains(['\r', '\n', '#'])
            }) {
                return Err(failure(format!(
                    "invalid native readiness probe for {name}"
                )));
            }
            if !valid_name(name)
                || !profile.executable.is_absolute()
                || !profile.working_directory.is_absolute()
                || !(1..=60).contains(&profile.stop_timeout_seconds)
                || profile.args.iter().any(|a| a.contains('\0'))
                || profile
                    .env
                    .iter()
                    .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
            {
                return Err(failure(format!("invalid native profile {name}")));
            }
        }
        Ok(())
    }
}

struct OwnedProcess {
    child: Child,
    profile: String,
    marker: PathBuf,
    exit: Option<i32>,
    drains: Vec<std::thread::JoinHandle<()>>,
}

impl OwnedProcess {
    // SAFETY: child is our unreaped child and group leader; no other code waits
    // for it. Its PID/PGID cannot be reused until we reap it below. Never signal
    // after exit has been cached. No PID from disk ever reaches this method.
    fn signal(&self, signal: i32) -> Result<(), RuntimeError> {
        if self.exit.is_none() {
            let pgid = unsafe { libc::getpgid(self.child.id() as i32) };
            if pgid >= 0 && pgid != self.child.id() as i32 {
                return Err(failure(
                    "native child escaped its owned process group; operator recovery required",
                ));
            }
            if pgid < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(io(std::io::Error::last_os_error()));
            }
        }
        if self.exit.is_none() && unsafe { libc::kill(-(self.child.id() as i32), signal) } != 0 {
            let error = std::io::Error::last_os_error();
            // Darwin's killpg1 skips zombies and returns EPERM when it finds
            // no signalable members. Do not swallow a real permission error:
            // require WNOWAIT exit evidence and an empty/leader-only group.
            #[cfg(target_os = "macos")]
            if error.raw_os_error() == Some(libc::EPERM)
                && self.exited_without_reaping()?
                && self.only_exited_leader_remains()
            {
                return Ok(());
            }
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(failure(format!("signal owned process group: {error}")));
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn only_exited_leader_remains(&self) -> bool {
        // Two slots suffice to disprove leader-only; a full buffer is never
        // interpreted as complete. proc_listpids returns zero on error too,
        // so clear/check errno and fail closed on any query failure.
        let mut pids = [0i32; 2];
        let bytes = unsafe {
            *libc::__error() = 0;
            libc::proc_listpids(
                2, /* PROC_PGRP_ONLY */
                self.child.id(),
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(&pids) as i32,
            )
        };
        let error = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        error == 0 && (bytes == 0 || (bytes == 4 && pids[0] == self.child.id() as i32))
    }

    fn exited_without_reaping(&self) -> Result<bool, RuntimeError> {
        if self.exit.is_some() {
            return Ok(true);
        }
        // WNOWAIT preserves ownership until all members of the group have been
        // terminated. In particular a foreground parent exiting must not leave
        // a pipe-holding child behind or let a reused PGID be signalled later.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(failure(format!(
                "waitid owned child: {}",
                std::io::Error::last_os_error()
            )));
        }
        #[cfg(target_os = "macos")]
        let pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let pid = unsafe { info.si_pid() };
        Ok(pid != 0)
    }

    fn observe(&mut self) -> Result<InstanceState, RuntimeError> {
        if self.exit.is_none() && self.exited_without_reaping()? {
            self.signal(libc::SIGKILL)?;
            let status = self.child.wait().map_err(io)?;
            self.exit = Some(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            );
            std::fs::remove_file(&self.marker).map_err(io)?;
            for drain in self.drains.drain(..) {
                if drain.join().is_err() {
                    tracing::warn!("native log drain panicked");
                }
            }
        }
        Ok(match self.exit {
            Some(exit_code) => InstanceState::Exited { exit_code },
            None => InstanceState::Running,
        })
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.exit.is_none() {
            if let Err(error) = self.signal(libc::SIGKILL) {
                tracing::error!(%error, "native cleanup failed; ownership marker retained");
                return;
            }
            if self.child.wait().is_ok() {
                let _ = std::fs::remove_file(&self.marker);
            }
        }
    }
}

/// Logs retain the first MiB per stream per launch, then continue draining.
/// Report truncation with counts; never block a server on a full output pipe.
fn drain(mut source: impl Read + Send + 'static, mut file: File) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        const LIMIT: usize = 1024 * 1024;
        let mut written = 0;
        let mut dropped = 0u64;
        let mut buffer = [0; 8192];
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let keep = n.min(LIMIT - written);
                    if let Err(error) = file.write_all(&buffer[..keep]) {
                        tracing::error!(%error, "native log write failed");
                        break;
                    }
                    written += keep;
                    if n > keep && dropped == 0 {
                        tracing::warn!(
                            limit = LIMIT,
                            "native log truncated; draining remaining output"
                        );
                    }
                    dropped += (n - keep) as u64;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::error!(%error, "native log read failed");
                    break;
                }
            }
        }
        if dropped > 0 {
            tracing::warn!(dropped_bytes = dropped, "native log truncation complete");
        }
    })
}

pub struct NativeProcess {
    config: NativeConfig,
    instances: Mutex<HashMap<String, OwnedProcess>>,
    attempts: Mutex<HashMap<String, (Instant, u32)>>,
    // Keep the exclusive state-directory lock for this runtime's lifetime.
    _lock: File,
}

impl NativeProcess {
    pub fn new(config: NativeConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        let root = &config.state_directory;
        match std::fs::create_dir(root) {
            Ok(()) => std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(io)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(e)),
        }
        let metadata = std::fs::symlink_metadata(root).map_err(io)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(failure(
                "native stateDirectory must be a private directory (0700), not a symlink",
            ));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join("owner.lock"))
            .map_err(io)?;
        lock.try_lock()
            .map_err(|e| failure(format!("native stateDirectory already owned: {e}")))?;
        for entry in std::fs::read_dir(root).map_err(io)? {
            let path = entry.map_err(io)?.path();
            if path.extension().is_some_and(|s| s == "active") {
                return Err(failure(format!(
                    "unclean native ownership record {}; operator recovery required; refusing PID adoption or relaunch",
                    path.display()
                )));
            }
        }
        Ok(Self {
            config,
            instances: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
            _lock: lock,
        })
    }

    fn launch(&self, uid: &str, profile_name: &str) -> Result<OwnedProcess, RuntimeError> {
        if !valid_name(uid) {
            return Err(failure("invalid native workload uid"));
        }
        let profile = self
            .config
            .profiles
            .get(profile_name)
            .ok_or_else(|| failure("native profile is not provisioned on this worker"))?;
        let root = &self.config.state_directory;
        let output = |suffix: &str| -> Result<File, RuntimeError> {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(root.join(format!("{uid}.{suffix}")))
                .map_err(io)
        };
        let stdout = output("stdout.log")?;
        let stderr = output("stderr.log")?;
        let marker = root.join(format!("{profile_name}.active"));
        let mut record = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
            .map_err(io)?;
        // Durable intent precedes spawn; even a crash in between fails closed.
        record.write_all(uid.as_bytes()).map_err(io)?;
        record.sync_all().map_err(io)?;
        File::open(root).and_then(|f| f.sync_all()).map_err(io)?;
        let result = Command::new(&profile.executable)
            .args(&profile.args)
            .current_dir(&profile.working_directory)
            .env_clear()
            .envs(&profile.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let child = match result {
            Ok(child) => child,
            Err(error) => {
                std::fs::remove_file(&marker).map_err(io)?;
                return Err(io(error));
            }
        };
        let mut owned = OwnedProcess {
            child,
            profile: profile_name.into(),
            marker,
            exit: None,
            drains: vec![],
        };
        if let Some(pipe) = owned.child.stdout.take() {
            owned.drains.push(drain(pipe, stdout));
        }
        if let Some(pipe) = owned.child.stderr.take() {
            owned.drains.push(drain(pipe, stderr));
        }
        Ok(owned)
    }

    pub async fn contains(&self, uid: &str) -> bool {
        self.instances.lock().await.contains_key(uid)
    }
}

#[async_trait]
impl ContainerRuntime for NativeProcess {
    async fn run(&self, spec: &RunSpec) -> Result<InstanceId, RuntimeError> {
        let profile = match &spec.workload {
            Workload::NativeProcess { profile } => profile,
            Workload::Container { .. } => return Err(failure("container runtime is disabled")),
        };
        if !spec.command.is_empty() || !spec.env.is_empty() {
            return Err(failure(
                "native argv and environment must come from the local profile",
            ));
        }
        let mut instances = self.instances.lock().await;
        if let Some(existing) = instances.get(&spec.uid) {
            if existing.profile != *profile {
                return Err(failure("cannot change an existing workload's profile"));
            }
            return Ok(InstanceId(format!("native-{}", spec.uid)));
        }
        for process in instances.values_mut() {
            process.observe()?;
            if process.profile == *profile && process.exit.is_none() {
                return Err(failure("native profile already has a live instance"));
            }
        }
        // Bound repeated launch failures / crash loops, shared by profile so a
        // new UID cannot accidentally create a rapid model reload loop.
        let mut attempts = self.attempts.lock().await;
        let now = Instant::now();
        let count = match attempts.get(profile) {
            Some((previous, count)) => {
                let delay = Duration::from_secs((1u64 << (*count).min(6)).min(60));
                if now.duration_since(*previous) < delay {
                    return Err(failure("native launch backoff active (maximum 60 seconds)"));
                }
                if now.duration_since(*previous) >= Duration::from_secs(60) {
                    0
                } else {
                    count.saturating_add(1)
                }
            }
            None => 0,
        };
        attempts.insert(profile.clone(), (now, count));
        instances.insert(spec.uid.clone(), self.launch(&spec.uid, profile)?);
        Ok(InstanceId(format!("native-{}", spec.uid)))
    }

    async fn stop(&self, uid: &str) -> Result<(), RuntimeError> {
        let mut instances = self.instances.lock().await;
        let Some(process) = instances.get_mut(uid) else {
            return Ok(());
        };
        if matches!(process.observe()?, InstanceState::Exited { .. }) {
            return Ok(());
        }
        process.signal(libc::SIGTERM)?;
        let timeout = self
            .config
            .profiles
            .get(&process.profile)
            .ok_or_else(|| failure("missing native profile"))?
            .stop_timeout_seconds;
        let deadline = Instant::now() + Duration::from_secs(timeout.into());
        while !process.exited_without_reaping()? && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        process.signal(libc::SIGKILL)?;
        // Poll before wait, so no synchronous wait holds up the executor.
        let kill_deadline = Instant::now() + Duration::from_secs(5);
        while !process.exited_without_reaping()? {
            if Instant::now() >= kill_deadline {
                return Err(failure(
                    "owned child did not exit after SIGKILL; retaining ownership record",
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        process.observe()?;
        Ok(())
    }

    async fn start(&self, uid: &str) -> Result<InstanceId, RuntimeError> {
        let mut instances = self.instances.lock().await;
        let process = instances
            .get_mut(uid)
            .ok_or_else(|| failure("no such native instance"))?;
        if matches!(process.observe()?, InstanceState::Exited { .. }) {
            let profile = process.profile.clone();
            for other in instances.values_mut() {
                other.observe()?;
                if other.profile == profile && other.exit.is_none() {
                    return Err(failure("native profile already has a live instance"));
                }
            }
            instances.insert(uid.into(), self.launch(uid, &profile)?);
        }
        Ok(InstanceId(format!("native-{uid}")))
    }

    async fn remove(&self, uid: &str) -> Result<(), RuntimeError> {
        self.stop(uid).await?;
        self.instances.lock().await.remove(uid);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Instance>, RuntimeError> {
        let mut instances = self.instances.lock().await;
        instances
            .iter_mut()
            .map(|(uid, process)| {
                Ok(Instance {
                    uid: uid.clone(),
                    id: InstanceId(format!("native-{uid}")),
                    state: process.observe()?,
                    // Shared host networking is not a private container address. Never
                    // route Services to an arbitrary loopback listener based on liveness.
                    ip: None,
                })
            })
            .collect()
    }

    async fn health(&self) -> Result<Vec<WorkloadHealth>, RuntimeError> {
        let snapshots: Vec<_> = {
            let mut instances = self.instances.lock().await;
            let mut snapshots = vec![];
            for (uid, process) in instances.iter_mut() {
                let running = matches!(process.observe()?, InstanceState::Running);
                let probe = self
                    .config
                    .profiles
                    .get(&process.profile)
                    .and_then(|p| p.readiness.clone());
                snapshots.push((uid.clone(), process.child.id(), running, probe));
            }
            snapshots
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .map_err(|e| failure(e.to_string()))?;
        let mut health = vec![];
        for (uid, pid, running, probe) in snapshots {
            let ready = match (running, probe) {
                (false, _) => Some(false),
                (true, None) => None,
                (true, Some(probe)) => {
                    let result = async {
                        let mut response = client
                            .get(format!("http://127.0.0.1:{}{}", probe.port, probe.path))
                            .send()
                            .await
                            .ok()?;
                        if !response.status().is_success() {
                            return None;
                        }
                        let mut bytes = Vec::new();
                        while let Some(chunk) = response.chunk().await.ok()? {
                            if bytes.len() + chunk.len() > 8192 {
                                tracing::warn!(%uid, observed_bytes = bytes.len() + chunk.len(), limit = 8192, "native readiness response exceeds limit");
                                return None;
                            }
                            bytes.extend_from_slice(&chunk);
                        }
                        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
                        Some(
                            value.get("pid").and_then(serde_json::Value::as_u64)
                                == Some(pid.into())
                                && value.get("ready").and_then(serde_json::Value::as_bool)
                                    == Some(true),
                        )
                    }
                    .await;
                    Some(result.unwrap_or(false))
                }
            };
            health.push(WorkloadHealth { uid, ready });
        }
        Ok(health)
    }

    async fn shutdown(&self) -> Result<(), RuntimeError> {
        let uids: Vec<_> = self.instances.lock().await.keys().cloned().collect();
        for uid in uids {
            self.stop(&uid).await?;
        }
        Ok(())
    }

    async fn version(&self) -> Result<String, RuntimeError> {
        Ok("native-process/1".into())
    }
}
