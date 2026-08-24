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
        "worker config holds neither a credential nor a join token; \
         mint one with `velosctl token create` and re-run `veloslet install --token <token>`"
    )]
    NoSecret,
    #[error("the server accepted the join token but returned no worker credential")]
    NoCredentialIssued,
}

/// Persisted worker configuration (written as JSON to `~/.velos/veloslet.json`).
///
/// The secrets live here — not in the LaunchAgent's argument vector — so they
/// never show up in the process table (`ps`). The file is created `0600`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Server base URL, e.g. `http://192.168.68.60:8088`.
    pub server: String,
    /// This worker's name.
    pub node: String,
    /// One-shot join token (`id.secret`), present only until this worker has
    /// joined. The first successful registration replaces it with `credential`,
    /// so an expired join token can never strand a worker that already joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The worker credential (`node.secret`) issued at join, re-presented on
    /// every later start. Revoking it is what evicts this worker for good.
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
    /// The secret to present at registration: the credential once this worker
    /// has joined, otherwise the one-shot join token. Fails closed when it holds
    /// neither (Principle #6) rather than registering anonymously.
    pub fn bearer(&self) -> Result<Bearer, JoinError> {
        match (&self.credential, &self.token) {
            (Some(c), _) => Ok(Bearer::Credential(c.clone())),
            (None, Some(t)) => Ok(Bearer::Join(t.clone())),
            (None, None) => Err(JoinError::NoSecret),
        }
    }

    /// Fold a successful registration response into the persisted config.
    ///
    /// A [`Bearer::Join`] registration must come back with a credential: it is
    /// stored and the join token erased, which is what consumes the join. A
    /// [`Bearer::Credential`] registration only refreshes what the worker
    /// advertises, so there is nothing to write back (`Ok(None)`).
    pub fn adopt(&self, bearer: &Bearer, response: &Value) -> Result<Option<Self>, JoinError> {
        match bearer {
            Bearer::Credential(_) => Ok(None),
            Bearer::Join(_) => {
                let credential = response
                    .get("token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or(JoinError::NoCredentialIssued)?;
                Ok(Some(Self {
                    token: None,
                    credential: Some(credential.to_string()),
                    ..self.clone()
                }))
            }
        }
    }
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

    /// A config as `veloslet install` writes it: a join token, no credential yet.
    fn unjoined() -> WorkerConfig {
        WorkerConfig {
            server: "http://192.168.68.60:8088".to_string(),
            node: "node-a".to_string(),
            token: Some("id.secret".to_string()),
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

        let joined = WorkerConfig {
            token: None,
            credential: Some("node-a.secret".to_string()),
            ..cfg
        };
        let text = serde_json::to_string(&joined).unwrap();
        let back: WorkerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(joined, back);
    }

    #[test]
    fn config_applies_interval_defaults_when_omitted() {
        let cfg: WorkerConfig = serde_json::from_str(
            r#"{"server":"http://h:1","node":"n","token":"t","cpu":4,"memory":"8G"}"#,
        )
        .unwrap();
        assert_eq!(cfg.cpu, 4);
        assert_eq!(cfg.memory.bytes(), 8 * 1024 * 1024 * 1024);
        assert_eq!(cfg.reconcile_secs, 5);
        assert_eq!(cfg.heartbeat_secs, 10);
        assert_eq!(cfg.lease_secs, 40);
    }

    #[test]
    fn a_consumed_join_token_is_not_serialized_back_to_disk() {
        // The whole point of the one-shot join: after adopting a credential the
        // written config must carry no join token at all, not an empty one.
        let joined = unjoined()
            .adopt(
                &Bearer::Join("id.secret".to_string()),
                &serde_json::json!({ "workerName": "node-a", "token": "node-a.secret" }),
            )
            .unwrap()
            .unwrap();
        let text = serde_json::to_string(&joined).unwrap();
        assert!(!text.contains("token"), "join token leaked into {text}");
        assert!(text.contains("credential"));
    }

    #[test]
    fn bearer_prefers_the_credential_over_a_leftover_join_token() {
        let cfg = WorkerConfig {
            credential: Some("node-a.secret".to_string()),
            ..unjoined()
        };
        // Even with a join token still on disk (e.g. the rewrite failed), a
        // joined worker presents its credential — never the token again.
        assert_eq!(
            cfg.bearer().unwrap(),
            Bearer::Credential("node-a.secret".to_string())
        );
    }

    #[test]
    fn bearer_uses_the_join_token_until_the_worker_has_joined() {
        assert_eq!(
            unjoined().bearer().unwrap(),
            Bearer::Join("id.secret".to_string())
        );
    }

    #[test]
    fn bearer_fails_closed_with_neither_secret() {
        let cfg = WorkerConfig {
            token: None,
            ..unjoined()
        };
        assert_eq!(cfg.bearer(), Err(JoinError::NoSecret));
    }

    #[test]
    fn adopt_stores_the_credential_and_erases_the_join_token() {
        let cfg = unjoined();
        let updated = cfg
            .adopt(
                &Bearer::Join("id.secret".to_string()),
                &serde_json::json!({ "workerName": "node-a", "token": "node-a.secret" }),
            )
            .unwrap()
            .expect("a join must be written back");
        assert_eq!(updated.token, None);
        assert_eq!(updated.credential, Some("node-a.secret".to_string()));
        // Everything else is untouched.
        assert_eq!(updated.server, cfg.server);
        assert_eq!(updated.node, cfg.node);
        assert_eq!(updated.lease_secs, cfg.lease_secs);
        assert_eq!(
            updated.bearer().unwrap(),
            Bearer::Credential("node-a.secret".to_string())
        );
    }

    #[test]
    fn adopt_writes_nothing_back_when_re_registering_with_a_credential() {
        let cfg = WorkerConfig {
            token: None,
            credential: Some("node-a.secret".to_string()),
            ..unjoined()
        };
        // A restart re-registers to refresh what it advertises; the server
        // issues no new credential, so there is nothing to persist.
        let updated = cfg
            .adopt(
                &Bearer::Credential("node-a.secret".to_string()),
                &serde_json::json!({ "workerName": "node-a" }),
            )
            .unwrap();
        assert_eq!(updated, None);
    }

    #[test]
    fn adopt_fails_closed_when_a_join_returns_no_credential() {
        let cfg = unjoined();
        let join = Bearer::Join("id.secret".to_string());
        assert_eq!(
            cfg.adopt(&join, &serde_json::json!({ "workerName": "node-a" })),
            Err(JoinError::NoCredentialIssued)
        );
        assert_eq!(
            cfg.adopt(&join, &serde_json::json!({ "token": "" })),
            Err(JoinError::NoCredentialIssued)
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
