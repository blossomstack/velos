//! `velosctl doctor` — one command that answers "is my setup working, and if
//! not, what do I fix?".
//!
//! The logic is split in two. An *observation* enum records what was seen (a
//! config file's mode, an HTTP status, the identity a token maps to), and a
//! pure function turns each observation into a [`Check`] carrying a verdict and
//! a fix-it hint. Only [`diagnose`] touches the filesystem, the network, or
//! another process, so every verdict and every hint is unit-testable.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::{Config, Source};

/// How long any single probe of the control plane may take. A doctor that hangs
/// is worse than one that reports "unreachable".
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// The `container` CLI (Apple Containerization) — the runtime a worker needs.
/// Matches the binary `velos-runtime`'s `AppleContainer` shells out to.
const RUNTIME_BIN: &str = "container";

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
    /// Broken — this is why Velos isn't usable.
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

/// What `~/.velos/config` looked like on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigState {
    /// No home directory, so there is nowhere for a config to live.
    NoHome,
    /// The path is free — nothing has been saved yet.
    Missing(PathBuf),
    /// Present but unreadable or not valid JSON.
    Unusable { path: PathBuf, error: String },
    /// Present and parsed. `mode` is the unix permission bits, where known.
    Present { path: PathBuf, mode: Option<u32> },
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

/// Whether the admin account has been created (`GET /auth/v1/status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitState {
    Initialized,
    Uninitialized,
    Unexpected(String),
}

/// Who the saved credential turns out to be (`GET /auth/v1/me`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityState {
    Admin,
    Worker(String),
    Rejected(u16),
    Unexpected(String),
}

/// What a list of workers looks like (`GET /api/v1/workers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiAccess {
    Denied(u16),
    Workers { total: usize, ready: usize },
    Unexpected(String),
}

/// Whether this machine can run containers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeState {
    Absent,
    Available(String),
    Broken(String),
}

// ---------------------------------------------------------------------------
// Pure interpreters — observation to verdict
// ---------------------------------------------------------------------------

fn check_cli(version: &str, path: Option<&str>) -> Check {
    match path {
        Some(p) => Check::pass("velosctl", format!("v{version} ({p})")),
        None => Check::pass("velosctl", format!("v{version}")),
    }
}

fn check_config(state: &ConfigState) -> Check {
    match state {
        ConfigState::NoHome => Check::fail(
            "config file",
            "no home directory, so no config can be saved",
            "set HOME, or pass --server/--token on every command",
        ),
        ConfigState::Missing(path) => Check::warn(
            "config file",
            format!("{} not written yet", path.display()),
            "run `velosctl login --token <token> --server <url>` to save one",
        ),
        ConfigState::Unusable { path, error } => Check::fail(
            "config file",
            format!("{} is unusable: {error}", path.display()),
            "delete it and run `velosctl login` again",
        ),
        // The file holds a bearer token, so group/other access is a real leak.
        ConfigState::Present { path, mode } => match mode {
            Some(m) if m & 0o077 != 0 => Check::warn(
                "config file",
                format!(
                    "{} is mode {:04o} — readable by other users",
                    path.display(),
                    m
                ),
                format!("chmod 600 {}", path.display()),
            ),
            Some(m) => Check::pass(
                "config file",
                format!("{} (mode {:04o})", path.display(), m),
            ),
            None => Check::pass("config file", path.display().to_string()),
        },
    }
}

fn check_server(server: &str, source: Source) -> Check {
    Check::pass("server url", format!("{server} (from {source})"))
}

fn check_credential(source: Option<Source>) -> Check {
    match source {
        Some(s) => Check::pass("credential", format!("present (from {s})")),
        None => Check::fail(
            "credential",
            "none — every API call will be rejected",
            "run `velosctl login --token <token> --server <url>`",
        ),
    }
}

fn check_reachable(server: &str, reach: &Reachability) -> Check {
    match reach {
        Reachability::Reachable => Check::pass("reachable", format!("{server} answered /healthz")),
        Reachability::NoTlsSupport => Check::fail(
            "reachable",
            format!("{server} is https, and this velosctl was built without TLS support"),
            "use an http:// URL, or tunnel to the server (ssh -L 8080:127.0.0.1:8080 <host>)",
        ),
        Reachability::Unreachable(why) => Check::fail(
            "reachable",
            format!("{server}: {why}"),
            "start the control plane with `velos-server`, or point at another one with --server",
        ),
    }
}

