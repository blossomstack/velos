//! Self-installation as a macOS launchd LaunchAgent.
//!
//! macOS "Local Network Privacy" silently blocks a bare LaunchAgent from
//! reaching a LAN server: a launchd job has no GUI-app ancestor for the system
//! to attribute (and prompt) the connection to, so it is denied with no UI.
//! The fix (Apple TN3179) is to give the worker a real *app-bundle identity*:
//! wrap the binary in a code-signed `.app` carrying a bundle id and an
//! `NSLocalNetworkUsageDescription`, then point the LaunchAgent at it via
//! `AssociatedBundleIdentifiers`. With that in place macOS shows the native
//! "… wants to access your local network" prompt on the first connection.
//!
//! This module holds the *pure* parts: rendering those files, the persisted
//! config type, and the decisions about which secret the worker presents and
//! what it writes back after registering. `main.rs` performs the filesystem,
//! HTTP, and `launchctl` side effects (Principle #5).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::memory::Memory;

/// The launchd label and app-bundle identifier for the worker daemon.
///
/// Reverse-DNS under the project's GitHub namespace (`io.github.<owner>`), which
/// the owner controls — unlike `com.velos.*`, an unowned domain. macOS pins the
/// Local Network privacy grant to this identity, so it must be stable.
pub const BUNDLE_ID: &str = "io.github.blossomstack.velos-worker";
/// Human-facing bundle name shown in the Local Network privacy prompt/list.
/// Distinct from any prior name so it is unmistakable in System Settings.
pub const BUNDLE_DISPLAY_NAME: &str = "Veloslet Worker";
/// The executable name inside the app bundle (`Velos.app/Contents/MacOS/<name>`).
pub const BUNDLE_EXECUTABLE: &str = "veloslet";

fn default_reconcile_secs() -> u64 {
    5
}
fn default_heartbeat_secs() -> u64 {
    10
}
fn default_lease_secs() -> u32 {
    40
}

/// The secret a worker presents when it registers.
///
/// The two variants are not interchangeable, and that is the point: only a
/// [`Bearer::Join`] registration mints a credential, so the join is one-shot
/// (Principle #2 — the illegal "join again with the same token forever" state
/// is not representable). Neither variant reveals itself through `Debug`
/// (Principle #1); callers must opt in via [`Bearer::expose`].
#[derive(Clone, PartialEq, Eq)]
pub enum Bearer {
    /// A one-shot join token minted by an admin. The registration it authorizes
    /// mints the worker's credential; it is never presented again afterwards.
    Join(String),
    /// The credential minted by a previous join. This — not the join token — is
    /// the durable secret on disk, and it is what every later start presents.
    Credential(String),
}

impl Bearer {
    /// Explicitly read the secret (to put it in an `Authorization` header).
    pub fn expose(&self) -> &str {
        match self {
            Bearer::Join(s) | Bearer::Credential(s) => s,
        }
    }
}

impl std::fmt::Debug for Bearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bearer::Join(_) => f.write_str("Join(***)"),
            Bearer::Credential(_) => f.write_str("Credential(***)"),
        }
    }
}

/// Why a worker cannot register, or cannot finish joining.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum JoinError {
    #[error(
        "this worker has not joined a control plane yet; \
         mint a join token with `velosctl token create`, then run \
         `veloslet setup --server <url> --node <name> --token <token> --cpu <n> --memory <size>`"
    )]
    NotConnected,
    #[error("the server accepted the join token but returned no worker credential")]
    NoCredentialIssued,
}

/// Persisted worker configuration (written as JSON to `~/.velos/veloslet.json`).
///
/// The credential lives here — not in the LaunchAgent's argument vector — so it
/// never shows up in the process table (`ps`). The file is created `0600`.
///
/// A join token is deliberately *not* a field. `veloslet setup` trades one for a
/// credential and never writes it down, so the illegal state "a worker still
/// holding a spent join token" cannot be represented (Principle #2), and an
/// expired token can never strand a worker that has already joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Server base URL, e.g. `http://192.168.68.60:8088`.
    pub server: String,
    /// This worker's name.
    pub node: String,
    /// The worker credential (`node.secret`) issued at join, re-presented on
    /// every later start. Revoking it is what evicts this worker for good.
    /// Absent until `veloslet setup` has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Advertised CPU cores. Required; validated against the host at startup.
    pub cpu: u32,
    /// Advertised memory (e.g. `"8G"`). Required; validated against the host.
    pub memory: Memory,
    #[serde(default = "default_reconcile_secs")]
    pub reconcile_secs: u64,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    #[serde(default = "default_lease_secs")]
    pub lease_secs: u32,
}

