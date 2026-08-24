//! Locating, reading, writing and editing `~/.velos/veloslet.json`.
//!
//! The config is the worker's whole identity: where its control plane is, what
//! it advertises, and — once `veloslet setup` has joined — the credential it
//! speaks with. That credential is why every write here goes through
//! [`save`], which creates the file `0600` and its directory `0700`.
//!
//! Fields are addressed by a closed [`Field`] enum rather than by string key.
//! A typo is then a parse error naming the valid fields, not a silently written
//! entry that only explodes later when `run` deserializes the file
//! (Principles #2 and #4).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::daemon::WorkerConfig;
use crate::memory::Memory;

/// What `config show` prints in place of the credential. Deliberately not a
/// fixed-width mask: the length of a secret is itself information.
const REDACTED: &str = "<set — hidden>";

/// The config path used when `--config` is not given.
pub fn default_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".velos").join("veloslet.json"))
}

/// Read and parse the config, with an error that says what to do when the file
/// simply is not there yet — the state every new machine starts in.
pub fn load(path: &Path) -> Result<WorkerConfig> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "no worker config at {} — run `veloslet setup --server <url> --node <name> \
                 --token <token> --cpu <n> --memory <size>` first",
                path.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("reading config {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

/// Persist the config as `0600` inside a `0700` directory.
pub fn save(path: &Path, cfg: &WorkerConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 700 {}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(cfg).context("serializing worker config")?;
    fs::write(path, format!("{json}\n")).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))?;
    Ok(())
}

/// A settable/readable config field.
///
/// The credential is absent by design: it is *earned* by `veloslet setup`, not
/// declared. Allowing it to be typed in would make a worker identity forgeable
/// by hand and put a secret in the shell history and the process table.
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
pub enum Field {
    Server,
    Node,
    Cpu,
    Memory,
    ReconcileSecs,
    HeartbeatSecs,
    LeaseSecs,
}

impl Field {
    /// The field's current value, rendered the same way it is stored.
    pub fn read(self, cfg: &WorkerConfig) -> String {
        match self {
            Field::Server => cfg.server.clone(),
            Field::Node => cfg.node.clone(),
            Field::Cpu => cfg.cpu.to_string(),
            Field::Memory => cfg.memory.to_string(),
            Field::ReconcileSecs => cfg.reconcile_secs.to_string(),
            Field::HeartbeatSecs => cfg.heartbeat_secs.to_string(),
            Field::LeaseSecs => cfg.lease_secs.to_string(),
        }
    }
}

/// The edits `config set` applies. Every field is optional; applying none at all
/// is rejected rather than silently rewriting the file unchanged.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct Edits {
    /// Server base URL, e.g. http://192.168.68.60:8088
    #[arg(long)]
    pub server: Option<String>,
    /// This worker's name.
    #[arg(long)]
    pub node: Option<String>,
    /// Advertised CPU cores.
    #[arg(long)]
    pub cpu: Option<u32>,
    /// Advertised memory, e.g. 8G.
    #[arg(long)]
    pub memory: Option<Memory>,
    /// Reconcile interval in seconds.
    #[arg(long)]
    pub reconcile_secs: Option<u64>,
    /// Heartbeat (lease renew) interval in seconds.
    #[arg(long)]
    pub heartbeat_secs: Option<u64>,
    /// Lease duration in seconds.
    #[arg(long)]
    pub lease_secs: Option<u32>,
}

impl Edits {
    /// Whether any field was actually supplied.
    pub fn is_empty(&self) -> bool {
        self.server.is_none()
            && self.node.is_none()
            && self.cpu.is_none()
            && self.memory.is_none()
            && self.reconcile_secs.is_none()
            && self.heartbeat_secs.is_none()
            && self.lease_secs.is_none()
    }