fn check_initialized(server: &str, state: &InitState) -> Check {
    match state {
        InitState::Initialized => Check::pass("initialized", "the admin account exists"),
        InitState::Uninitialized => Check::fail(
            "initialized",
            "no admin account — the server rejects everything until first-run setup",
            format!("open {server} and create the admin account"),
        ),
        InitState::Unexpected(what) => Check::fail(
            "initialized",
            format!("could not read the setup state: {what}"),
            "check that --server points at a Velos control plane",
        ),
    }
}

fn check_identity(state: &IdentityState) -> Check {
    match state {
        IdentityState::Admin => Check::pass("identity", "admin"),
        IdentityState::Worker(name) => Check::warn(
            "identity",
            format!("worker '{name}' — a worker credential, not an admin one"),
            "log in with an admin CLI token to manage the cluster",
        ),
        IdentityState::Rejected(code) => Check::fail(
            "identity",
            format!("the server rejected this credential (HTTP {code})"),
            "mint a fresh CLI token in the dashboard and run `velosctl login` again",
        ),
        IdentityState::Unexpected(what) => Check::fail(
            "identity",
            format!("unexpected reply from /auth/v1/me: {what}"),
            "check that --server points at a Velos control plane",
        ),
    }
}

fn check_api(access: &ApiAccess) -> Check {
    match access {
        ApiAccess::Workers { total: 0, .. } => Check::warn(
            "api access",
            "no workers registered",
            "register one with `veloslet run --server <url> --node <name> --token <bootstrap>`",
        ),
        ApiAccess::Workers { total, ready } if *ready == 0 => Check::warn(
            "api access",
            format!("none of {} Ready", plural(*total, "worker")),
            "check `velosctl get workers` — a stale lease marks a worker NotReady",
        ),
        ApiAccess::Workers { total, ready } => Check::pass(
            "api access",
            format!("{ready} of {} Ready", plural(*total, "worker")),
        ),
        ApiAccess::Denied(code) => Check::fail(
            "api access",
            format!("/api/v1/workers returned HTTP {code}"),
            "the credential is not allowed to read workers — log in as admin",
        ),
        ApiAccess::Unexpected(what) => Check::fail(
            "api access",
            format!("unexpected reply from /api/v1/workers: {what}"),
            "check that --server points at a Velos control plane",
        ),
    }
}

fn check_runtime(state: &RuntimeState) -> Check {
    match state {
        RuntimeState::Available(version) => Check::pass("runtime", version.clone()),
        // Only a worker host needs the runtime, so its absence is never fatal.
        RuntimeState::Absent => Check::warn(
            "runtime",
            format!("`{RUNTIME_BIN}` not on PATH"),
            "only needed on machines that run workers — install Apple's container CLI there",
        ),
        RuntimeState::Broken(why) => Check::warn(
            "runtime",
            format!("`{RUNTIME_BIN}` failed: {why}"),
            "containers will not start on this machine until it runs",
        ),
    }
}

/// Count workers and how many are Ready, from a `{ "items": [...] }` list.
fn count_workers(body: &Value) -> Option<(usize, usize)> {
    let items = body.get("items")?.as_array()?;
    let ready = items.iter().filter(|w| is_ready(w)).count();
    Some((items.len(), ready))
}

/// A worker is Ready when it carries a true `Ready` condition.
fn is_ready(worker: &Value) -> bool {
    worker
        .get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|c| {
                c.get("conditionType").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_bool) == Some(true)
            })
        })
}

/// Read the identity out of `{"identity": "admin"}` / `{"identity": {"worker": n}}`.
fn parse_identity(body: &Value) -> Option<IdentityState> {
    let identity = body.get("identity")?;
    if identity.as_str() == Some("admin") {
        return Some(IdentityState::Admin);
    }
    let worker = identity.get("worker")?.as_str()?;
    Some(IdentityState::Worker(worker.to_string()))
}

// ---------------------------------------------------------------------------
// Probes — the only side-effecting code
// ---------------------------------------------------------------------------

/// How the CLI resolved its settings, as `doctor` needs to report them.
pub struct Environment<'a> {
    pub server: &'a str,
    pub server_source: Source,
    /// The bearer credential and where it came from, when there is one.
    pub token: Option<(&'a str, Source)>,
}

