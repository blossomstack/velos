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
    rt.start("one").await.unwrap();
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
    assert!(!dir.0.join("state/fixture.active").exists());
    NativeProcess::new(dir.config("sleep")).unwrap();
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let route_pid = pid.clone();
    let route_ready = ready.clone();
    let app = axum::Router::new().route(
        "/ready",
        axum::routing::get(move || {
            let pid = route_pid.load(Ordering::SeqCst);
            let ready = route_ready.load(Ordering::SeqCst);
            async move { axum::Json(serde_json::json!({"pid":pid,"ready":ready})) }
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
    assert_eq!(runtime.health().await.unwrap()[0].ready, Some(true));
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
