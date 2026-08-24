//! `veloslet status` — one command that answers "is this worker working, and if
//! not, what do I fix?".
//!
//! The worker-side counterpart of `velosctl doctor`, named for what it reports
//! rather than for the CLI it mirrors. Deliberately a separate copy of the
//! report scaffolding: the two tools check disjoint things (a CLI's saved login
//! versus a worker's credential, capacity and launchd agent), and a shared crate
//! for ~150 lines of presentation would couple the worker daemon to the CLI.
//!
//! The logic is split in two. An *observation* enum records what was seen (the
//! config's mode, an HTTP status, whether launchd knows the label), and a pure
//! function turns each observation into a [`Check`] carrying a verdict and a
//! fix-it hint. Only [`diagnose`] touches the filesystem, the network, or
//! another process, so every verdict and every hint is unit-testable.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::daemon::{BUNDLE_ID, WorkerConfig};
use crate::host::{HostResources, detect_host, validate_capacity};
use crate::memory::Memory;

/// How long any single probe of the control plane may take. A doctor that hangs
/// is worse than one that reports "unreachable".
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `container` CLI (Apple Containerization) — the runtime this worker drives.
/// Matches the binary `velos-runtime`'s `AppleContainer` shells out to.
const RUNTIME_BIN: &str = "container";

/// The innermost cause of an error. `reqwest`'s own `Display` repeats the URL
/// the report already prints and buries the actual reason (connection refused,
/// DNS failure) one or two sources down.
fn root_cause(err: &dyn std::error::Error) -> String {
    let mut cause = err;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// The verdict of a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Working.
    Pass,
    /// Working, but something is worth knowing about.
    Warn,
    /// Broken — this is why the worker isn't usable.
    Fail,
    /// Not checked, because an earlier check made it meaningless.
    Skip,
}

impl Status {
    /// The glyph the report leads each line with.
    fn glyph(self) -> &'static str {
        match self {
            Status::Pass => "✔",
            Status::Warn => "!",
            Status::Fail => "✗",
            Status::Skip => "-",
        }
    }
}

/// One checked thing: what it is, how it went, and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub hint: Option<String>,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
            hint: None,
        }
    }

    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Pass, detail)
    }

    fn skip(name: &'static str, why: impl Into<String>) -> Self {
        Self::new(name, Status::Skip, why)
    }

    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail).with_hint(hint)
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail).with_hint(hint)
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Every check, in the order they were run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// True when something is actually broken. Warnings do not count — they are
    /// notes about a setup that still works.
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self
            .checks
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or_default();
        for check in &self.checks {
            writeln!(
                f,
                "  {} {:width$}  {}",
                check.status.glyph(),
                check.name,
                check.detail
            )?;
            if let Some(hint) = &check.hint {
                writeln!(f, "    {:width$}    → {hint}", "")?;
            }
        }

        let failed = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        let warned = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count();
        match (failed, warned) {
            (0, 0) => writeln!(f, "\nall checks passed"),
            (0, w) => writeln!(f, "\n{}, nothing broken", plural(w, "warning")),
            (fl, 0) => writeln!(f, "\n{fl} failed — see the hints above"),
            (fl, w) => writeln!(
                f,
                "\n{fl} failed, {} — see the hints above",
                plural(w, "warning")
            ),
        }
    }
}

/// `1 warning` / `2 warnings`.
fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("{n} {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

// ---------------------------------------------------------------------------
// Observations — what the probes saw
// ---------------------------------------------------------------------------

/// What the worker config looked like on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigState {
    /// Nothing has been written yet — this machine has never run `setup`.
    Missing(PathBuf),
    /// Present but unreadable or not valid JSON.
    Unusable { path: PathBuf, error: String },
    /// Present and parsed. `mode` is the unix permission bits, where known.
    Present {
        path: PathBuf,
        mode: Option<u32>,
        config: Box<WorkerConfig>,
    },
}

/// Whether the control plane answered its liveness probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    Reachable,
    Unreachable(String),
    /// The URL is `https` but this build has no TLS backend, so no request to
    /// it can ever succeed.
    NoTlsSupport,
}