    /// The fields this would change, for the "set X, Y" confirmation line.
    pub fn touched(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.server.is_some() {
            names.push("server");
        }
        if self.node.is_some() {
            names.push("node");
        }
        if self.cpu.is_some() {
            names.push("cpu");
        }
        if self.memory.is_some() {
            names.push("memory");
        }
        if self.reconcile_secs.is_some() {
            names.push("reconcile-secs");
        }
        if self.heartbeat_secs.is_some() {
            names.push("heartbeat-secs");
        }
        if self.lease_secs.is_some() {
            names.push("lease-secs");
        }
        names
    }

    /// Apply these edits to a config.
    ///
    /// Renaming a joined worker is refused: the credential is bound to the name
    /// the server issued it for, so a rename would leave the worker
    /// authenticating as a node that no longer matches its config — a 401 loop
    /// whose cause is nowhere in the message. Fail closed and say to re-join
    /// (Principle #6).
    pub fn apply(self, cfg: &WorkerConfig) -> Result<WorkerConfig> {
        if let Some(node) = &self.node
            && cfg.is_connected()
            && node != &cfg.node
        {
            bail!(
                "this worker joined as {:?}, and its credential is bound to that name — \
                 renaming it here would leave it unable to authenticate. \
                 Run `veloslet setup --node {} --token <new token> ...` to join again as {:?}",
                cfg.node,
                node,
                node
            );
        }
        let mut next = cfg.clone();
        if let Some(v) = self.server {
            next.server = v;
        }
        if let Some(v) = self.node {
            next.node = v;
        }
        if let Some(v) = self.cpu {
            next.cpu = v;
        }
        if let Some(v) = self.memory {
            next.memory = v;
        }
        if let Some(v) = self.reconcile_secs {
            next.reconcile_secs = v;
        }
        if let Some(v) = self.heartbeat_secs {
            next.heartbeat_secs = v;
        }
        if let Some(v) = self.lease_secs {
            next.lease_secs = v;
        }
        Ok(next)
    }
}

