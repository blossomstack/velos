//! Real native child processes, no model imports, GPU use or container daemon.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};
use velos_runtime::dispatch::MixedRuntime;
use velos_runtime::native::{NativeConfig, NativeProcess, NativeProfile};
use velos_runtime::{ContainerRuntime, FakeRuntime, InstanceState, RunSpec, Workload};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "velos-native-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn config(&self, mode: &str) -> NativeConfig {
        NativeConfig {
            state_directory: self.0.join("state"),
            profiles: HashMap::from([(
                "fixture".into(),
                NativeProfile {
                    readiness: None,
                    executable: std::env::current_exe().unwrap(),
                    args: vec![
                        "--ignored".into(),
                        "--exact".into(),
                        "native_fixture".into(),
                        "--nocapture".into(),
                    ],
                    working_directory: self.0.clone(),
                    env: HashMap::from([
                        ("VELOS_FIXTURE".into(), mode.into()),
                        ("LITERAL".into(), "$(touch injected); `whoami`".into()),
                    ]),
                    stop_timeout_seconds: 1,
                },
            )]),
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn spec(uid: &str) -> RunSpec {
    RunSpec {
        uid: uid.into(),
        workload: Workload::NativeProcess {
            profile: "fixture".into(),
        },
        command: vec![],
        env: vec![],
    }
}

// Invoked as a child by the profiles above, never as part of the parent suite.
#[test]
#[ignore = "fixture child; launched explicitly by native integration tests"]
fn native_fixture() {
    let mode = std::env::var("VELOS_FIXTURE").unwrap();
    println!("literal={}", std::env::var("LITERAL").unwrap());
    println!("cwd={}", std::env::current_dir().unwrap().display());
    println!("parent_path_present={}", std::env::var_os("PATH").is_some());
    eprintln!("fixture stderr");
    std::fs::write("instance", std::env::var("VELOS_INSTANCE_ID").unwrap()).unwrap();
    std::fs::write("ready", std::process::id().to_string()).unwrap();
    match mode.as_str() {
        "exit" => std::process::exit(7),
        "logs" => {
            println!("{}", "x".repeat(2 * 1024 * 1024));
            std::process::exit(0);
        }
        "tree" => {
            let child = std::process::Command::new("/bin/sleep")
                .arg("120")
                .spawn()
                .unwrap();
            std::fs::write("descendant", child.id().to_string()).unwrap();
            // Runtime owns/reaps the entire process group on stop.
            std::mem::forget(child);
        }
        "ignore-term" => {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            std::fs::write("ignoring", "yes").unwrap();
        }
        "pipe-holder" => {
            use std::os::unix::process::CommandExt;
            let child = std::process::Command::new("/bin/sleep")
                .arg("3")
                .process_group(0)
                .spawn()
                .unwrap();
            std::mem::forget(child);
            std::process::exit(0);
        }
        "sleep" => {}
        _ => panic!("unknown fixture mode"),
    }
    std::thread::sleep(Duration::from_secs(120));
}
async fn wait_file(path: PathBuf) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn wait_exit(rt: &dyn ContainerRuntime) -> i32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let InstanceState::Exited { exit_code } = rt.list().await.unwrap()[0].state {
                return exit_code;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn native_lifecycle_is_owned_idempotent_and_does_not_stop_unrelated_processes() {
    let dir = Directory::new();
    let rt = NativeProcess::new(dir.config("sleep")).unwrap();
    let mut unrelated = std::process::Command::new("/bin/sleep")
        .arg("5")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let id = rt.run(&spec("one")).await.unwrap();
    assert_eq!(id, rt.run(&spec("one")).await.unwrap());
    wait_file(dir.0.join("ready")).await;
    assert_eq!(rt.list().await.unwrap()[0].state, InstanceState::Running);
    assert!(rt.list().await.unwrap()[0].ip.is_none());
    assert!(rt.run(&spec("two")).await.is_err());
    rt.stop("unknown").await.unwrap();
    rt.stop("one").await.unwrap();
    rt.stop("one").await.unwrap();
    assert!(matches!(
        rt.list().await.unwrap()[0].state,
        InstanceState::Exited { .. }
    ));
    assert!(unrelated.try_wait().unwrap().is_none());
    let resumed_id = rt.start("one").await.unwrap();
    assert_ne!(
        id, resumed_id,
        "a fresh process has a fresh launch identity"
    );
    assert_eq!(rt.list().await.unwrap()[0].state, InstanceState::Running);
    rt.remove("one").await.unwrap();
    assert!(rt.list().await.unwrap().is_empty());
    assert!(rt.start("one").await.is_err());
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
}

#[tokio::test]
async fn native_exit_logs_environment_and_literal_arguments() {
    let dir = Directory::new();
    let rt = NativeProcess::new(dir.config("exit")).unwrap();
    rt.run(&spec("one")).await.unwrap();
    assert_eq!(wait_exit(&rt).await, 7);
    let stdout = std::fs::read_to_string(dir.0.join("state/one.stdout.log")).unwrap();
    assert!(stdout.contains("literal=$(touch injected); `whoami`"));
    assert!(stdout.contains("parent_path_present=false"));
    assert!(stdout.contains(&format!("cwd={}", dir.0.canonicalize().unwrap().display())));
    assert!(
        std::fs::read_to_string(dir.0.join("state/one.stderr.log"))
            .unwrap()
            .contains("fixture stderr")
    );
    assert!(!dir.0.join("injected").exists());
    assert!(!dir.0.join("state/fixture.active").exists());
}

#[tokio::test]
async fn log_budget_does_not_block_a_verbose_child() {
    let dir = Directory::new();
    let rt = NativeProcess::new(dir.config("logs")).unwrap();
    rt.run(&spec("one")).await.unwrap();
    assert_eq!(wait_exit(&rt).await, 0);
    assert_eq!(
        std::fs::metadata(dir.0.join("state/one.stdout.log"))
            .unwrap()
            .len(),
        1024 * 1024
    );
}

#[tokio::test]
async fn termination_timeout_escalates_and_drop_cleans_owned_children() {
    let dir = Directory::new();
    let rt = NativeProcess::new(dir.config("ignore-term")).unwrap();
    rt.run(&spec("one")).await.unwrap();
    wait_file(dir.0.join("ignoring")).await;
    rt.stop("one").await.unwrap();
    assert_eq!(wait_exit(&rt).await, 128 + libc::SIGKILL);
    rt.start("one").await.unwrap();
    drop(rt);
    assert!(
        dir.0.join("state/fixture.active").exists(),
        "unclean Drop retains the recovery record"
    );
    assert!(NativeProcess::new(dir.config("sleep")).is_err());
}

#[tokio::test]
async fn exclusive_owner_stale_record_validation_and_spawn_failure() {
    let dir = Directory::new();
    let rt = NativeProcess::new(dir.config("sleep")).unwrap();
    assert!(NativeProcess::new(dir.config("sleep")).is_err());
    assert!(rt.run(&spec("../../bad")).await.is_err());
    let mut unknown = spec("one");
    unknown.workload = Workload::NativeProcess {
        profile: "not-provisioned".into(),
    };
    assert!(rt.run(&unknown).await.is_err());
    let mut injected = spec("one");
    injected.command = vec!["/bin/sh".into()];
    assert!(rt.run(&injected).await.is_err());
    drop(rt);
    std::fs::write(
        dir.0.join("state/fixture.active"),
        "arbitrary-pid-is-never-read",
    )
    .unwrap();
    assert!(NativeProcess::new(dir.config("sleep")).is_err());
    std::fs::remove_file(dir.0.join("state/fixture.active")).unwrap();
    let mut config = dir.config("sleep");
    config.profiles.get_mut("fixture").unwrap().executable = dir.0.join("missing-python");
    let rt = NativeProcess::new(config).unwrap();
    assert!(rt.run(&spec("one")).await.is_err());
    assert!(!dir.0.join("state/fixture.active").exists());
    assert!(
        rt.run(&spec("one"))
            .await
            .unwrap_err()
            .to_string()
            .contains("backoff")
    );
}

#[tokio::test]
async fn mixed_runtime_dispatch_preserves_container_lifecycle() {
    let dir = Directory::new();
    let fake = Arc::new(FakeRuntime::new());
    let rt = MixedRuntime::new(
        fake.clone(),
        NativeProcess::new(dir.config("sleep")).unwrap(),
    );
    let container = RunSpec {
        uid: "container".into(),
        workload: Workload::Container {
            image: "alpine".into(),
        },
        command: vec![],
        env: vec![],
    };
    rt.run(&container).await.unwrap();
    rt.run(&spec("native")).await.unwrap();
    assert_eq!(rt.list().await.unwrap().len(), 2);
    rt.shutdown().await.unwrap();
    assert_eq!(fake.list().await.unwrap()[0].state, InstanceState::Running);
    rt.remove("native").await.unwrap();
    rt.remove("container").await.unwrap();
}

#[tokio::test]
async fn native_workload_api_scheduling_readiness_hibernate_resume_and_delete() {
    use velos_store::{SqliteStore, Store};
    let directory = Directory::new();
    let runtime = NativeProcess::new(directory.config("sleep")).unwrap();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server_store = store.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, velos_server::app(server_store))
            .await
            .unwrap();
    });
    let http = reqwest::Client::new();
    let worker = serde_json::json!({"metadata":{"name":"native-node"},"spec":{},"status":{"allocatable":{"cpu":4,"memoryBytes":8589934592u64},"conditions":[{"conditionType":"Ready","status":true}]}});
    assert_eq!(
        http.post(format!("{base}/api/v1/workers"))
            .json(&worker)
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let workload = serde_json::json!({"metadata":{"name":"model"},"spec":{"nodeName":"native-node","workload":{"type":"NativeProcess","object":{"profile":"fixture"}},"resources":{"cpu":1,"memoryBytes":1024},"restartPolicy":"Never"},"status":{"phase":"Pending"}});
    assert_eq!(
        http.post(format!("{base}/api/v1/containers"))
            .json(&workload)
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    velos_server::controllers::reconcile_scheduling(store.as_ref()).unwrap();
    let client = veloslet::ApiClient::new(&base, None);
    veloslet::run_once(&client, &runtime, "native-node")
        .await
        .unwrap();
    let doc = client.get_container("model").await.unwrap().unwrap();
    assert_eq!(doc["status"]["phase"], "Running");
    assert!(
        doc["status"]["ready"].is_null(),
        "liveness without a readiness contract remains unknown"
    );
    for (action, expected) in [("hibernate", "Hibernated"), ("resume", "Running")] {
        assert!(
            http.post(format!("{base}/api/v1/containers/model/{action}"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        veloslet::run_once(&client, &runtime, "native-node")
            .await
            .unwrap();
        assert_eq!(
            client.get_container("model").await.unwrap().unwrap()["status"]["phase"],
            expected
        );
    }
    assert!(
        http.delete(format!("{base}/api/v1/containers/model"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    veloslet::run_once(&client, &runtime, "native-node")
        .await
        .unwrap();
    assert!(runtime.list().await.unwrap().is_empty());
    server.abort();
}

#[tokio::test]
async fn readiness_requires_explicit_ready_and_the_owned_pid() {
    use std::sync::atomic::AtomicBool;
    let directory = Directory::new();
    let pid = Arc::new(AtomicU64::new(0));
    let ready = Arc::new(AtomicBool::new(false));
    let large = Arc::new(AtomicBool::new(false));
    let route_large = large.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let instance = Arc::new(std::sync::Mutex::new(String::new()));
    let route_instance = instance.clone();
    let route_pid = pid.clone();
    let route_ready = ready.clone();
    let app = axum::Router::new().route(
        "/ready",
        axum::routing::get(move || {
            let pid = route_pid.load(Ordering::SeqCst);
            let instance = route_instance.lock().unwrap().clone();
            let ready = route_ready.load(Ordering::SeqCst);
            let large = route_large.load(Ordering::SeqCst);
            async move {
                let mut body = serde_json::json!({"pid":pid,"instance":instance,"ready":ready});
                if large {
                    body["padding"] = serde_json::json!("x".repeat(9000));
                }
                axum::Json(body)
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut config = directory.config("sleep");
    config.profiles.get_mut("fixture").unwrap().readiness =
        Some(velos_runtime::native::ReadinessProbe {
            port,
            path: "/ready".into(),
        });
    let runtime = NativeProcess::new(config).unwrap();
    runtime.run(&spec("one")).await.unwrap();
    wait_file(directory.0.join("ready")).await;
    assert_eq!(runtime.health().await.unwrap()[0].ready, Some(false));
    ready.store(true, Ordering::SeqCst);
    assert_eq!(
        runtime.health().await.unwrap()[0].ready,
        Some(false),
        "unrelated listener cannot claim readiness"
    );
    pid.store(
        std::fs::read_to_string(directory.0.join("ready"))
            .unwrap()
            .parse()
            .unwrap(),
        Ordering::SeqCst,
    );
    assert_eq!(
        runtime.health().await.unwrap()[0].ready,
        Some(false),
        "matching PID with wrong launch ID is not ready"
    );
    *instance.lock().unwrap() = std::fs::read_to_string(directory.0.join("instance")).unwrap();
    assert_eq!(runtime.health().await.unwrap()[0].ready, Some(true));
    large.store(true, Ordering::SeqCst);
    assert_eq!(
        runtime.health().await.unwrap()[0].ready,
        Some(false),
        "oversized response must not qualify"
    );
    large.store(false, Ordering::SeqCst);
    ready.store(false, Ordering::SeqCst);
    assert_eq!(runtime.health().await.unwrap()[0].ready, Some(false));
    runtime.shutdown().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn stop_cleans_a_foreground_process_tree() {
    let dir = Directory::new();
    let rt = NativeProcess::new(dir.config("tree")).unwrap();
    rt.run(&spec("one")).await.unwrap();
    wait_file(dir.0.join("descendant")).await;
    rt.stop("one").await.unwrap();
    assert!(matches!(
        rt.list().await.unwrap()[0].state,
        InstanceState::Exited { .. }
    ));
    rt.remove("one").await.unwrap();
}

#[tokio::test]
async fn executable_arguments_are_literal_not_shell_code() {
    let dir = Directory::new();
    let mut config = dir.config("exit");
    let profile = config.profiles.get_mut("fixture").unwrap();
    profile.executable = "/bin/echo".into();
    profile.args = vec!["$(touch injected); `whoami`".into()];
    let rt = NativeProcess::new(config).unwrap();
    rt.run(&spec("one")).await.unwrap();
    assert_eq!(wait_exit(&rt).await, 0);
    assert_eq!(
        std::fs::read_to_string(dir.0.join("state/one.stdout.log")).unwrap(),
        "$(touch injected); `whoami`\n"
    );
    assert!(!dir.0.join("injected").exists());
}

#[tokio::test]
async fn inherited_output_pipe_is_bounded_and_cancelled_cleanup_stays_fail_closed() {
    let dir = Directory::new();
    let runtime = NativeProcess::new(dir.config("pipe-holder")).unwrap();
    runtime.run(&spec("one")).await.unwrap();
    wait_file(dir.0.join("ready")).await;
    // Allow the foreground fixture to exit while its short-lived, deliberately
    // detached sleep child still holds the inherited pipes. Never kill/adopt it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), runtime.list())
            .await
            .is_err()
    );
    let result = tokio::time::timeout(Duration::from_secs(2), runtime.list())
        .await
        .unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("ownership record retained")
    );
    assert!(runtime.start("one").await.is_err());
    assert!(runtime.remove("one").await.is_err());
    assert!(dir.0.join("state/fixture.active").exists());
    drop(runtime);
    assert!(NativeProcess::new(dir.config("sleep")).is_err());
}

#[tokio::test]
#[ignore = "crash fixture owner; launched explicitly by integration test"]
async fn crash_owner_fixture() {
    let root = PathBuf::from(std::env::var("OWNER_ROOT").unwrap());
    let directory = std::mem::ManuallyDrop::new(Directory(root));
    let mut config = directory.config("sleep");
    let profile = config.profiles.get_mut("fixture").unwrap();
    profile.executable = "/bin/sleep".into();
    profile.args = vec!["0.2".into()];
    let runtime = NativeProcess::new(config).unwrap();
    runtime.run(&spec("orphan")).await.unwrap();
    std::process::exit(0); // deliberately skip runtime Drop, like worker death
}

#[tokio::test]
async fn fresh_worker_refuses_actual_unclean_owner_record_even_after_child_exits() {
    let directory = Directory::new();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "crash_owner_fixture", "--nocapture"])
        .env_clear()
        .env("OWNER_ROOT", &directory.0)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(directory.0.join("state/fixture.active").exists());
    assert!(NativeProcess::new(directory.config("sleep")).is_err());
}

#[tokio::test]
async fn readiness_timeout_and_restart_in_flight_never_report_stale_success() {
    use tokio::sync::Notify;
    let directory = Directory::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let response_pid = Arc::new(AtomicU64::new(0));
    let response_instance = Arc::new(std::sync::Mutex::new(String::new()));
    let route_instance = response_instance.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let route_entered = entered.clone();
    let route_release = release.clone();
    let route_pid = response_pid.clone();
    let app = axum::Router::new().route(
        "/ready",
        axum::routing::get(move || {
            let entered = route_entered.clone();
            let release = route_release.clone();
            let pid = route_pid.load(Ordering::SeqCst);
            let instance = route_instance.lock().unwrap().clone();
            async move {
                entered.notify_one();
                release.notified().await;
                axum::Json(serde_json::json!({"pid":pid,"instance":instance,"ready":true}))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut config = directory.config("sleep");
    config.profiles.get_mut("fixture").unwrap().readiness =
        Some(velos_runtime::native::ReadinessProbe {
            port,
            path: "/ready".into(),
        });
    let runtime = Arc::new(NativeProcess::new(config).unwrap());
    runtime.run(&spec("one")).await.unwrap();
    wait_file(directory.0.join("ready")).await;
    response_pid.store(
        std::fs::read_to_string(directory.0.join("ready"))
            .unwrap()
            .parse()
            .unwrap(),
        Ordering::SeqCst,
    );
    *response_instance.lock().unwrap() =
        std::fs::read_to_string(directory.0.join("instance")).unwrap();
    // Server never replies: the entire request/body must be bounded.
    let health = tokio::time::timeout(Duration::from_secs(2), runtime.health())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(health[0].ready, Some(false));
    // Consume the notification from the timed-out probe before the next probe.
    entered.notified().await;
    let probing_runtime = runtime.clone();
    let probing = tokio::spawn(async move { probing_runtime.health().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    runtime.stop("one").await.unwrap();
    runtime.start("one").await.unwrap();
    release.notify_waiters();
    assert_eq!(
        probing.await.unwrap()[0].ready,
        Some(false),
        "old response cannot bless a new launch"
    );
    assert_eq!(
        runtime.list().await.unwrap()[0].state,
        InstanceState::Running
    );
    runtime.shutdown().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn concurrent_mixed_launches_cannot_claim_the_same_uid() {
    let dir = Directory::new();
    let runtime = MixedRuntime::new(
        Arc::new(FakeRuntime::new()),
        NativeProcess::new(dir.config("sleep")).unwrap(),
    );
    let native = spec("same");
    let container = RunSpec {
        uid: "same".into(),
        workload: Workload::Container {
            image: "fixture".into(),
        },
        command: vec![],
        env: vec![],
    };
    let (first, second) = tokio::join!(runtime.run(&native), runtime.run(&container));
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(runtime.list().await.unwrap().len(), 1);
    runtime.remove("same").await.unwrap();
}

#[test]
fn local_profile_validation_and_diagnostic_redaction() {
    let directory = Directory::new();
    let valid = directory.config("sleep");
    for path in [
        "http://remote/",
        "//remote/",
        "/\\remote",
        "/ready\r\nHost: remote",
        "/ready\0",
        "/ready#fragment",
    ] {
        let mut config = valid.clone();
        config.profiles.get_mut("fixture").unwrap().readiness =
            Some(velos_runtime::native::ReadinessProbe {
                port: 8766,
                path: path.into(),
            });
        assert!(config.validate().is_err(), "{path:?}");
    }
    let mut relative = valid.clone();
    relative.profiles.get_mut("fixture").unwrap().executable = "python".into();
    assert!(relative.validate().is_err());
    let mut invalid_env = valid.clone();
    invalid_env
        .profiles
        .get_mut("fixture")
        .unwrap()
        .env
        .insert("INVALID=KEY".into(), "value".into());
    assert!(invalid_env.validate().is_err());
    let mut config = valid;
    config
        .profiles
        .get_mut("fixture")
        .unwrap()
        .env
        .insert("EXAMPLE_SECRET".into(), "not-a-real-secret".into());
    let worker: veloslet::daemon::WorkerConfig = serde_json::from_value(serde_json::json!({
        "server":"http://127.0.0.1:1", "node":"fixture", "cpu":1, "memory":"1G", "runtime": {"type":"NativeProcess", "config":config}
    })).unwrap();
    let diagnostic = veloslet::config::redacted_json(&worker).unwrap();
    assert!(diagnostic.contains("EXAMPLE_SECRET"));
    assert!(!diagnostic.contains("not-a-real-secret"));
}

#[tokio::test]
async fn log_paths_and_state_directory_do_not_follow_symlinks() {
    use std::os::unix::fs::symlink;
    let directory = Directory::new();
    let config = directory.config("sleep");
    let runtime = NativeProcess::new(config.clone()).unwrap();
    let sentinel = directory.0.join("sentinel");
    std::fs::write(&sentinel, "unchanged").unwrap();
    symlink(&sentinel, directory.0.join("state/one.stdout.log")).unwrap();
    assert!(runtime.run(&spec("one")).await.is_err());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
    assert!(!directory.0.join("state/fixture.active").exists());
    drop(runtime);
    symlink(directory.0.join("state"), directory.0.join("alias")).unwrap();
    let mut alias = config;
    alias.state_directory = directory.0.join("alias");
    assert!(NativeProcess::new(alias).is_err());
}