/// Who the server says this worker's credential belongs to (`GET /auth/v1/me`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityState {
    /// The credential is live and names this worker.
    Worker(String),
    /// Accepted, but for a different name than the config claims.
    Admin,
    /// The server refused it — revoked, or the worker was deleted.
    Rejected(u16),
    Unexpected(String),
}

/// Advertised capacity versus what the machine physically has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityState {
    Fits {
        cpu: u32,
        memory: Memory,
        host: HostResources,
    },
    Exceeds(String),
    /// The host could not be measured (not macOS, or `sysctl` failed).
    Unknown(String),
}

/// Whether this machine can run containers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeState {
    Absent,
    Available(String),
    Broken(String),
}

/// Whether launchd is running the background worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Not macOS — there is no LaunchAgent to have.
    NotApplicable,
    /// launchd knows the label.
    Loaded,
    /// launchd does not know it; the worker only runs when `run` is in a terminal.
    NotLoaded,
    /// A LaunchAgent is loaded under the *pre-rename* bundle id, which no
    /// current `stop`/`uninstall` will touch.
    StaleLabel(String),
    /// The current agent **and** a stale one are both loaded: two workers, same
    /// config, same node name, racing each other.
    Duplicate(String),
}

// ---------------------------------------------------------------------------
// Pure interpreters — observation to verdict
// ---------------------------------------------------------------------------

fn check_agent_binary(version: &str, path: Option<&str>) -> Check {
    match path {
        Some(p) => Check::pass("veloslet", format!("v{version} ({p})")),
        None => Check::pass("veloslet", format!("v{version}")),
    }
}

fn check_config(state: &ConfigState) -> Check {
    match state {
        ConfigState::Missing(path) => Check::fail(
            "config file",
            format!("{} does not exist", path.display()),
            "run `veloslet setup --server <url> --node <name> --token <token> \
             --cpu <n> --memory <size>`",
        ),
        ConfigState::Unusable { path, error } => Check::fail(
            "config file",
            format!("{} is unusable: {error}", path.display()),
            "fix the JSON by hand, or delete it and run `veloslet setup` again",
        ),
        ConfigState::Present { path, mode, .. } => match mode {
            // The credential lives in this file; group/other read is a leak.
            Some(m) if m & 0o077 != 0 => Check::warn(
                "config file",
                format!(
                    "{} is mode {:o} — it holds this worker's credential",
                    path.display(),
                    m & 0o777
                ),
                format!("chmod 600 {}", path.display()),
            ),
            _ => Check::pass("config file", path.display().to_string()),
        },
    }
}

fn check_joined(cfg: &WorkerConfig) -> Check {
    if cfg.is_connected() {
        Check::pass("joined", format!("holds a credential for {}", cfg.node))
    } else {
        Check::fail(
            "joined",
            "no credential — this worker has never joined a control plane",
            "mint a token with `velosctl token create`, then run `veloslet setup --token <token> …`",
        )
    }
}

fn check_reachable(server: &str, reach: &Reachability) -> Check {
    match reach {
        Reachability::Reachable => Check::pass("reachable", format!("{server} answered /healthz")),
        Reachability::NoTlsSupport => Check::fail(
            "reachable",
            format!("{server} is https, and this veloslet was built without TLS support"),
            "reinstall an official release build, or rebuild with reqwest's `rustls` feature",
        ),
        Reachability::Unreachable(why) => Check::fail(
            "reachable",
            format!("{server}: {why}"),
            "check the control plane is running and the URL is right \
             (`veloslet config set --server <url>`)",
        ),
    }
}

fn check_identity(node: &str, state: &IdentityState) -> Check {
    match state {
        IdentityState::Worker(name) if name == node => Check::pass(
            "identity",
            format!("the server knows this worker as {name}"),
        ),
        IdentityState::Worker(name) => Check::fail(
            "identity",
            format!("the credential belongs to {name}, but the config says {node}"),
            "the config was edited after joining — run `veloslet setup` again with a new token",
        ),
        IdentityState::Admin => Check::fail(
            "identity",
            "this credential is an admin token, not a worker credential",
            "worker credentials come from `veloslet setup`; do not paste a CLI token into the config",
        ),
        IdentityState::Rejected(code) => Check::fail(
            "identity",
            format!("the server rejected this worker's credential (HTTP {code})"),
            "the worker was deleted, which revokes its credential — \
             mint a new join token and run `veloslet setup` again",
        ),
        IdentityState::Unexpected(why) => Check::fail(
            "identity",
            format!("could not confirm this worker's identity: {why}"),
            "check the server logs",
        ),
    }
}

