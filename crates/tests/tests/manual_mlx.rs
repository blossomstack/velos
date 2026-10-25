//! Explicitly opted-in, externally provisioned real MLX validation. Never run in CI.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use velos_runtime::native::{NativeConfig, NativeProcess};
use velos_runtime::{ContainerRuntime, InstanceState};
use velos_store::{SqliteStore, Store};

async fn ready(runtime: &NativeProcess) {
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            if runtime.health().await.unwrap()[0].ready == Some(true) {
                return;
            }
            assert_eq!(
                runtime.list().await.unwrap()[0].state,
                InstanceState::Running
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
}
async fn synth(http: &reqwest::Client, base: &str, text: &str) -> Value {
    let response = http
        .post(format!("{base}/v1/synthesize"))
        .json(&json!({"language":"en","voice":"narrator","text":text,"seed":42}))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "inference HTTP {}",
        response.status()
    );
    let mut result: Value = response.json().await.unwrap();
    assert!(result["audio_base64"].as_str().unwrap().len() > 100);
    result.as_object_mut().unwrap().remove("audio_base64");
    assert_eq!(result["provider"]["provider"], "mlx-qwen3-1.7b");
    assert!(result["metadata"]["duration_s"].as_f64().unwrap() > 0.0);
    result
}

#[tokio::test]
#[ignore = "requires explicit operator approval, pinned Qwen POC, exclusive GPU and VELOS_MODEL_CONFIG"]
async fn real_qwen_api_lifecycle() {
    let config: NativeConfig = serde_json::from_slice(
        &std::fs::read(std::env::var("VELOS_MODEL_CONFIG").unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(config.profiles.len(), 1);
    let profile = config.profiles.get("model").unwrap();
    assert_eq!(profile.stop_timeout_seconds, 60);
    let model_base = format!(
        "http://127.0.0.1:{}",
        profile.readiness.as_ref().unwrap().port
    );
    let state = config.state_directory.clone();
    let runtime = NativeProcess::new(config).unwrap();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server_store = store.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, velos_server::app(server_store))
            .await
            .unwrap();
    });
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(180))
        .build()
        .unwrap();
    let worker = json!({"metadata":{"name":"native-model-test"},"spec":{},"status":{"allocatable":{"cpu":4,"memoryBytes":8589934592u64},"conditions":[{"conditionType":"Ready","status":true}]}});
    assert_eq!(
        http.post(format!("{base}/api/v1/workers"))
            .json(&worker)
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    let workload = json!({"metadata":{"name":"model"},"spec":{"nodeName":"native-model-test","workload":{"type":"NativeProcess","object":{"profile":"model"}},"resources":{"cpu":1,"memoryBytes":7340032000u64},"restartPolicy":"OnFailure"},"status":{"phase":"Pending"}});
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
    let start = Instant::now();
    veloslet::run_once(&client, &runtime, "native-model-test")
        .await
        .unwrap();
    assert_eq!(runtime.health().await.unwrap()[0].ready, Some(false));
    ready(&runtime).await;
    veloslet::run_once(&client, &runtime, "native-model-test")
        .await
        .unwrap();
    assert_eq!(
        client.get_container("model").await.unwrap().unwrap()["status"]["ready"],
        true
    );
    let startup_s = start.elapsed().as_secs_f64();
    let first_id = runtime.list().await.unwrap()[0].id.clone();
    let first_health: Value = http
        .get(format!("{model_base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let inference = synth(&http, &model_base, "Velos is ready.").await;
    println!(
        "{}",
        json!({"stage":"inference","startup_s":startup_s,"result":inference})
    );
    // The runtime retains its unreaped child handle. No persisted PID is adopted;
    // validate the live response against the launch UUID before injecting failure.
    assert!(
        first_id
            .0
            .ends_with(first_health["instance"].as_str().unwrap())
    );
    let pid = i32::try_from(first_health["pid"].as_u64().unwrap()).unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.list().await.unwrap()[0].state == InstanceState::Running {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.health().await.unwrap()[0].ready, Some(false));
    let restart = Instant::now();
    // OnFailure goes through the same worker actuator as ordinary workloads.
    veloslet::run_once(&client, &runtime, "native-model-test")
        .await
        .unwrap();
    ready(&runtime).await;
    let restarted_id = runtime.list().await.unwrap()[0].id.clone();
    assert_ne!(first_id, restarted_id);
    let restart_s = restart.elapsed().as_secs_f64();
    println!("{}", json!({"stage":"restarted","restart_s":restart_s}));
    let request_http = http.clone();
    let request_base = model_base.clone();
    let request =
        tokio::spawn(
            async move { synth(&request_http, &request_base, "The crow flew home.").await },
        );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let health: Value = http
                .get(format!("{model_base}/health"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if health["active"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let drain = Instant::now();
    assert!(
        http.post(format!("{base}/api/v1/containers/model/hibernate"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    veloslet::run_once(&client, &runtime, "native-model-test")
        .await
        .unwrap();
    let drained_result = request.await.unwrap();
    let drain_s = drain.elapsed().as_secs_f64();
    assert!(drain_s < 65.0);
    assert_eq!(
        runtime.list().await.unwrap()[0].state,
        InstanceState::Exited { exit_code: 0 }
    );
    assert_eq!(
        client.get_container("model").await.unwrap().unwrap()["status"]["phase"],
        "Hibernated"
    );
    assert!(!state.join("model.active").exists());
    println!(
        "{}",
        json!({"stage":"drained","drain_s":drain_s,"result":drained_result})
    );
    assert!(
        http.post(format!("{base}/api/v1/containers/model/resume"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let resumed = Instant::now();
    veloslet::run_once(&client, &runtime, "native-model-test")
        .await
        .unwrap();
    ready(&runtime).await;
    assert_ne!(runtime.list().await.unwrap()[0].id, restarted_id);
    let resume_s = resumed.elapsed().as_secs_f64();
    let stop = Instant::now();
    assert!(
        http.delete(format!("{base}/api/v1/containers/model"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    veloslet::run_once(&client, &runtime, "native-model-test")
        .await
        .unwrap();
    assert!(runtime.list().await.unwrap().is_empty());
    assert!(!state.join("model.active").exists());
    runtime.shutdown().await.unwrap();
    let result = json!({"startup_s":startup_s,"restart_s":restart_s,"drain_s":drain_s,"resume_s":resume_s,"delete_s":stop.elapsed().as_secs_f64(),"inference":inference,"drained_inference":drained_result});
    std::fs::write(
        std::env::var("VELOS_MODEL_RESULT").unwrap(),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    println!("{}", result);
    server.abort();
}
