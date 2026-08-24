//! `veloslet` — the Velos worker daemon (the kubelet analog).
//!
//! It registers with the server, watches the containers assigned to it,
//! reconciles desired vs. observed via the pure [`reconcile`] core, and actuates
//! through the [`velos_runtime::ContainerRuntime`] seam. It renews a `Lease` as a
//! liveness heartbeat. The worker is authoritative for container `status`.

pub mod client;
pub mod config;
pub mod daemon;
pub mod host;
pub mod memory;
pub mod proxy;
pub mod reconcile;
pub mod status;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use velos_runtime::ContainerRuntime;

pub use client::{ApiClient, ClientError};
pub use proxy::{
    LocalContainer, NodeProxy, ProxyBinding, ServicePortView, ServiceView, plan_bindings,
};
pub use reconcile::{
    Action, DesiredContainer, DesiredState, ObservedInstance, RestartPolicy, reconcile,
};

/// The finalizer this worker owns; its presence means "veloslet must clean up
/// the micro-VM before the server may remove the object".
pub const FINALIZER: &str = "veloslet";

#[derive(Debug, thiserror::Error)]
pub enum VelosletError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("runtime error: {0}")]
    Runtime(#[from] velos_runtime::RuntimeError),
}

// ---------------------------------------------------------------------------
// Observation: turn server container documents into the pure-core inputs.
// ---------------------------------------------------------------------------

fn str_at<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = doc;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str()
}

fn desired_from_doc(doc: &Value) -> Option<DesiredContainer> {
    let name = str_at(doc, &["metadata", "name"])?.to_string();
    let uid = str_at(doc, &["metadata", "uid"])?.to_string();
    let image = str_at(doc, &["spec", "image"]).unwrap_or("").to_string();
    let command = doc
        .pointer("/spec/command")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let env = doc
        .pointer("/spec/env")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let restart_policy =
        RestartPolicy::parse(str_at(doc, &["spec", "restartPolicy"]).unwrap_or("Never"));
    let desired_state =
        DesiredState::parse(str_at(doc, &["spec", "desiredState"]).unwrap_or("Running"));
    let phase = str_at(doc, &["status", "phase"])
        .unwrap_or("Pending")
        .to_string();
    let marked_for_deletion = doc
        .pointer("/metadata/deletionTimestamp")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    let has_finalizer = doc
        .pointer("/metadata/finalizers")
        .and_then(Value::as_array)
        .map(|a| a.iter().any(|v| v.as_str() == Some(FINALIZER)))
        .unwrap_or(false);

    Some(DesiredContainer {
        name,
        uid,
        image,
        command,
        env,
        restart_policy,
        desired_state,
        phase,
        marked_for_deletion,
        has_finalizer,
    })
}

// ---------------------------------------------------------------------------
// Actuation
// ---------------------------------------------------------------------------

fn running_status(node: &str, instance_id: &str) -> Value {
    serde_json::json!({
        "phase": "Running",
        "workerName": node,
        "containerID": instance_id,
        "startedAt": chrono::Utc::now().to_rfc3339(),
    })
}

/// The status of a container that is asleep. `containerID` is deliberately
/// dropped: the instance exists but nothing is running in it, and the status
/// subresource replaces the whole object, so anything not restated is gone.
fn hibernated_status(node: &str) -> Value {
    serde_json::json!({
        "phase": "Hibernated",
        "workerName": node,
        "hibernatedAt": chrono::Utc::now().to_rfc3339(),
    })
}

/// A container's current status with a failed action recorded on it.
///
/// The phase is deliberately left alone. A launch that failed does not make the
/// container something other than `Scheduled`: it is still bound to this worker
/// and still not running, and the worker will try again on the next tick. Only
/// the worker's own retries can change that, and it has no way to tell an image
/// that will never pull from a registry having a bad minute — so declaring the
/// container `Failed` here would be a verdict it cannot justify. What was
/// missing was never a different phase, it was the reason.
///
/// The status subresource replaces the whole object, so `current` is restated
/// verbatim; anything dropped here would be erased.
fn failed_status(current: &Value, node: &str, reason: &str, message: &str) -> Value {
    let mut status = current.as_object().cloned().unwrap_or_default();
    status.insert("workerName".to_string(), serde_json::json!(node));
    status.insert("reason".to_string(), serde_json::json!(reason));
    status.insert("message".to_string(), serde_json::json!(message));
    Value::Object(status)
}

