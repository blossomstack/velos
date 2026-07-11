//! End-to-end: real server (in-process, over TCP) + a `veloslet` driving a
//! `FakeRuntime`, exercising the container happy path Pending → Scheduled →
//! Running → Succeeded through the public REST API.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use velos_runtime::FakeRuntime;
use velos_server::{app, controllers};
use velos_store::{SqliteStore, Store};
use veloslet::{ApiClient, run_once};

/// Bind an ephemeral port, serve the server in the background, and return the
/// base URL plus the shared store (so the test can drive controllers directly).
async fn start() -> (String, Arc<dyn Store>) {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().unwrap());
    let router = app(Arc::clone(&store));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), store)
}

async fn post(http: &reqwest::Client, base: &str, plural: &str, body: serde_json::Value) {
    let resp = http
        .post(format!("{base}/api/v1/{plural}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
}

async fn get_container(http: &reqwest::Client, base: &str, name: &str) -> serde_json::Value {
    http.get(format!("{base}/api/v1/containers/{name}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn container_runs_through_full_lifecycle() {
    let (base, store) = start().await;
    let http = reqwest::Client::new();

    // A ready worker with capacity.
    post(
        &http,
        &base,
        "workers",
        serde_json::json!({
            "metadata": { "name": "w1" },
            "spec": { "unschedulable": false },
            "status": {
                "allocatable": { "cpu": 4, "memoryBytes": 8589934592u64 },
                "conditions": [{ "conditionType": "Ready", "status": true }]
            }
        }),
    )
    .await;

    // A pending container.
    post(
        &http,
        &base,
        "containers",
        serde_json::json!({
            "metadata": { "name": "c1" },
            "spec": { "image": "alpine", "resources": { "cpu": 1, "memoryBytes": 536870912u64 } },
            "status": { "phase": "Pending" }
        }),
    )
    .await;

    // Scheduler binds the container to the worker.
    let bound = controllers::reconcile_scheduling(store.as_ref()).unwrap();
    assert_eq!(bound, 1);
    let c = get_container(&http, &base, "c1").await;
    assert_eq!(c["spec"]["nodeName"], "w1");
    assert_eq!(c["status"]["phase"], "Scheduled");
    let uid = c["metadata"]["uid"].as_str().unwrap().to_string();

    // veloslet observes the assignment and launches the instance.
    let client = ApiClient::new(&base, None);
    let runtime = FakeRuntime::new();
    let acted = run_once(&client, &runtime, "w1").await.unwrap();
    assert_eq!(acted, 1);
    let c = get_container(&http, &base, "c1").await;
    assert_eq!(c["status"]["phase"], "Running");
    assert_eq!(c["status"]["workerName"], "w1");

    // A second pass is a no-op (already Running and reported).
    assert_eq!(run_once(&client, &runtime, "w1").await.unwrap(), 0);

    // The instance exits cleanly; veloslet reports the terminal phase.
    runtime.set_exited(&uid, 0).unwrap();
    let acted = run_once(&client, &runtime, "w1").await.unwrap();
    assert_eq!(acted, 1);
    let c = get_container(&http, &base, "c1").await;
    assert_eq!(c["status"]["phase"], "Succeeded");
    assert_eq!(c["status"]["exitCode"], 0);
}

/// Serve an auth-enabled server on an ephemeral port; return the base URL.
async fn start_auth() -> String {
    let store: Arc<dyn Store> = Arc::new(SqliteStore::in_memory().unwrap());
    let auth = Arc::new(velos_auth::StoreAuthenticator::new(Arc::clone(&store)));
    let router = velos_server::app_with_auth(store, auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn admin_auth_end_to_end() {
    let base = start_auth().await;
    let http = reqwest::Client::new();

    // Unauthenticated /api/v1 is rejected while uninitialized.
    let r = http
        .get(format!("{base}/api/v1/containers"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);

    // First-run setup, then login for a session token.
    http.post(format!("{base}/auth/v1/setup"))
        .json(&serde_json::json!({ "username": "admin", "password": "pw" }))
        .send()
        .await
        .unwrap();
    let session = http
        .post(format!("{base}/auth/v1/login"))
        .json(&serde_json::json!({ "username": "admin", "password": "pw" }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // Mint a CLI token and use it on /api/v1.
    let cli = http
        .post(format!("{base}/auth/v1/admin/tokens"))
        .bearer_auth(&session)
        .json(&serde_json::json!({ "label": "ci" }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = http
        .get(format!("{base}/api/v1/containers"))
        .bearer_auth(&cli)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // Worker bootstrap still works: admin mints, worker registers.
    let boot = http
        .post(format!("{base}/auth/v1/tokens"))
        .bearer_auth(&session)
        .json(&serde_json::json!({ "ttlSeconds": 3600 }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let boot_tok = format!(
        "{}.{}",
        boot["tokenId"].as_str().unwrap(),
        boot["secret"].as_str().unwrap()
    );
    let reg = http
        .post(format!("{base}/auth/v1/register"))
        .bearer_auth(&boot_tok)
        .json(&serde_json::json!({ "name": "w1" }))
        .send()
        .await
        .unwrap();
    assert!(reg.status().is_success());
}

/// First-run setup, then login for a session token.
async fn setup_and_login(http: &reqwest::Client, base: &str) -> String {
    http.post(format!("{base}/auth/v1/setup"))
        .json(&serde_json::json!({ "username": "admin", "password": "pw" }))
        .send()
        .await
        .unwrap();
    http.post(format!("{base}/auth/v1/login"))
        .json(&serde_json::json!({ "username": "admin", "password": "pw" }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Admin-mint a labelled bootstrap token; returns the raw mint response JSON.
async fn mint_bootstrap(
    http: &reqwest::Client,
    base: &str,
    session: &str,
    label: &str,
) -> serde_json::Value {
    http.post(format!("{base}/auth/v1/tokens"))
        .bearer_auth(session)
        .json(&serde_json::json!({ "label": label, "ttlSeconds": 3600 }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()
}

fn joined(mint: &serde_json::Value) -> String {
    format!(
        "{}.{}",
        mint["tokenId"].as_str().unwrap(),
        mint["secret"].as_str().unwrap()
    )
}

#[tokio::test]
async fn worker_registration_round_trips_node_info() {
    let base = start_auth().await;
    let http = reqwest::Client::new();
    let session = setup_and_login(&http, &base).await;
    let boot_tok = joined(&mint_bootstrap(&http, &base, &session, "fleet").await);

    // Register a worker that reports full system info.
    http.post(format!("{base}/auth/v1/register"))
        .bearer_auth(&boot_tok)
        .json(&serde_json::json!({
            "name": "w1",
            "capacity": { "cpu": 4, "memoryBytes": 8589934592u64 },
            "addresses": [],
            "containerRuntimeVersion": "1.2.3",
            "nodeInfo": {
                "agentVersion": "0.9.9",
                "os": "macOS 15.1",
                "arch": "arm64",
                "hostname": "mac-01"
            }
        }))
        .send()
        .await
        .unwrap();

    let w = http
        .get(format!("{base}/api/v1/workers/w1"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(w["status"]["nodeInfo"]["agentVersion"], "0.9.9");
    assert_eq!(w["status"]["nodeInfo"]["os"], "macOS 15.1");
    assert_eq!(w["status"]["nodeInfo"]["arch"], "arm64");
    assert_eq!(w["status"]["nodeInfo"]["hostname"], "mac-01");

    // An older agent that omits nodeInfo still gets a stable "unknown" block.
    http.post(format!("{base}/auth/v1/register"))
        .bearer_auth(&boot_tok)
        .json(&serde_json::json!({ "name": "w2" }))
        .send()
        .await
        .unwrap();
    let w2 = http
        .get(format!("{base}/api/v1/workers/w2"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(w2["status"]["nodeInfo"]["agentVersion"], "unknown");
    assert_eq!(w2["status"]["nodeInfo"]["hostname"], "unknown");
}

#[tokio::test]
async fn deleting_worker_revokes_its_credential() {
    let base = start_auth().await;
    let http = reqwest::Client::new();
    let session = setup_and_login(&http, &base).await;
    let boot_tok = joined(&mint_bootstrap(&http, &base, &session, "fleet").await);

    let cred = http
        .post(format!("{base}/auth/v1/register"))
        .bearer_auth(&boot_tok)
        .json(&serde_json::json!({ "name": "w1" }))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // The freshly issued credential authenticates against the worker's own object.
    let r = http
        .get(format!("{base}/api/v1/workers/w1"))
        .bearer_auth(&cred)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // Admin deletes the worker.
    let r = http
        .delete(format!("{base}/api/v1/workers/w1"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // The per-worker credential is now revoked and fails closed.
    let r = http
        .get(format!("{base}/api/v1/workers/w1"))
        .bearer_auth(&cred)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bootstrap_tokens_list_and_revoke() {
    let base = start_auth().await;
    let http = reqwest::Client::new();
    let session = setup_and_login(&http, &base).await;

    // Listing is admin-gated.
    let r = http
        .get(format!("{base}/auth/v1/tokens"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED);

    let mint = mint_bootstrap(&http, &base, &session, "fleet-a").await;
    let id = mint["tokenId"].as_str().unwrap().to_string();
    let boot_tok = joined(&mint);

    // The token shows in the admin listing with its label and never its secret.
    let listed = http
        .get(format!("{base}/auth/v1/tokens"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let entry = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == serde_json::json!(id))
        .unwrap()
        .clone();
    assert_eq!(entry["label"], "fleet-a");
    assert!(entry.get("secret").is_none());
    assert!(entry.get("secretHash").is_none());

    // Revoke it: registration with it now fails closed and it leaves the list.
    let r = http
        .delete(format!("{base}/auth/v1/tokens/{id}"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    let reg = http
        .post(format!("{base}/auth/v1/register"))
        .bearer_auth(&boot_tok)
        .json(&serde_json::json!({ "name": "w1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(reg.status(), reqwest::StatusCode::UNAUTHORIZED);

    let listed = http
        .get(format!("{base}/auth/v1/tokens"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(
        listed["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["id"] != serde_json::json!(id))
    );
}