/// The config as JSON with the credential replaced by a marker.
///
/// `config show` exists to be pasted into an issue, so the one thing it must
/// never do is print the secret (Principle #1). Whether a credential exists is
/// the part that is actually diagnostic, and that survives.
///
/// The substitution happens on the struct rather than on a parsed
/// `serde_json::Value`, so the output keeps the field order of the file itself.
/// A `Value` is a map whose iteration order depends on whether *anything* in the
/// build graph turned on serde_json's `preserve_order`, which would make this
/// output silently reorder itself on an unrelated dependency change.
pub fn redacted_json(cfg: &WorkerConfig) -> Result<String> {
    let masked = WorkerConfig {
        credential: cfg.credential.as_ref().map(|_| REDACTED.to_string()),
        ..cfg.clone()
    };
    serde_json::to_string_pretty(&masked).context("rendering worker config")
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
mod tests {
    use super::*;

    fn base() -> WorkerConfig {
        WorkerConfig {
            server: "http://h:8088".to_string(),
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
    fn every_field_reads_back_what_was_set() {
        // Walks the enum so a field added to `Field` without a `read` arm, or
        // without an `Edits` flag, shows up here rather than in a bug report.
        use clap::ValueEnum;
        let edits = Edits {
            server: Some("http://other:9000".to_string()),
            node: Some("node-b".to_string()),
            cpu: Some(12),
            memory: Some(Memory::from_bytes(16 * 1024 * 1024 * 1024)),
            reconcile_secs: Some(7),
            heartbeat_secs: Some(14),
            lease_secs: Some(60),
        };
        let updated = edits.clone().apply(&base()).unwrap();
        let expected = [
            (Field::Server, "http://other:9000"),
            (Field::Node, "node-b"),
            (Field::Cpu, "12"),
            (Field::Memory, "16G"),
            (Field::ReconcileSecs, "7"),
            (Field::HeartbeatSecs, "14"),
            (Field::LeaseSecs, "60"),
        ];
        assert_eq!(
            Field::value_variants().len(),
            expected.len(),
            "a Field variant is not covered here"
        );
        for (field, want) in expected {
            assert_eq!(field.read(&updated), want, "{field:?}");
        }
        assert_eq!(edits.touched().len(), expected.len());
    }

    #[test]
    fn an_empty_edit_is_recognised_as_empty() {
        assert!(Edits::default().is_empty());
        assert!(Edits::default().touched().is_empty());
        assert!(
            !Edits {
                cpu: Some(2),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn setting_one_field_leaves_the_rest_alone() {
        let cfg = base();
        let updated = Edits {
            cpu: Some(2),
            ..Default::default()
        }
        .apply(&cfg)
        .unwrap();
        assert_eq!(updated.cpu, 2);
        assert_eq!(updated.server, cfg.server);
        assert_eq!(updated.node, cfg.node);
        assert_eq!(updated.memory, cfg.memory);
        assert_eq!(updated.lease_secs, cfg.lease_secs);
    }

    #[test]
    fn renaming_a_joined_worker_is_refused() {
        let joined = base().with_credential("node-a.secret".to_string());
        let err = Edits {
            node: Some("node-b".to_string()),
            ..Default::default()
        }
        .apply(&joined)
        .unwrap_err()
        .to_string();
        assert!(err.contains("node-a"), "{err}");
        assert!(err.contains("veloslet setup"), "{err}");
    }

    #[test]
    fn renaming_an_unjoined_worker_is_allowed() {
        let updated = Edits {
            node: Some("node-b".to_string()),
            ..Default::default()
        }
        .apply(&base())
        .unwrap();
        assert_eq!(updated.node, "node-b");
    }

    #[test]
    fn setting_the_same_name_on_a_joined_worker_is_not_a_rename() {
        let joined = base().with_credential("node-a.secret".to_string());
        let updated = Edits {
            node: Some("node-a".to_string()),
            cpu: Some(6),
            ..Default::default()
        }
        .apply(&joined)
        .unwrap();
        assert_eq!(updated.cpu, 6);
        assert_eq!(updated.credential, joined.credential);
    }

    #[test]
    fn show_never_prints_the_credential() {
        let joined = base().with_credential("node-a.super-secret".to_string());
        let shown = redacted_json(&joined).unwrap();
        assert!(!shown.contains("super-secret"), "{shown}");
        assert!(shown.contains(REDACTED), "{shown}");
        // An unjoined worker has no credential key at all, and must not grow a
        // misleading "<set>" marker.
        let shown = redacted_json(&base()).unwrap();
        assert!(!shown.contains("credential"), "{shown}");
    }

    #[test]
    fn show_keeps_the_field_order_of_the_file() {
        // Not alphabetical: `show` must read like the file on disk. Going via a
        // serde_json::Value would sort (or not) depending on whether anything in
        // the build graph enabled `preserve_order`.
        let shown = redacted_json(&base().with_credential("s".to_string())).unwrap();
        let order: Vec<&str> = shown
            .lines()
            .filter_map(|l| l.trim().split('"').nth(1))
            .collect();
        assert_eq!(
            order,
            vec![
                "server",
                "node",
                "credential",
                "cpu",
                "memory",
                "reconcile_secs",
                "heartbeat_secs",
                "lease_secs"
            ]
        );
    }

    #[test]
    fn save_writes_0600_and_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("veloslet-cfg-{}", std::process::id()));
        let path = dir.join("veloslet.json");
        let cfg = base().with_credential("node-a.secret".to_string());
        save(&path, &cfg).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config must not be world-readable");
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(load(&path).unwrap(), cfg);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_a_missing_config_says_how_to_create_one() {
        let path = std::env::temp_dir().join("veloslet-does-not-exist-42.json");
        let _ = fs::remove_file(&path);
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("veloslet setup"), "{err}");
    }
}