fn check_capacity(state: &CapacityState) -> Check {
    match state {
        CapacityState::Fits { cpu, memory, host } => Check::pass(
            "capacity",
            format!(
                "advertising {cpu} cpu, {memory} of {} cpu, {}",
                host.cpu,
                Memory::from_bytes(host.memory_bytes)
            ),
        ),
        // Reachable only by hand-editing the file: `setup` and `config set` both
        // validate. It means the worker will refuse to start.
        CapacityState::Exceeds(why) => Check::fail(
            "capacity",
            why.clone(),
            "lower it with `veloslet config set --cpu <n> --memory <size>` — \
             the worker refuses to start while it advertises more than the machine has",
        ),
        CapacityState::Unknown(why) => Check::warn(
            "capacity",
            format!("could not measure this machine: {why}"),
            "capacity is validated against the host at startup; this check needs macOS",
        ),
    }
}

fn check_runtime(state: &RuntimeState) -> Check {
    match state {
        // Printed as-is: `container --version` already names itself, so any
        // prefix here reads as "container CLI container CLI version 1.0.0".
        RuntimeState::Available(version) => Check::pass("runtime", version.clone()),
        RuntimeState::Absent => Check::fail(
            "runtime",
            format!("no `{RUNTIME_BIN}` CLI on PATH — this machine cannot run containers"),
            "install Apple Containerization (https://github.com/apple/containerization) \
             and run `container system start`",
        ),
        RuntimeState::Broken(why) => Check::fail(
            "runtime",
            format!("`{RUNTIME_BIN}` is present but did not answer: {why}"),
            "run `container system start`",
        ),
    }
}

