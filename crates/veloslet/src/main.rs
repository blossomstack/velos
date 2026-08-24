//! The `veloslet` command line.
//!
//! The worker's life has three verbs and one noun:
//!
//! - `setup` joins a control plane once and writes the config, credential included.
//! - `config` reads and edits that config afterwards.
//! - `run` runs the worker loop, in this terminal or (`-d`) as a background
//!   LaunchAgent; `stop` and `uninstall` take the background one away again.
//!
//! Joining is deliberately not something `run` can do. A credential is earned by
//! `setup` and nothing else, so there is exactly one way for a worker to acquire
//! an identity, and a join token never reaches the config file or the process
//! table (Principles #2 and #6).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use velos_runtime::{AppleContainer, ContainerRuntime};
use veloslet::config::{self, Edits, Field};
use veloslet::daemon::{self, BUNDLE_EXECUTABLE, BUNDLE_ID, Bearer, WorkerConfig};
use veloslet::host::{detect_address, detect_host, detect_system_info, validate_capacity};
use veloslet::{ApiClient, run_loop};

mod signing;

/// The Velos worker daemon.
#[derive(Parser, Debug)]
#[command(name = "veloslet", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Join a control plane and write the worker config, credential included.
    /// Run this once per machine, before `run`.
    Setup(SetupArgs),
    /// Read or edit the worker config.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run the worker loop — in this terminal, or with `-d` as a background
    /// macOS LaunchAgent.
    Run(RunArgs),
    /// Stop the background worker, leaving it installed so `run -d` starts it
    /// again.
    Stop(PathArgs),
    /// Remove the background worker for good: LaunchAgent, app bundle, and the
    /// config with its credential.
    Uninstall(PathArgs),
    /// Report this worker's health — config, credential, control plane,
    /// capacity, runtime and background agent. Exits non-zero if anything is
    /// broken.
    Status(PathArgs),
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Print the whole config as JSON, with the credential redacted.
    Show(PathArgs),
    /// Print one field's value.
    Get {
        /// Which field to read.
        #[arg(value_enum)]
        field: Field,
        #[command(flatten)]
        path: PathArgs,
    },
    /// Change one or more fields.
    Set {
        #[command(flatten)]
        edits: Edits,
        #[command(flatten)]
        path: PathArgs,
    },
    /// Print the path of the config file.
    Path(PathArgs),
}

/// The `--config` override, shared by every command that touches the file.
#[derive(clap::Args, Debug, Clone, Default)]
struct PathArgs {
    /// Path to the worker config (default: ~/.velos/veloslet.json).
    #[arg(long)]
    config: Option<PathBuf>,
}

impl PathArgs {
    fn resolve(&self) -> Result<PathBuf> {
        match &self.config {
            Some(p) => Ok(p.clone()),
            None => config::default_path(),
        }
    }
}