/// Whether `current` does not already record exactly this failure.
///
/// The worker retries a failing action on every reconcile tick, and every status
/// write is a store write plus a watch event. Re-reporting an unchanged failure
/// would append an event every few seconds for as long as the container stays
/// broken — which, for the failure this exists to surface, is forever.
fn failure_is_new(current: &Value, reason: &str, message: &str) -> bool {
    current.get("reason").and_then(Value::as_str) != Some(reason)
        || current.get("message").and_then(Value::as_str) != Some(message)
}

fn terminal_status(node: &str, phase: &str, exit_code: i32) -> Value {
    serde_json::json!({
        "phase": phase,
        "workerName": node,
        "exitCode": exit_code,
        "finishedAt": chrono::Utc::now().to_rfc3339(),
    })
}

async fn clear_finalizer(client: &ApiClient, name: &str) -> Result<(), VelosletError> {
    let Some(mut doc) = client.get_container(name).await? else {
        return Ok(());
    };
    if let Some(meta) = doc.get_mut("metadata").and_then(Value::as_object_mut) {
        let remaining: Vec<Value> = meta
            .get("finalizers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|v| v.as_str() != Some(FINALIZER))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        meta.insert("finalizers".to_string(), Value::Array(remaining));
    }
    client.replace_container(name, &doc).await?;
    Ok(())
}

/// Apply one decided action against the runtime and server.
pub async fn apply_action(
    client: &ApiClient,
    runtime: &dyn ContainerRuntime,
    node: &str,
    action: Action,
) -> Result<(), VelosletError> {
    match action {
        Action::Start { name, spec } => {
            let id = runtime.run(&spec).await?;
            client
                .put_status(&name, running_status(node, &id.0))
                .await?;
        }
        Action::Restart { name, uid, spec } => {
            runtime.remove(&uid).await?;
            let id = runtime.run(&spec).await?;
            client
                .put_status(&name, running_status(node, &id.0))
                .await?;
        }
        Action::ReportRunning { name } => {
            client.put_status(&name, running_status(node, "")).await?;
        }
        Action::Hibernate { name, uid } => {
            // `stop`, never `remove`: the instance and its disk must survive so
            // `Resume` can boot the same micro-VM back up.
            runtime.stop(&uid).await?;
            client.put_status(&name, hibernated_status(node)).await?;
        }
        Action::Resume { name, uid } => {
            let id = runtime.start(&uid).await?;
            client
                .put_status(&name, running_status(node, &id.0))
                .await?;
        }
        Action::ReportHibernated { name } => {
            client.put_status(&name, hibernated_status(node)).await?;
        }
        Action::ReportTerminal {
            name,
            phase,
            exit_code,
        } => {
            client
                .put_status(&name, terminal_status(node, &phase, exit_code))
                .await?;
        }
        Action::Cleanup {
            name,
            uid,
            clear_finalizer: clear,
        } => {
            runtime.stop(&uid).await?;
            runtime.remove(&uid).await?;
            if clear {
                clear_finalizer(client, &name).await?;
            }
        }
        Action::ClearFinalizer { name } => {
            clear_finalizer(client, &name).await?;
        }
        Action::Reap { uid } => {
            runtime.stop(&uid).await?;
            runtime.remove(&uid).await?;
        }
    }
    Ok(())
}

/// One reconcile pass: observe assigned containers + runtime, decide, actuate.
/// Returns the number of actions applied.
pub async fn run_once(
    client: &ApiClient,
    runtime: &dyn ContainerRuntime,
    node: &str,
) -> Result<usize, VelosletError> {
    let assigned = client.list_assigned(node).await?;
    let desired: Vec<DesiredContainer> = assigned.iter().filter_map(desired_from_doc).collect();

    let observed: Vec<ObservedInstance> = runtime
        .list()
        .await?
        .into_iter()
        .map(|i| ObservedInstance {
            uid: i.uid,
            state: i.state,
        })
        .collect();

    let actions = reconcile(&desired, &observed);
    let mut applied = 0;
    for action in actions {
        // Read what the action targets before it is consumed, so a failure can
        // be attributed to a container rather than to the pass as a whole.
        let target = action.container().map(str::to_string);
        let reason = action.failure_reason();
        match apply_action(client, runtime, node, action).await {
            Ok(()) => applied += 1,
            // Isolated on purpose: one container that cannot start must not cost
            // every other container on this worker its reconcile pass.
            Err(e) => report_failure(client, &assigned, node, target.as_deref(), reason, &e).await,
        }
    }
    Ok(applied)
}

/// Log a failed action against the container it was for and, when the failure is
/// that container's to carry, publish it as `status.reason` + `status.message`.
///
/// Without this a failing action was visible only in this worker's log, and only
/// as `reconcile failed: <error>` — no container name, and nothing at all on the
/// control plane, so the container simply sat in `Scheduled` with no way to ask
/// why from the API or the dashboard.
async fn report_failure(
    client: &ApiClient,
    assigned: &[Value],
    node: &str,
    name: Option<&str>,
    reason: Option<&'static str>,
    err: &VelosletError,
) {
    let message = err.to_string();
    let Some(name) = name else {
        tracing::warn!("reaping an orphaned instance failed: {message}");
        return;
    };
    tracing::warn!("container {name}: {message}");
    let Some(reason) = reason else {
        return;
    };
    let current = assigned
        .iter()
        .find(|doc| str_at(doc, &["metadata", "name"]) == Some(name))
        .and_then(|doc| doc.get("status"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !failure_is_new(&current, reason, &message) {
        return;
    }
    if let Err(e) = client
        .put_status(name, failed_status(&current, node, reason, &message))
        .await
    {
        tracing::warn!("container {name}: reporting the failure above failed too: {e}");
    }
}

// ---------------------------------------------------------------------------
// Service proxying
// ---------------------------------------------------------------------------

/// Read a port number that has to fit a TCP port. Anything outside the range is
/// `None` rather than truncated — a wrapped port would bind or forward to the
/// wrong place, which is worse than not serving the port at all.
fn port_at(doc: &Value, key: &str) -> Option<u16> {
    u16::try_from(doc.get(key)?.as_u64()?).ok()
}

fn service_from_doc(doc: &Value) -> Option<ServiceView> {
    let name = str_at(doc, &["metadata", "name"])?.to_string();
    let selector = doc
        .pointer("/spec/selector")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let ports = doc
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ports| {
            ports
                .iter()
                .filter_map(|p| {
                    Some(ServicePortView {
                        node_port: port_at(p, "nodePort")?,
                        target_port: port_at(p, "targetPort")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ServiceView {
        name,
        selector,
        ports,
    })
}

/// Join the containers the server bound here with what the runtime reports, so
/// a Service selector (which speaks labels) can reach an instance address
/// (which only the runtime knows). The join key is the uid, as everywhere else.
fn local_containers(
    assigned: &[Value],
    instances: &[velos_runtime::Instance],
) -> Vec<LocalContainer> {
    assigned
        .iter()
        .filter_map(|doc| {
            let uid = str_at(doc, &["metadata", "uid"])?;
            let labels = doc
                .pointer("/metadata/labels")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let ip = instances
                .iter()
                .find(|i| i.uid == uid)
                .and_then(|i| i.ip.clone());
            Some(LocalContainer { labels, ip })
        })
        .collect()
}

/// One service-proxy pass: converge this worker's node-port listeners onto the
/// Services that select a container running here. Returns how many listeners
/// should be up.
///
/// Kept apart from [`run_once`] on purpose. Container reconciliation and
/// traffic forwarding fail independently — an unreachable API server must not
/// tear down listeners that are still carrying traffic, and a port that cannot
/// be bound must not stall the container lifecycle.
pub async fn sync_services(
    client: &ApiClient,
    runtime: &dyn ContainerRuntime,
    node: &str,
    proxy: &NodeProxy,
) -> Result<usize, VelosletError> {
    let services: Vec<ServiceView> = client
        .list_services()
        .await?
        .iter()
        .filter_map(service_from_doc)
        .collect();
    let assigned = client.list_assigned(node).await?;
    let instances = runtime.list().await?;
    let plan = plan_bindings(&services, &local_containers(&assigned, &instances));
    let n = plan.len();
    proxy.sync(plan).await;
    Ok(n)
}

/// Run the worker forever: heartbeat + reconcile on intervals.
pub async fn run_loop(
    client: ApiClient,
    runtime: Arc<dyn ContainerRuntime>,
    node: String,
    reconcile_interval: Duration,
    heartbeat_interval: Duration,
    lease_duration_secs: u32,
) {
    let mut reconcile_tick = tokio::time::interval(reconcile_interval);
    let mut heartbeat_tick = tokio::time::interval(heartbeat_interval);
    let proxy = NodeProxy::new();
    loop {
        tokio::select! {
            _ = reconcile_tick.tick() => {
                if let Err(e) = run_once(&client, runtime.as_ref(), &node).await {
                    tracing::warn!("reconcile failed: {e}");
                }
                // After reconciling, not before: a container that just started
                // has no address until the runtime reports one.
                if let Err(e) = sync_services(&client, runtime.as_ref(), &node, &proxy).await {
                    tracing::warn!("service proxy sync failed: {e}");
                }
            }
            _ = heartbeat_tick.tick() => {
                if let Err(e) = client.renew_lease(&node, lease_duration_secs).await {
                    tracing::warn!("lease renew failed: {e}");
                }
            }
        }
    }
}