impl WorkerConfig {
    /// The secret this worker presents on every start. Fails closed when it has
    /// not joined yet (Principle #6) rather than registering anonymously.
    pub fn bearer(&self) -> Result<Bearer, JoinError> {
        self.credential
            .clone()
            .map(Bearer::Credential)
            .ok_or(JoinError::NotConnected)
    }

    /// Whether `veloslet setup` has been run against a control plane.
    pub fn is_connected(&self) -> bool {
        self.credential.is_some()
    }

    /// The config as it should be persisted after a successful join.
    pub fn with_credential(&self, credential: String) -> Self {
        Self {
            credential: Some(credential),
            ..self.clone()
        }
    }
}

/// Read the worker credential out of a registration response.
///
/// A [`Bearer::Join`] registration must come back with one — that exchange *is*
/// the join. An empty or absent `token` means the server accepted the join token
/// without minting anything, which would leave the worker unable to speak as
/// itself, so it is an error rather than a silently credential-less config.
pub fn credential_from_response(response: &Value) -> Result<String, JoinError> {
    response
        .get("token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or(JoinError::NoCredentialIssued)
}

/// Minimal XML text escaping for plist `<string>` values.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the app-bundle `Info.plist` that gives the worker a stable identity
/// plus the local-network usage string macOS shows the user.
pub fn render_info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Velos</string>
    <key>CFBundleDisplayName</key><string>{display}</string>
    <key>CFBundleIdentifier</key><string>{bundle_id}</string>
    <key>CFBundleExecutable</key><string>{exe}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>LSBackgroundOnly</key><true/>
    <key>NSLocalNetworkUsageDescription</key>
    <string>Velos Worker connects to the Velos control-plane server on your local network to register this machine and reconcile containers.</string>
</dict>
</plist>
"#,
        display = xml_escape(BUNDLE_DISPLAY_NAME),
        bundle_id = xml_escape(BUNDLE_ID),
        exe = xml_escape(BUNDLE_EXECUTABLE),
        version = xml_escape(version),
    )
}