fn check_agent(state: &AgentState) -> Check {
    match state {
        AgentState::NotApplicable => Check::skip(
            "background",
            "the LaunchAgent is macOS-only; run this worker under your own service manager",
        ),
        AgentState::Loaded => Check::pass("background", format!("launchd is running {BUNDLE_ID}")),
        AgentState::Duplicate(stale) => Check::fail(
            "background",
            format!(
                "two workers are loaded — {BUNDLE_ID} and {stale} — sharing one config \
                 and one node name"
            ),
            format!(
                "they reconcile against each other and delete each other's containers; \
                 drop the old one with `launchctl unload \
                 ~/Library/LaunchAgents/{stale}.plist && rm ~/Library/LaunchAgents/{stale}.plist`"
            ),
        ),
        AgentState::NotLoaded => Check::warn(
            "background",
            "no LaunchAgent loaded — this worker only runs while `veloslet run` is in a terminal",
            "run `veloslet run -d` to keep it running across logins and crashes",
        ),
        AgentState::StaleLabel(label) => Check::warn(
            "background",
            format!("launchd is running {label}, which is not the current {BUNDLE_ID}"),
            format!(
                "that agent predates a bundle rename, so `veloslet stop`/`uninstall` will not \
                 touch it — remove it with `launchctl unload ~/Library/LaunchAgents/{label}.plist` \
                 and `rm` that plist, then `veloslet run -d`"
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Probes — the only side-effecting code
// ---------------------------------------------------------------------------

fn url(server: &str, path: &str) -> String {
    format!("{}{path}", server.trim_end_matches('/'))
}

/// Tell a missing TLS backend apart from a server that is simply down: without
/// one, reqwest rejects an https URL up front with "scheme is not http" rather
/// than failing to connect.
fn classify_transport(server: &str, cause: String) -> Reachability {
    if server.starts_with("https://") && cause.contains("scheme is not http") {
        return Reachability::NoTlsSupport;
    }
    Reachability::Unreachable(cause)
}

fn observe_config(path: PathBuf) -> ConfigState {
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigState::Missing(path),
        Err(e) => {
            return ConfigState::Unusable {
                path,
                error: e.to_string(),
            };
        }
    };
    match serde_json::from_str::<WorkerConfig>(&text) {
        Ok(config) => {
            let mode = file_mode(&path);
            ConfigState::Present {
                path,
                mode,
                config: Box::new(config),
            }
        }
        Err(e) => ConfigState::Unusable {
            path,
            error: e.to_string(),
        },
    }
}

fn file_mode(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode())
}

fn observe_capacity(cfg: &WorkerConfig) -> CapacityState {
    match detect_host() {
        Ok(host) => match validate_capacity(cfg.cpu, cfg.memory, host) {
            Ok(()) => CapacityState::Fits {
                cpu: cfg.cpu,
                memory: cfg.memory,
                host,
            },
            Err(e) => CapacityState::Exceeds(e.to_string()),
        },
        Err(e) => CapacityState::Unknown(e.to_string()),
    }
}

fn observe_runtime() -> RuntimeState {
    match Command::new(RUNTIME_BIN).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text.lines().next().unwrap_or_default().trim();
            RuntimeState::Available(line.to_string())
        }
        Ok(out) => RuntimeState::Broken(
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .chars()
                .take(120)
                .collect(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RuntimeState::Absent,
        Err(e) => RuntimeState::Broken(e.to_string()),
    }
}

/// Ask launchd which velos worker label, if any, it is running.
///
/// Looks for *any* `velos-worker` label rather than only the current one, so a
/// LaunchAgent left behind by a pre-rename install is reported instead of
/// silently reading as "not running" — the current `stop`/`uninstall` cannot see
/// it either.
fn observe_agent() -> AgentState {
    if !cfg!(target_os = "macos") {
        return AgentState::NotApplicable;
    }
    let Ok(out) = Command::new("launchctl").arg("list").output() else {
        return AgentState::NotLoaded;
    };
    let listing = String::from_utf8_lossy(&out.stdout);
    let labels: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter(|label| label.ends_with("velos-worker"))
        .collect();
    let stale: Vec<&&str> = labels.iter().filter(|l| **l != BUNDLE_ID).collect();
    match (labels.contains(&BUNDLE_ID), stale.first()) {
        // The worst case, and not hypothetical: `run -d` loads the current
        // label without touching one left by a pre-rename install, so both run
        // the same binary against the same config under the same node name.
        (true, Some(other)) => AgentState::Duplicate((**other).to_string()),
        (true, None) => AgentState::Loaded,
        (false, Some(other)) => AgentState::StaleLabel((**other).to_string()),
        (false, None) => AgentState::NotLoaded,
    }
}

async fn observe_reachability(http: &reqwest::Client, server: &str) -> Reachability {
    match http.get(url(server, "/healthz")).send().await {
        Ok(resp) if resp.status().is_success() => Reachability::Reachable,
        Ok(resp) => Reachability::Unreachable(format!("HTTP {}", resp.status())),
        Err(e) => classify_transport(server, root_cause(&e)),
    }
}

async fn observe_identity(http: &reqwest::Client, server: &str, credential: &str) -> IdentityState {
    let resp = match http
        .get(url(server, "/auth/v1/me"))
        .bearer_auth(credential)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return IdentityState::Unexpected(root_cause(&e)),
    };
    let status = resp.status();
    if !status.is_success() {
        return IdentityState::Rejected(status.as_u16());
    }
    match resp.json::<Value>().await {
        Ok(body) => parse_identity(&body)
            .unwrap_or_else(|| IdentityState::Unexpected(format!("unrecognized body: {body}"))),
        Err(e) => IdentityState::Unexpected(root_cause(&e)),
    }
}

fn parse_identity(body: &Value) -> Option<IdentityState> {
    let identity = body.get("identity")?;
    if identity.as_str() == Some("admin") {
        return Some(IdentityState::Admin);
    }
    let worker = identity.get("worker")?.as_str()?;
    Some(IdentityState::Worker(worker.to_string()))
}

/// Run every check against the config at `config_path` and return the report.
///
/// Checks that depend on an earlier one are skipped rather than guessed: without
/// a usable config there is nothing to check at all, an unreachable server skips
/// the identity check, and a worker that has not joined skips it too rather than
/// reporting a 401 it was always going to get.
pub async fn diagnose(config_path: PathBuf) -> Report {
    let mut checks = vec![check_agent_binary(
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string())
            .as_deref(),
    )];

    let state = observe_config(config_path);
    checks.push(check_config(&state));

    let ConfigState::Present { config: cfg, .. } = &state else {
        // Everything below reads the config. Say so once instead of repeating
        // "no config" on six lines.
        for name in ["joined", "server", "reachable", "identity", "capacity"] {
            checks.push(Check::skip(name, "no usable config"));
        }
        checks.push(check_runtime(&observe_runtime()));
        checks.push(check_agent(&observe_agent()));
        return Report { checks };
    };

    checks.push(check_joined(cfg));
    checks.push(Check::pass("server", cfg.server.clone()));

    let http = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .unwrap_or_default();
    let reach = observe_reachability(&http, &cfg.server).await;
    checks.push(check_reachable(&cfg.server, &reach));

    match (&reach, &cfg.credential) {
        (Reachability::Reachable, Some(credential)) => {
            let identity = observe_identity(&http, &cfg.server, credential).await;
            checks.push(check_identity(&cfg.node, &identity));
        }
        (Reachability::Reachable, None) => {
            checks.push(Check::skip("identity", "this worker has not joined yet"));
        }
        _ => checks.push(Check::skip("identity", "server unreachable")),
    }

    checks.push(check_capacity(&observe_capacity(cfg)));
    checks.push(check_runtime(&observe_runtime()));
    checks.push(check_agent(&observe_agent()));
    Report { checks }
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
mod tests {
    use super::*;

    fn cfg() -> WorkerConfig {
        WorkerConfig {
            server: "http://h:8088".to_string(),
            node: "node-a".to_string(),
            credential: Some("node-a.secret".to_string()),
            cpu: 4,
            memory: Memory::from_bytes(8 * 1024 * 1024 * 1024),
            reconcile_secs: 5,
            heartbeat_secs: 10,
            lease_secs: 40,
        }
    }

    #[test]
    fn a_missing_config_says_to_run_setup() {
        let check = check_config(&ConfigState::Missing("/tmp/x.json".into()));
        assert_eq!(check.status, Status::Fail);
        assert!(check.hint.unwrap().contains("veloslet setup"));
    }

    #[test]
    fn a_group_readable_config_is_a_warning_not_a_failure() {
        // It still works — it is just leaking the credential, so the worker is
        // usable and the report should not claim otherwise.
        let check = check_config(&ConfigState::Present {
            path: "/tmp/x.json".into(),
            mode: Some(0o100644),
            config: Box::new(cfg()),
        });
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("644"), "{}", check.detail);
        assert_eq!(
            check_config(&ConfigState::Present {
                path: "/tmp/x.json".into(),
                mode: Some(0o100600),
                config: Box::new(cfg()),
            })
            .status,
            Status::Pass
        );
    }

    #[test]
    fn an_unjoined_worker_fails_the_joined_check() {
        let mut unjoined = cfg();
        unjoined.credential = None;
        assert_eq!(check_joined(&unjoined).status, Status::Fail);
        assert_eq!(check_joined(&cfg()).status, Status::Pass);
    }

    #[test]
    fn https_without_tls_is_told_apart_from_a_server_that_is_down() {
        assert_eq!(
            classify_transport("https://velos.example", "scheme is not http".to_string()),
            Reachability::NoTlsSupport
        );
        // The same cause on an http URL is not a TLS problem.
        assert_eq!(
            classify_transport("http://velos.example", "scheme is not http".to_string()),
            Reachability::Unreachable("scheme is not http".to_string())
        );
        assert_eq!(
            classify_transport("https://velos.example", "connection refused".to_string()),
            Reachability::Unreachable("connection refused".to_string())
        );
    }

    #[test]
    fn a_credential_for_another_node_is_reported_as_such() {
        // The failure mode of hand-editing `node` after joining. Without this
        // arm it would read as a plain 401 with no clue why.
        let check = check_identity("node-a", &IdentityState::Worker("node-b".to_string()));
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("node-b"), "{}", check.detail);
        assert!(check.detail.contains("node-a"), "{}", check.detail);
    }

    #[test]
    fn a_matching_identity_passes() {
        assert_eq!(
            check_identity("node-a", &IdentityState::Worker("node-a".to_string())).status,
            Status::Pass
        );
    }

    #[test]
    fn a_revoked_credential_says_the_worker_was_deleted() {
        let check = check_identity("node-a", &IdentityState::Rejected(401));
        assert_eq!(check.status, Status::Fail);
        assert!(check.hint.unwrap().contains("veloslet setup"));
    }

    #[test]
    fn identity_parses_both_shapes() {
        assert_eq!(
            parse_identity(&serde_json::json!({ "identity": "admin" })),
            Some(IdentityState::Admin)
        );
        assert_eq!(
            parse_identity(&serde_json::json!({ "identity": { "worker": "w1" } })),
            Some(IdentityState::Worker("w1".to_string()))
        );
        assert_eq!(parse_identity(&serde_json::json!({ "nope": 1 })), None);
    }

    #[test]
    fn capacity_that_exceeds_the_host_fails_with_the_reason() {
        let check = check_capacity(&CapacityState::Exceeds(
            "requested 99 cores but machine has 10".to_string(),
        ));
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("99"), "{}", check.detail);
    }

    #[test]
    fn a_stale_launch_agent_label_is_surfaced() {
        // The real case: an agent installed before the bundle id was renamed is
        // running, but `stop`/`uninstall` target the new label and miss it, so
        // "not loaded" would be actively misleading.
        let check = check_agent(&AgentState::StaleLabel(
            "io.github.zhxiaogg.velos-worker".to_string(),
        ));
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("zhxiaogg"), "{}", check.detail);
        assert!(check.hint.unwrap().contains("launchctl unload"));
    }

    #[test]
    fn two_loaded_agents_are_a_failure_not_a_pass() {
        // Seen for real: `run -d` loaded the current label while a pre-rename
        // agent was still loaded, so two workers shared one config and one node
        // name and reaped each other's containers. Reporting a plain pass here
        // hid an actively broken machine.
        let check = check_agent(&AgentState::Duplicate(
            "io.github.zhxiaogg.velos-worker".to_string(),
        ));
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("two workers"), "{}", check.detail);
        assert!(check.detail.contains("zhxiaogg"), "{}", check.detail);
        assert!(check.hint.unwrap().contains("launchctl unload"));
    }

    #[test]
    fn the_report_counts_failures_and_warnings_separately() {
        let report = Report {
            checks: vec![
                Check::pass("a", "ok"),
                Check::warn("b", "meh", "do x"),
                Check::fail("c", "broken", "do y"),
            ],
        };
        assert!(report.has_failures());
        let text = report.to_string();
        assert!(text.contains("1 failed, 1 warning"), "{text}");

        let clean = Report {
            checks: vec![Check::pass("a", "ok")],
        };
        assert!(!clean.has_failures());
        assert!(clean.to_string().contains("all checks passed"));

        // Warnings alone must not read as a broken setup.
        let warned = Report {
            checks: vec![Check::warn("b", "meh", "do x")],
        };
        assert!(!warned.has_failures());
        assert!(warned.to_string().contains("nothing broken"));
    }

    #[test]
    fn a_missing_config_still_reports_runtime_and_agent() {
        // The two checks that do not need a config are the ones a fresh machine
        // most wants: is there a container runtime, and is anything running.
        let report = futures_lite_block_on(diagnose("/tmp/veloslet-doctor-absent.json".into()));
        let names: Vec<&str> = report.checks.iter().map(|c| c.name).collect();
        assert!(names.contains(&"runtime"), "{names:?}");
        assert!(names.contains(&"background"), "{names:?}");
        assert!(report.has_failures());
        // Nothing that needs the config may be reported as passing.
        for c in &report.checks {
            if ["joined", "server", "reachable", "identity", "capacity"].contains(&c.name) {
                assert_eq!(c.status, Status::Skip, "{}", c.name);
            }
        }
    }

    /// Minimal block-on so this test needs no extra dev-dependency.
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