#[derive(clap::Args, Debug)]
struct SetupArgs {
    /// Join token (`id.secret`), e.g. from `velosctl token create`. Traded for a
    /// worker credential here and never written to disk.
    #[arg(long)]
    token: String,
    /// Settings for this worker. Each falls back to the existing config, so a
    /// machine that has joined before needs only a fresh `--token`; they are
    /// required only when there is no config yet.
    #[command(flatten)]
    fields: Edits,
    #[command(flatten)]
    path: PathArgs,
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Run in the background as a macOS LaunchAgent instead of in this terminal.
    #[arg(short = 'd', long = "daemon")]
    daemon: bool,
    #[command(flatten)]
    path: PathArgs,
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The on-disk locations the background worker owns, all under the user's home
/// directory. The config path comes from `config::default_path` (or `--config`)
/// so there is one answer to "where does the config live".
struct Paths {
    bundle_dir: PathBuf,
    bundle_bin: PathBuf,
    info_plist: PathBuf,
    config_file: PathBuf,
    /// Persistent self-signed signing identity (cert + key). Survives uninstall
    /// so the bundle's code-signature stays stable across reinstalls.
    codesign_dir: PathBuf,
    agent_plist: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
}

impl Paths {
    fn resolve(config_file: PathBuf) -> Result<Self> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        let bundle_dir = home.join("Applications/Velos.app");
        Ok(Self {
            bundle_bin: bundle_dir.join("Contents/MacOS").join(BUNDLE_EXECUTABLE),
            info_plist: bundle_dir.join("Contents/Info.plist"),
            bundle_dir,
            config_file,
            codesign_dir: home.join(".velos/codesign"),
            agent_plist: home
                .join("Library/LaunchAgents")
                .join(format!("{BUNDLE_ID}.plist")),
            stdout_log: home.join("Library/Logs/veloslet.out.log"),
            stderr_log: home.join("Library/Logs/veloslet.err.log"),
        })
    }
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", p.display()))
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

/// Join a control plane and persist the resulting config.
///
/// Nothing is written until the credential is in hand: a config file that exists
/// but holds no credential would be a half-joined state that `run` would have to
/// interpret, so failure here leaves the machine exactly as it was.
async fn setup(args: SetupArgs) -> Result<()> {
    let path = args.path.resolve()?;

    // Flags win, the existing config fills the rest. Re-joining an already
    // configured machine is then just `veloslet setup --token <new token>`.
    let existing = config::load_if_present(&path)?;
    let rejoin = existing.as_ref().map(|c| c.node.clone());
    let cfg = config::resolve_setup(args.fields, existing)?;

    // Reject impossible capacity before touching the network (Principle #6).
    let host = detect_host()?;
    validate_capacity(cfg.cpu, cfg.memory, host)?;

    let runtime = AppleContainer::new();
    let runtime_version = runtime
        .version()
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    let join = Bearer::Join(args.token);
    let client = ApiClient::new(&cfg.server, Some(join.expose().to_string()));
    let response = client
        .register(&registration(&cfg, &runtime_version))
        .await
        .with_context(|| format!("registering {} with {}", cfg.node, cfg.server))?;

    let credential = daemon::credential_from_response(&response)?;
    let joined = cfg.with_credential(credential);
    config::save(&path, &joined)?;

    match rejoin {
        Some(previous) if previous == joined.node => {
            println!("re-joined {} as {}", joined.server, joined.node)
        }
        Some(previous) => println!(
            "joined {} as {} (was {previous})",
            joined.server, joined.node
        ),
        None => println!("joined {} as {}", joined.server, joined.node),
    }
    println!("  config:  {}", path.display());
    println!("  advertising {} cpu, {} memory", joined.cpu, joined.memory);
    println!(
        "\nNext: `veloslet run` to run in this terminal, or `veloslet run -d` for the background worker."
    );
    Ok(())
}

/// The registration body a worker publishes about itself.
fn registration(cfg: &WorkerConfig, runtime_version: &str) -> serde_json::Value {
    let sys = detect_system_info();
    // The address the control plane hands to whatever fronts this worker's
    // services. Without it a container here is reachable from nowhere, so an
    // endpoint for it is never published.
    let addresses: Vec<String> = detect_address(&cfg.server).into_iter().collect();
    serde_json::json!({
        "name": cfg.node,
        "capacity": { "cpu": cfg.cpu, "memoryBytes": cfg.memory.bytes() },
        "addresses": addresses,
        "containerRuntimeVersion": runtime_version,
        "nodeInfo": {
            "agentVersion": sys.agent_version,
            "os": sys.os,
            "arch": sys.arch,
            "hostname": sys.hostname,
        },
    })
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path(path) => {
            println!("{}", path.resolve()?.display());
            Ok(())
        }
        ConfigCommand::Show(path) => {
            let cfg = config::load(&path.resolve()?)?;
            println!("{}", config::redacted_json(&cfg)?);
            Ok(())
        }
        ConfigCommand::Get { field, path } => {
            let cfg = config::load(&path.resolve()?)?;
            println!("{}", field.read(&cfg));
            Ok(())
        }
        ConfigCommand::Set { edits, path } => {
            if edits.is_empty() {
                bail!("nothing to set — pass at least one field, e.g. `--cpu 8` (see --help)");
            }
            let file = path.resolve()?;
            let cfg = config::load(&file)?;
            let changed = edits.touched().join(", ");
            let updated = edits.apply(&cfg)?;
            // Capacity is validated here as well as at startup, so an impossible
            // value is refused while the user is still looking at the terminal
            // rather than at the next restart.
            validate_capacity(updated.cpu, updated.memory, detect_host()?)?;
            config::save(&file, &updated)?;
            println!("set {changed} in {}", file.display());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

async fn run(path: PathBuf) -> Result<()> {
    let cfg = config::load(&path)?;

    // Fail closed: never advertise more than the machine physically has.
    let host = detect_host()?;
    validate_capacity(cfg.cpu, cfg.memory, host)?;

    // A worker only ever speaks as itself. `setup` is the one place a credential
    // is minted, so an unjoined config stops here with a message that says so.
    let bearer = cfg.bearer()?;

    let runtime = AppleContainer::new();
    let runtime_version = runtime
        .version()
        .await
        .unwrap_or_else(|_| "unknown".to_string());

    // Re-register on every start to refresh what this worker advertises, and
    // retry in-process rather than exiting. A long-lived process is what lets
    // macOS attribute (and the user approve) the Local Network privacy prompt —
    // one that exits on the first blocked connection tears the prompt's owner
    // down before it can be approved. It also rides out transient outages.
    let client = ApiClient::new(&cfg.server, Some(bearer.expose().to_string()));
    let request = registration(&cfg, &runtime_version);
    loop {
        match client.register(&request).await {
            Ok(_) => break,
            Err(e) => {
                tracing::warn!("register failed, retrying in 10s: {e}");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
    tracing::info!("registered worker {} with its credential", cfg.node);

    let runtime: Arc<dyn ContainerRuntime> = Arc::new(runtime);
    tracing::info!("veloslet {} reconciling against {}", cfg.node, cfg.server);
    run_loop(
        client,
        runtime,
        cfg.node,
        Duration::from_secs(cfg.reconcile_secs),
        Duration::from_secs(cfg.heartbeat_secs),
        cfg.lease_secs,
    )
    .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn status(config_path: PathBuf) -> Result<()> {
    let report = veloslet::status::diagnose(config_path).await;
    println!("veloslet status\n");
    print!("{report}");
    if report.has_failures() {
        // A non-zero exit lets a script gate on a healthy worker; the report
        // above already said what is wrong.
        std::process::exit(1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run -d / stop / uninstall (side effects)
// ---------------------------------------------------------------------------

fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))?;
    Ok(())
}

/// The background worker is a launchd LaunchAgent, so `run -d`, `stop` and
/// `uninstall` only mean anything on macOS. Say so once, here, rather than
/// letting the caller discover it as a missing `codesign` or `launchctl`
/// several side effects deep — `install.sh` installs `veloslet` on Linux too.
fn require_macos(command: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    bail!(
        "`veloslet {command}` manages a macOS launchd LaunchAgent and does nothing on this \
         platform — run the worker with `veloslet run` under your own service manager \
         (systemd, supervisord, …) instead"
    )
}

/// Run `launchctl` quietly — we report success/failure ourselves, so suppress
/// its own output (notably the harmless "Unload failed" when nothing is loaded).
fn launchctl(args: &[&str]) -> Result<bool> {
    let status = Process::new("launchctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running launchctl")?;
    Ok(status.success())
}

/// `run -d`: install the worker as a launchd LaunchAgent and start it.
///
/// This only ever *starts* an already-joined worker. The credential comes from
/// the config `setup` wrote, so a machine that has not joined is told to do that
/// first instead of ending up with a background process that cannot authenticate.
fn run_daemon(config_file: PathBuf) -> Result<()> {
    require_macos("run -d")?;
    let paths = Paths::resolve(config_file)?;
    let version = env!("CARGO_PKG_VERSION");

    let cfg = config::load(&paths.config_file)?;
    if !cfg.is_connected() {
        bail!(
            "the config at {} has no credential — run `veloslet setup` first",
            paths.config_file.display()
        );
    }
    validate_capacity(cfg.cpu, cfg.memory, detect_host()?)?;

    // 1. App bundle: copy this running binary into Velos.app and give it a
    //    bundle identity so Local Network privacy can attribute its traffic.
    let src_exe = std::env::current_exe().context("locating the running veloslet binary")?;
    if let Some(parent) = paths.bundle_bin.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(&src_exe, &paths.bundle_bin).with_context(|| {
        format!(
            "copying {} -> {}",
            src_exe.display(),
            paths.bundle_bin.display()
        )
    })?;
    std::fs::set_permissions(&paths.bundle_bin, std::fs::Permissions::from_mode(0o755))?;
    write_file(
        &paths.info_plist,
        &daemon::render_info_plist(version),
        0o644,
    )?;

    // 2. Code-sign the bundle with a *persistent* self-signed identity so macOS
    //    keeps the Local Network privacy grant across reinstalls. Ad-hoc signing
    //    would re-pin the grant to the cdhash and break it on every rebuild.
    let identity = signing::ensure_identity(&paths.codesign_dir)?;
    signing::sign_bundle(&paths.bundle_dir, BUNDLE_ID, identity)?;
    // Verify with the system codesign so a bad signature fails loudly.
    let bundle = path_str(&paths.bundle_dir)?;
    let verified = Process::new("codesign")
        .args(["--verify", "--strict", bundle])
        .status()
        .context("running codesign --verify")?;
    if !verified.success() {
        bail!("codesign verification failed for {bundle}");
    }

    // 3. LaunchAgent plist pointing at the bundled binary + config.
    let program_args = vec![
        path_str(&paths.bundle_bin)?.to_string(),
        "run".to_string(),
        "--config".to_string(),
        path_str(&paths.config_file)?.to_string(),
    ];
    let agent = daemon::render_launch_agent(
        &program_args,
        "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        path_str(&paths.stdout_log)?,
        path_str(&paths.stderr_log)?,
    );
    write_file(&paths.agent_plist, &agent, 0o644)?;

    // 4. (Re)load the agent.
    let agent_path = path_str(&paths.agent_plist)?;
    let _ = launchctl(&["unload", agent_path]);
    if !launchctl(&["load", "-w", agent_path])? {
        bail!("launchctl load failed for {agent_path}");
    }

    // Best-effort: surface the Local Network privacy pane in case the prompt is
    // missed. Approving the "{name} wants to access your local network" prompt
    // is the one manual step.
    let _ = Process::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")
        .status();

    println!("started background worker {BUNDLE_ID}");
    println!("  bundle:  {}", paths.bundle_dir.display());
    println!("  config:  {}", paths.config_file.display());
    println!("  agent:   {}", paths.agent_plist.display());
    println!("  logs:    {}", paths.stdout_log.display());
    let name = daemon::BUNDLE_DISPLAY_NAME;
    println!(
        "\nApprove the macOS \"{name} wants to access your local network\" prompt\n\
         when it appears (or enable {name} under System Settings → Privacy &\n\
         Security → Local Network) — until then the worker cannot reach the server."
    );
    Ok(())
}

/// `stop`: unload the LaunchAgent, leaving the bundle and config in place so
/// `run -d` can start it again without re-joining.
fn stop(config_file: PathBuf) -> Result<()> {
    require_macos("stop")?;
    let paths = Paths::resolve(config_file)?;
    let agent_path = path_str(&paths.agent_plist)?;
    let _ = launchctl(&["unload", agent_path]);
    remove_if_exists(&paths.agent_plist)?;
    println!("stopped background worker {BUNDLE_ID}");
    println!("  the app bundle and config are kept — `veloslet run -d` starts it again");
    Ok(())
}

/// `uninstall`: take the worker off this machine for good.
fn uninstall(config_file: PathBuf) -> Result<()> {
    require_macos("uninstall")?;
    let paths = Paths::resolve(config_file)?;
    let agent_path = path_str(&paths.agent_plist)?;
    let _ = launchctl(&["unload", agent_path]);
    remove_if_exists(&paths.agent_plist)?;
    remove_dir_if_exists(&paths.bundle_dir)?;
    remove_if_exists(&paths.config_file)?;
    println!("uninstalled {BUNDLE_ID}: LaunchAgent, app bundle, and config removed");
    println!(
        "  the worker credential is gone with the config — rejoining needs a new\n  \
         join token and `veloslet setup`"
    );
    println!(
        "  the Local Network privacy grant for {} remains in System Settings →\n  \
         Privacy & Security → Local Network; remove it there if desired.",
        daemon::BUNDLE_DISPLAY_NAME
    );
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    match Cli::parse().command {
        Command::Setup(args) => setup(args).await,
        Command::Config { command } => config_command(command),
        Command::Run(args) => {
            let path = args.path.resolve()?;
            if args.daemon {
                run_daemon(path)
            } else {
                run(path).await
            }
        }
        Command::Status(path) => status(path.resolve()?).await,
        Command::Stop(path) => stop(path.resolve()?),
        Command::Uninstall(path) => uninstall(path.resolve()?),
    }
}