fn url(server: &str, path: &str) -> String {
    format!("{}{path}", server.trim_end_matches('/'))
}

/// Inspect `~/.velos/config` without going through [`crate::load_config`],
/// which deliberately hides every error behind defaults.
fn observe_config() -> ConfigState {
    let Some(path) = crate::config_path() else {
        return ConfigState::NoHome;
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigState::Missing(path),
        Err(e) => {
            return ConfigState::Unusable {
                path,
                error: e.to_string(),
            };
        }
    };
    if let Err(e) = serde_json::from_str::<Config>(&contents) {
        return ConfigState::Unusable {
            path,
            error: e.to_string(),
        };
    }
    ConfigState::Present {
        path: path.clone(),
        mode: file_mode(&path),
    }
}

#[cfg(unix)]
fn file_mode(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_path: &std::path::Path) -> Option<u32> {
    None
}

/// Run `container --version`, the same probe a worker makes at startup.
fn observe_runtime() -> RuntimeState {
    match Command::new(RUNTIME_BIN).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text.lines().next().unwrap_or_default().trim();
            RuntimeState::Available(line.to_string())
        }
        Ok(out) => RuntimeState::Broken(format!("exited with {}", out.status)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RuntimeState::Absent,
        Err(e) => RuntimeState::Broken(e.to_string()),
    }
}

async fn observe_reachability(http: &reqwest::Client, server: &str) -> Reachability {
    match http.get(url(server, "/healthz")).send().await {
        Ok(resp) if resp.status().is_success() => Reachability::Reachable,
        Ok(resp) => Reachability::Unreachable(format!("HTTP {}", resp.status())),
        Err(e) => classify_transport(server, root_cause(&e)),
    }
}

/// Tell a missing TLS backend apart from a server that is simply down: without
/// one, reqwest rejects an https URL up front with "scheme is not http" rather
/// than failing to connect. A TLS-capable build never produces that error, so
/// this classification stays correct if the backend is ever added.
fn classify_transport(server: &str, cause: String) -> Reachability {
    if server.starts_with("https://") && cause.contains("scheme is not http") {
        return Reachability::NoTlsSupport;
    }
    Reachability::Unreachable(cause)
}

async fn observe_init(http: &reqwest::Client, server: &str) -> InitState {
    let resp = match http.get(url(server, "/auth/v1/status")).send().await {
        Ok(r) => r,
        Err(e) => return InitState::Unexpected(root_cause(&e)),
    };
    if !resp.status().is_success() {
        return InitState::Unexpected(format!("HTTP {}", resp.status()));
    }
    match resp.json::<Value>().await {
        Ok(body) => match body.get("initialized").and_then(Value::as_bool) {
            Some(true) => InitState::Initialized,
            Some(false) => InitState::Uninitialized,
            None => InitState::Unexpected("no `initialized` field".to_string()),
        },
        Err(e) => InitState::Unexpected(root_cause(&e)),
    }
}