/// Render the LaunchAgent plist. `AssociatedBundleIdentifiers` is what lets
/// Local Network Privacy attribute the agent's traffic to the signed bundle.
pub fn render_launch_agent(
    program_args: &[String],
    path_env: &str,
    stdout_path: &str,
    stderr_path: &str,
) -> String {
    let args_xml = program_args
        .iter()
        .map(|a| format!("        <string>{}</string>", xml_escape(a)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>

    <!-- Tell macOS which signed bundle is responsible for this agent's network
         access so Local Network Privacy can attribute (and prompt for) the
         connection instead of silently denying it. (Apple TN3179) -->
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>{label}</string>
    </array>

    <key>ProgramArguments</key>
    <array>
{args_xml}
    </array>

    <!-- launchd's default PATH is minimal; add /usr/local/bin so the Apple
         `container` CLI is discoverable by the runtime. -->
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path_env}</string>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>

    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#,
        label = xml_escape(BUNDLE_ID),
        args_xml = args_xml,
        path_env = xml_escape(path_env),
        stdout = xml_escape(stdout_path),
        stderr = xml_escape(stderr_path),
    )
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
mod tests {
    use super::*;

    /// A config as `veloslet config`/`setup` writes it before the join lands.
    fn unjoined() -> WorkerConfig {
        WorkerConfig {
            server: "http://192.168.68.60:8088".to_string(),
            node: "node-a".to_string(),
            credential: None,
            cpu: 4,
            memory: Memory::from_bytes(8 * 1024 * 1024 * 1024),
            reconcile_secs: 5,
            heartbeat_secs: 10,
            lease_secs: 40,
        }
    }

    #[test]
    fn config_roundtrips_through_json() {
        let cfg = unjoined();
        let text = serde_json::to_string(&cfg).unwrap();
        let back: WorkerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(cfg, back);

        let joined = cfg.with_credential("node-a.secret".to_string());
        let text = serde_json::to_string(&joined).unwrap();
        let back: WorkerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(joined, back);
    }

    #[test]
    fn config_applies_interval_defaults_when_omitted() {
        let cfg: WorkerConfig =
            serde_json::from_str(r#"{"server":"http://h:1","node":"n","cpu":4,"memory":"8G"}"#)
                .unwrap();
        assert_eq!(cfg.cpu, 4);
        assert_eq!(cfg.memory.bytes(), 8 * 1024 * 1024 * 1024);
        assert_eq!(cfg.reconcile_secs, 5);
        assert_eq!(cfg.heartbeat_secs, 10);
        assert_eq!(cfg.lease_secs, 40);
    }

    #[test]
    fn a_join_token_is_never_serialized_to_disk() {
        // The one-shot join, enforced by construction: `WorkerConfig` has no
        // field a join token could occupy, so no write can leak one. A worker
        // therefore cannot be stranded by its join token expiring.
        let joined = unjoined().with_credential("node-a.secret".to_string());
        let text = serde_json::to_string(&joined).unwrap();
        assert!(
            !text.contains("\"token\""),
            "a token field appeared in {text}"
        );
        assert!(text.contains("credential"));
    }

    #[test]
    fn an_unjoined_config_omits_the_credential_key_entirely() {
        // Not `"credential": null` — nothing at all, so a hand-inspected config
        // reads unambiguously as "this worker has not joined".
        let text = serde_json::to_string(&unjoined()).unwrap();
        assert!(!text.contains("credential"), "{text}");
    }

    #[test]
    fn bearer_presents_the_credential_once_joined() {
        let cfg = unjoined().with_credential("node-a.secret".to_string());
        assert!(cfg.is_connected());
        assert_eq!(
            cfg.bearer().unwrap(),
            Bearer::Credential("node-a.secret".to_string())
        );
    }

    #[test]
    fn bearer_fails_closed_before_the_worker_has_joined() {
        let cfg = unjoined();
        assert!(!cfg.is_connected());
        assert_eq!(cfg.bearer(), Err(JoinError::NotConnected));
    }

    #[test]
    fn with_credential_leaves_every_other_field_alone() {
        let cfg = unjoined();
        let joined = cfg.with_credential("node-a.secret".to_string());
        assert_eq!(joined.credential, Some("node-a.secret".to_string()));
        assert_eq!(joined.server, cfg.server);
        assert_eq!(joined.node, cfg.node);
        assert_eq!(joined.cpu, cfg.cpu);
        assert_eq!(joined.memory, cfg.memory);
        assert_eq!(joined.lease_secs, cfg.lease_secs);
    }

    #[test]
    fn a_join_response_without_a_credential_fails_closed() {
        assert_eq!(
            credential_from_response(&serde_json::json!({ "workerName": "node-a" })),
            Err(JoinError::NoCredentialIssued)
        );
        assert_eq!(
            credential_from_response(&serde_json::json!({ "token": "" })),
            Err(JoinError::NoCredentialIssued)
        );
        assert_eq!(
            credential_from_response(
                &serde_json::json!({ "workerName": "node-a", "token": "node-a.secret" })
            ),
            Ok("node-a.secret".to_string())
        );
    }

    #[test]
    fn bearer_never_leaks_through_debug() {
        assert_eq!(
            format!("{:?}", Bearer::Join("id.secret".into())),
            "Join(***)"
        );
        assert_eq!(
            format!("{:?}", Bearer::Credential("node-a.secret".into())),
            "Credential(***)"
        );
    }

    #[test]
    fn launch_agent_carries_associated_bundle_id_and_args() {
        let args = vec![
            "/Applications/Velos.app/Contents/MacOS/veloslet".to_string(),
            "run".to_string(),
            "--config".to_string(),
            "/home/u/.velos/veloslet.json".to_string(),
        ];
        let plist = render_launch_agent(&args, "/usr/local/bin:/usr/bin:/bin", "/o.log", "/e.log");
        assert!(plist.contains("<key>AssociatedBundleIdentifiers</key>"));
        assert!(plist.contains("<string>io.github.blossomstack.velos-worker</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(plist.contains("<string>/home/u/.velos/veloslet.json</string>"));
    }

    #[test]
    fn info_plist_declares_identity_and_local_network_usage() {
        let info = render_info_plist("0.1.1");
        assert!(info.contains(
            "<key>CFBundleIdentifier</key><string>io.github.blossomstack.velos-worker</string>"
        ));
        assert!(info.contains("<key>NSLocalNetworkUsageDescription</key>"));
    }

    #[test]
    fn xml_escape_neutralizes_markup() {
        assert_eq!(xml_escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}