async fn observe_identity(http: &reqwest::Client, server: &str, token: &str) -> IdentityState {
    let resp = match http
        .get(url(server, "/auth/v1/me"))
        .bearer_auth(token)
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

async fn observe_api(http: &reqwest::Client, server: &str, token: &str) -> ApiAccess {
    let resp = match http
        .get(url(server, "/api/v1/workers"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ApiAccess::Unexpected(root_cause(&e)),
    };
    let status = resp.status();
    if !status.is_success() {
        return ApiAccess::Denied(status.as_u16());
    }
    match resp.json::<Value>().await {
        Ok(body) => match count_workers(&body) {
            Some((total, ready)) => ApiAccess::Workers { total, ready },
            None => ApiAccess::Unexpected(format!("unrecognized body: {body}")),
        },
        Err(e) => ApiAccess::Unexpected(root_cause(&e)),
    }
}

/// Run every check against `env` and return the report.
///
/// Checks that depend on an earlier one are skipped rather than guessed: an
/// unreachable server skips the three checks that would have to talk to it, a
/// missing credential skips the two that would have to present one, and a
/// rejected credential skips the API check that would only repeat its 401.
pub async fn diagnose(env: &Environment<'_>) -> Report {
    let mut checks = vec![
        check_cli(
            env!("CARGO_PKG_VERSION"),
            std::env::current_exe()
                .ok()
                .map(|p| p.display().to_string())
                .as_deref(),
        ),
        check_config(&observe_config()),
        check_server(env.server, env.server_source),
        check_credential(env.token.map(|(_, source)| source)),
    ];

    let http = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            checks.push(Check::fail(
                "reachable",
                format!("could not build an HTTP client: {e}"),
                "this is a bug — please report it",
            ));
            return Report { checks };
        }
    };

    let reach = observe_reachability(&http, env.server).await;
    checks.push(check_reachable(env.server, &reach));

    match (&reach, env.token) {
        (Reachability::Unreachable(_) | Reachability::NoTlsSupport, _) => {
            checks.push(Check::skip("initialized", "server unreachable"));
            checks.push(Check::skip("identity", "server unreachable"));
            checks.push(Check::skip("api access", "server unreachable"));
        }
        (Reachability::Reachable, None) => {
            checks.push(check_initialized(
                env.server,
                &observe_init(&http, env.server).await,
            ));
            checks.push(Check::skip("identity", "no credential to check"));
            checks.push(Check::skip("api access", "no credential to check"));
        }
        (Reachability::Reachable, Some((token, _))) => {
            checks.push(check_initialized(
                env.server,
                &observe_init(&http, env.server).await,
            ));
            let identity = observe_identity(&http, env.server, token).await;
            let rejected = matches!(identity, IdentityState::Rejected(_));
            checks.push(check_identity(&identity));
            if rejected {
                checks.push(Check::skip("api access", "the credential was rejected"));
            } else {
                checks.push(check_api(&observe_api(&http, env.server, token).await));
            }
        }
    }

    checks.push(check_runtime(&observe_runtime()));
    Report { checks }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn find<'a>(report: &'a Report, name: &str) -> &'a Check {
        report.checks.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn a_world_readable_config_warns_with_a_chmod_hint() {
        let check = check_config(&ConfigState::Present {
            path: PathBuf::from("/home/u/.velos/config"),
            mode: Some(0o644),
        });
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("0644"), "{}", check.detail);
        assert_eq!(
            check.hint.as_deref(),
            Some("chmod 600 /home/u/.velos/config")
        );
    }

    #[test]
    fn a_private_config_passes() {
        let check = check_config(&ConfigState::Present {
            path: PathBuf::from("/home/u/.velos/config"),
            mode: Some(0o600),
        });
        assert_eq!(check.status, Status::Pass);
        assert!(check.hint.is_none());
    }

    #[test]
    fn a_missing_config_warns_but_an_unusable_one_fails() {
        let missing = check_config(&ConfigState::Missing(PathBuf::from(
            "/home/u/.velos/config",
        )));
        assert_eq!(missing.status, Status::Warn);
        let unusable = check_config(&ConfigState::Unusable {
            path: PathBuf::from("/home/u/.velos/config"),
            error: "expected value at line 1".to_string(),
        });
        assert_eq!(unusable.status, Status::Fail);
    }

    #[test]
    fn a_missing_credential_fails_and_points_at_login() {
        let check = check_credential(None);
        assert_eq!(check.status, Status::Fail);
        assert!(check.hint.unwrap().contains("velosctl login"));
        assert_eq!(check_credential(Some(Source::Config)).status, Status::Pass);
    }

    #[test]
    fn an_uninitialized_server_fails_but_a_worker_token_only_warns() {
        let uninitialized = check_initialized("http://h:8080", &InitState::Uninitialized);
        assert_eq!(uninitialized.status, Status::Fail);
        assert!(
            uninitialized
                .hint
                .unwrap_or_default()
                .contains("http://h:8080"),
            "the hint should name the server to open"
        );
        assert_eq!(
            check_initialized("http://h:8080", &InitState::Initialized).status,
            Status::Pass
        );
        assert_eq!(check_identity(&IdentityState::Admin).status, Status::Pass);
        assert_eq!(
            check_identity(&IdentityState::Worker("w1".into())).status,
            Status::Warn
        );
        assert_eq!(
            check_identity(&IdentityState::Rejected(401)).status,
            Status::Fail
        );
    }

    #[test]
    fn no_ready_workers_warns_rather_than_fails() {
        let empty = check_api(&ApiAccess::Workers { total: 0, ready: 0 });
        assert_eq!(empty.status, Status::Warn);
        assert_eq!(empty.detail, "no workers registered");

        let none_ready = check_api(&ApiAccess::Workers { total: 2, ready: 0 });
        assert_eq!(none_ready.status, Status::Warn);
        assert_eq!(none_ready.detail, "none of 2 workers Ready");

        let some_ready = check_api(&ApiAccess::Workers { total: 2, ready: 1 });
        assert_eq!(some_ready.status, Status::Pass);
        assert_eq!(some_ready.detail, "1 of 2 workers Ready");

        assert_eq!(check_api(&ApiAccess::Denied(403)).status, Status::Fail);
    }

    #[test]
    fn a_missing_container_runtime_is_only_a_warning() {
        // The control plane and CLI run fine without it; only workers need it.
        assert_eq!(check_runtime(&RuntimeState::Absent).status, Status::Warn);
        assert_eq!(
            check_runtime(&RuntimeState::Available("container CLI 1.0".into())).status,
            Status::Pass
        );
    }

    #[test]
    fn ready_is_counted_from_the_ready_condition_only() {
        let body = json!({ "items": [
            { "metadata": { "name": "a" },
              "status": { "conditions": [{ "conditionType": "Ready", "status": true }] } },
            { "metadata": { "name": "b" },
              "status": { "conditions": [{ "conditionType": "Ready", "status": false }] } },
            { "metadata": { "name": "c" }, "status": { "conditions": [] } },
        ]});
        assert_eq!(count_workers(&body), Some((3, 1)));
        assert_eq!(count_workers(&json!({ "items": [] })), Some((0, 0)));
        assert_eq!(count_workers(&json!({ "oops": 1 })), None);
    }

    #[test]
    fn identity_parses_both_admin_and_worker_shapes() {
        assert_eq!(
            parse_identity(&json!({ "identity": "admin" })),
            Some(IdentityState::Admin)
        );
        assert_eq!(
            parse_identity(&json!({ "identity": { "worker": "node-1" } })),
            Some(IdentityState::Worker("node-1".to_string()))
        );
        assert_eq!(parse_identity(&json!({ "identity": 7 })), None);
    }

    #[test]
    fn an_https_url_without_a_tls_backend_is_named_as_such() {
        assert_eq!(
            classify_transport(
                "https://velos.example",
                "invalid URL, scheme is not http".into()
            ),
            Reachability::NoTlsSupport
        );
        // A real connect failure, and any http URL, stay ordinary.
        assert_eq!(
            classify_transport("https://velos.example", "Connection refused".into()),
            Reachability::Unreachable("Connection refused".to_string())
        );
        assert_eq!(
            classify_transport("http://velos.example", "Connection refused".into()),
            Reachability::Unreachable("Connection refused".to_string())
        );
        let check = check_reachable("https://velos.example", &Reachability::NoTlsSupport);
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("TLS"), "{}", check.detail);
    }

    #[test]
    fn urls_survive_a_trailing_slash() {
        assert_eq!(
            url("http://h:8080/", "/healthz"),
            "http://h:8080/healthz".to_string()
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_skips_the_checks_that_need_it() {
        // Port 1 on loopback refuses connections, so this exercises the real
        // probe path without a server.
        let env = Environment {
            server: "http://127.0.0.1:1",
            server_source: Source::Flag,
            token: Some(("tok", Source::Flag)),
        };
        let report = diagnose(&env).await;

        assert_eq!(find(&report, "reachable").status, Status::Fail);
        for skipped in ["initialized", "identity", "api access"] {
            assert_eq!(
                find(&report, skipped).status,
                Status::Skip,
                "{skipped} should be skipped when the server is unreachable"
            );
        }
        assert!(report.has_failures());
        // Every check must be rendered, skips included — a silently dropped
        // check reads as a clean bill of health.
        let rendered = report.to_string();
        for check in &report.checks {
            assert!(rendered.contains(check.name), "{} missing", check.name);
        }
    }

    #[test]
    fn only_failures_make_the_report_fail() {
        let warn_only = Report {
            checks: vec![
                Check::pass("a", "fine"),
                Check::warn("b", "meh", "do something"),
                Check::skip("c", "not checked"),
            ],
        };
        assert!(!warn_only.has_failures());
        assert!(warn_only.to_string().contains("nothing broken"));

        let broken = Report {
            checks: vec![Check::fail("a", "bad", "fix it")],
        };
        assert!(broken.has_failures());
        assert!(broken.to_string().contains("fix it"));
    }
}
