//! In-process control loops hosted by the server.
//!
//! Each controller follows Principle #5: a **pure** decision function maps
//! observed state to intended actions, and a thin **actuator** applies those
//! actions to the `Store`. The decision functions are unit-tested in isolation;
//! the actuators are the only side-effecting code.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use velos_scheduler::{
    NodeSelectorOperator, NodeSelectorRequirement, NodeSelectorTerm, Placement, PlacementRequest,
    PreferredSchedulingTerm, ResourceRequest, Taint, TaintEffect, Toleration, TolerationOperator,
    WorkerName, WorkerView, schedule,
};
use velos_store::{Selector, Store, StoreError, StoredObject};

/// Default whole-core ask when a container omits `spec.resources.cpu`.
const DEFAULT_CPU: u32 = 1;
/// Default memory ask (512 MiB) when a container omits `spec.resources.memoryBytes`.
const DEFAULT_MEM: u64 = 512 * 1024 * 1024;
/// Label opting a container into rescheduling when its node dies.
const RESCHEDULABLE_LABEL: &str = "velos.io/reschedulable";

/// Tunables for the controller loops. Times mirror the design doc defaults.
#[derive(Debug, Clone)]
pub struct ControllerConfig {
    pub schedule_interval: Duration,
    pub lifecycle_interval: Duration,
    pub eviction_timeout: Duration,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            schedule_interval: Duration::from_secs(2),
            lifecycle_interval: Duration::from_secs(5),
            eviction_timeout: Duration::from_secs(300),
        }
    }
}

// ---------------------------------------------------------------------------
// JSON envelope readers (documents are opaque; read only what we interpret).
// ---------------------------------------------------------------------------

fn str_at<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = doc;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str()
}

fn u64_at(doc: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = doc;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_u64()
}

fn phase(doc: &Value) -> Option<&str> {
    str_at(doc, &["status", "phase"])
}

fn label(doc: &Value, key: &str) -> Option<String> {
    doc.get("metadata")?
        .get("labels")?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Scheduler controller
// ---------------------------------------------------------------------------

/// A container awaiting placement.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingContainer {
    pub name: String,
    pub request: PlacementRequest,
}

/// A decided placement of a container onto a worker.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub container: String,
    pub worker: String,
}

/// The outcome for one pending container in a scheduling pass.
#[derive(Debug, Clone, PartialEq)]
pub enum PlacementOutcome {
    Bind(Binding),
    Unschedulable { container: String, reason: String },
}

/// The bound worker recorded in a container's status (the actual placement).
fn worker_name(doc: &Value) -> Option<&str> {
    str_at(doc, &["status", "workerName"])
}

fn container_request(doc: &Value) -> ResourceRequest {
    ResourceRequest {
        cpu: u64_at(doc, &["spec", "resources", "cpu"])
            .map(|c| c as u32)
            .unwrap_or(DEFAULT_CPU),
        memory_bytes: u64_at(doc, &["spec", "resources", "memoryBytes"]).unwrap_or(DEFAULT_MEM),
    }
}

fn parse_node_selector(spec: &Value) -> Vec<(String, String)> {
    spec.get("nodeSelector")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_operator(s: &str) -> Option<NodeSelectorOperator> {
    match s {
        "In" => Some(NodeSelectorOperator::In),
        "NotIn" => Some(NodeSelectorOperator::NotIn),
        "Exists" => Some(NodeSelectorOperator::Exists),
        "DoesNotExist" => Some(NodeSelectorOperator::DoesNotExist),
        "Gt" => Some(NodeSelectorOperator::Gt),
        "Lt" => Some(NodeSelectorOperator::Lt),
        _ => None,
    }
}

fn parse_requirement(v: &Value) -> Option<NodeSelectorRequirement> {
    let key = v.get("key")?.as_str()?.to_string();
    let operator = parse_operator(v.get("operator")?.as_str()?)?;
    let values = v
        .get("values")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(NodeSelectorRequirement {
        key,
        operator,
        values,
    })
}

fn parse_term(v: &Value) -> NodeSelectorTerm {
    let match_expressions = v
        .get("matchExpressions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_requirement).collect())
        .unwrap_or_default();
    NodeSelectorTerm { match_expressions }
}

fn parse_affinity(spec: &Value) -> (Vec<NodeSelectorTerm>, Vec<PreferredSchedulingTerm>) {
    let Some(na) = spec.get("affinity").filter(|v| v.is_object()) else {
        return (Vec::new(), Vec::new());
    };
    let required = na
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_term).collect())
        .unwrap_or_default();
    let preferred = na
        .get("preferred")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let weight = p.get("weight")?.as_i64()? as i32;
                    Some(PreferredSchedulingTerm {
                        weight,
                        preference: parse_term(p.get("preference")?),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (required, preferred)
}

fn parse_taint_effect(s: &str) -> Option<TaintEffect> {
    match s {
        "NoSchedule" => Some(TaintEffect::NoSchedule),
        "PreferNoSchedule" => Some(TaintEffect::PreferNoSchedule),
        _ => None,
    }
}

fn parse_toleration(v: &Value) -> Option<Toleration> {
    let operator = match v.get("operator").and_then(Value::as_str) {
        Some("Exists") => TolerationOperator::Exists,
        Some("Equal") | None => TolerationOperator::Equal,
        Some(_) => return None,
    };
    let effect = v
        .get("effect")
        .and_then(Value::as_str)
        .and_then(parse_taint_effect);
    Some(Toleration {
        key: v
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        operator,
        value: v
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        effect,
    })
}

fn parse_tolerations(spec: &Value) -> Vec<Toleration> {
    spec.get("tolerations")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_toleration).collect())
        .unwrap_or_default()
}

fn parse_taints(spec: &Value) -> Vec<Taint> {
    spec.get("taints")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    let effect = parse_taint_effect(t.get("effect")?.as_str()?)?;
                    Some(Taint {
                        key: t.get("key")?.as_str()?.to_string(),
                        value: t
                            .get("value")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        effect,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build the scheduler request for a container from its document.
fn container_placement_request(doc: &Value) -> PlacementRequest {
    let null = Value::Null;
    let spec = doc.get("spec").unwrap_or(&null);
    let (required, preferred) = parse_affinity(spec);
    PlacementRequest {
        resources: container_request(doc),
        node_name: spec
            .get("nodeName")
            .and_then(Value::as_str)
            .map(|s| WorkerName(s.to_string())),
        node_selector: parse_node_selector(spec),
        required,
        preferred,
        tolerations: parse_tolerations(spec),
    }
}

/// Containers that are `Pending` and not yet bound to a worker. A container is
/// bound once `status.workerName` is set; a user-supplied `spec.nodeName` is a
/// *desired pin* (a request), not a binding, so it no longer excludes scheduling.
fn pending_containers(containers: &[StoredObject]) -> Vec<PendingContainer> {
    containers
        .iter()
        .filter(|c| phase(&c.document) == Some("Pending") && worker_name(&c.document).is_none())
        .map(|c| PendingContainer {
            name: c.name.clone(),
            request: container_placement_request(&c.document),
        })
        .collect()
}

/// Phases in which a container still holds its worker's resources.
///
/// `Hibernated` counts: the micro-VM is shut down but its disk stays on that
/// worker, and waking it must not fail because the slot was given away in the
/// meantime. Hibernating frees the *machine*, not the reservation.
fn holds_resources(doc: &Value) -> bool {
    matches!(
        phase(doc),
        Some("Scheduled") | Some("Running") | Some("Hibernated")
    )
}

/// Resources currently committed to a worker by containers bound to it.
fn usage_for(containers: &[StoredObject], worker: &str) -> ResourceRequest {
    let mut cpu = 0;
    let mut mem = 0;
    for c in containers {
        if c.node_name.as_deref() == Some(worker) && holds_resources(&c.document) {
            let r = container_request(&c.document);
            cpu += r.cpu;
            mem += r.memory_bytes;
        }
    }
    ResourceRequest {
        cpu,
        memory_bytes: mem,
    }
}

fn worker_ready(doc: &Value) -> bool {
    doc.get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(Value::as_array)
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("conditionType").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_bool) == Some(true)
            })
        })
        .unwrap_or(false)
}

fn build_worker_views(workers: &[StoredObject], containers: &[StoredObject]) -> Vec<WorkerView> {
    workers
        .iter()
        .map(|w| {
            let allocated = usage_for(containers, &w.name);
            WorkerView {
                name: WorkerName(w.name.clone()),
                ready: worker_ready(&w.document),
                unschedulable: w
                    .document
                    .get("spec")
                    .and_then(|s| s.get("unschedulable"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                allocatable: ResourceRequest {
                    cpu: u64_at(&w.document, &["status", "allocatable", "cpu"])
                        .map(|c| c as u32)
                        .unwrap_or(0),
                    memory_bytes: u64_at(&w.document, &["status", "allocatable", "memoryBytes"])
                        .unwrap_or(0),
                },
                allocated,
                labels: w.labels.clone(),
                taints: w.document.get("spec").map(parse_taints).unwrap_or_default(),
            }
        })
        .collect()
}

/// Pure: greedily place each pending container, accounting for prior placements
/// in the same pass so one worker is not overcommitted.
pub fn plan_bindings(
    pending: &[PendingContainer],
    workers: &[WorkerView],
) -> Vec<PlacementOutcome> {
    let mut views = workers.to_vec();
    let mut out = Vec::new();
    for p in pending {
        match schedule(&p.request, &views) {
            Placement::Scheduled(WorkerName(name)) => {
                if let Some(w) = views.iter_mut().find(|w| w.name.0 == name) {
                    w.allocated.cpu += p.request.resources.cpu;
                    w.allocated.memory_bytes += p.request.resources.memory_bytes;
                }
                out.push(PlacementOutcome::Bind(Binding {
                    container: p.name.clone(),
                    worker: name,
                }));
            }
            Placement::Unschedulable { reason } => {
                out.push(PlacementOutcome::Unschedulable {
                    container: p.name.clone(),
                    reason,
                });
            }
        }
    }
    out
}

/// Actuator: bind every schedulable pending container; returns the count bound.
pub fn reconcile_scheduling(store: &dyn Store) -> Result<usize, StoreError> {
    let containers = store.list("Container", &Selector::default())?;
    let workers = store.list("Worker", &Selector::default())?;
    let pending = pending_containers(&containers);
    let views = build_worker_views(&workers, &containers);
    let outcomes = plan_bindings(&pending, &views);

    let mut n = 0;
    for outcome in &outcomes {
        match outcome {
            PlacementOutcome::Bind(b) => {
                let Some(mut obj) = store.get("Container", &b.container)? else {
                    continue;
                };
                let rv = store.next_resource_version()?;
                // `spec.nodeName` is user-owned (the desired pin); the binding is
                // recorded only in status + the bound-worker index column.
                set_phase(&mut obj.document, "Scheduled");
                if let Some(status) = obj
                    .document
                    .get_mut("status")
                    .and_then(Value::as_object_mut)
                {
                    status.insert("workerName".to_string(), serde_json::json!(b.worker));
                }
                set_rv(&mut obj.document, rv);
                obj.resource_version = rv;
                obj.node_name = Some(b.worker.clone());
                store.put(&obj)?;
                n += 1;
            }
            PlacementOutcome::Unschedulable { container, reason } => {
                let Some(mut obj) = store.get("Container", container)? else {
                    continue;
                };
                // Record why it can't be placed — but only on change, to avoid
                // rewriting (and bumping the resource version) every tick.
                if str_at(&obj.document, &["status", "message"]) == Some(reason.as_str()) {
                    continue;
                }
                let rv = store.next_resource_version()?;
                if let Some(status) = obj
                    .document
                    .get_mut("status")
                    .and_then(Value::as_object_mut)
                {
                    status.insert("message".to_string(), serde_json::json!(reason));
                }
                set_rv(&mut obj.document, rv);
                obj.resource_version = rv;
                store.put(&obj)?;
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Node-lifecycle controller
// ---------------------------------------------------------------------------

/// A lease as seen by the lifecycle loop.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseView {
    pub worker: String,
    pub renew_time: DateTime<Utc>,
    pub duration: Duration,
}

/// Pure: names of workers whose lease has not been renewed within its duration.
pub fn expired_workers(leases: &[LeaseView], now: DateTime<Utc>) -> Vec<String> {
    leases
        .iter()
        .filter(|l| {
            let age = now.signed_duration_since(l.renew_time);
            age.to_std().map(|a| a > l.duration).unwrap_or(false)
        })
        .map(|l| l.worker.clone())
        .collect()
}

fn parse_lease(obj: &StoredObject) -> Option<LeaseView> {
    let holder = str_at(&obj.document, &["spec", "holderIdentity"])?;
    let renew = str_at(&obj.document, &["spec", "renewTime"])?;
    let renew_time = DateTime::parse_from_rfc3339(renew)
        .ok()?
        .with_timezone(&Utc);
    let secs = u64_at(&obj.document, &["spec", "leaseDurationSeconds"]).unwrap_or(40);
    Some(LeaseView {
        worker: holder.to_string(),
        renew_time,
        duration: Duration::from_secs(secs),
    })
}

fn ready_condition(now: DateTime<Utc>, ready: bool) -> Value {
    serde_json::json!({
        "conditionType": "Ready",
        "status": ready,
        "lastTransitionTime": now.to_rfc3339(),
        "reason": if ready { "LeaseRenewed" } else { "LeaseExpired" },
    })
}

fn ready_since(doc: &Value) -> Option<DateTime<Utc>> {
    let conds = doc.get("status")?.get("conditions")?.as_array()?;
    let cond = conds
        .iter()
        .find(|c| c.get("conditionType").and_then(Value::as_str) == Some("Ready"))?;
    let ts = cond.get("lastTransitionTime")?.as_str()?;
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn set_phase(doc: &mut Value, phase: &str) {
    if !doc.get("status").map(Value::is_object).unwrap_or(false) {
        doc["status"] = serde_json::json!({});
    }
    if let Some(s) = doc.get_mut("status").and_then(Value::as_object_mut) {
        s.insert("phase".to_string(), serde_json::json!(phase));
    }
}

fn set_rv(doc: &mut Value, rv: u64) {
    if let Some(m) = doc.get_mut("metadata").and_then(Value::as_object_mut) {
        m.insert("resourceVersion".to_string(), serde_json::json!(rv));
    }
}

/// Actuator: flip worker readiness from lease freshness, then evict containers
/// off workers that have been `NotReady` longer than the eviction timeout.
pub fn reconcile_node_lifecycle(
    store: &dyn Store,
    now: DateTime<Utc>,
    eviction_timeout: Duration,
) -> Result<(), StoreError> {
    let leases = store.list("Lease", &Selector::default())?;
    let lease_views: Vec<LeaseView> = leases.iter().filter_map(parse_lease).collect();
    let expired = expired_workers(&lease_views, now);
    let has_fresh_lease = |name: &str| {
        lease_views.iter().any(|l| l.worker == name) && !expired.iter().any(|e| e == name)
    };

    let workers = store.list("Worker", &Selector::default())?;
    for w in &workers {
        let target_ready = has_fresh_lease(&w.name);
        let current_ready = worker_ready(&w.document);
        if target_ready != current_ready {
            let rv = store.next_resource_version()?;
            let mut obj = w.clone();
            if let Some(status) = obj
                .document
                .get_mut("status")
                .and_then(Value::as_object_mut)
            {
                status.insert(
                    "conditions".to_string(),
                    serde_json::json!([ready_condition(now, target_ready)]),
                );
            } else {
                obj.document["status"] =
                    serde_json::json!({ "conditions": [ready_condition(now, target_ready)] });
            }
            set_rv(&mut obj.document, rv);
            obj.resource_version = rv;
            store.put(&obj)?;
        }
    }

    // Eviction: a worker NotReady past the grace window loses its containers.
    let containers = store.list("Container", &Selector::default())?;
    for w in &workers {
        if has_fresh_lease(&w.name) {
            continue;
        }
        let notready_since = ready_since(&w.document);
        let evict = match notready_since {
            Some(since) => now
                .signed_duration_since(since)
                .to_std()
                .map(|d| d > eviction_timeout)
                .unwrap_or(false),
            None => false,
        };
        if !evict {
            continue;
        }
        for c in &containers {
            if c.node_name.as_deref() != Some(w.name.as_str()) {
                continue;
            }
            // Hibernated containers are evicted too: their disk lived on the
            // dead worker, so staying bound to it would leave them unwakeable.
            if !holds_resources(&c.document) {
                continue;
            }
            let reschedulable = label(&c.document, RESCHEDULABLE_LABEL).as_deref() == Some("true");
            let rv = store.next_resource_version()?;
            let mut obj = c.clone();
            if reschedulable {
                // Clear the binding (status + index column) so the scheduler
                // re-places it; the user's `spec.nodeName` pin is preserved.
                if let Some(status) = obj
                    .document
                    .get_mut("status")
                    .and_then(Value::as_object_mut)
                {
                    status.remove("workerName");
                }
                obj.node_name = None;
                set_phase(&mut obj.document, "Pending");
            } else {
                set_phase(&mut obj.document, "Unknown");
            }
            set_rv(&mut obj.document, rv);
            obj.resource_version = rv;
            store.put(&obj)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Service endpoints
// ---------------------------------------------------------------------------

/// One address a Service is answering on right now.
///
/// This is the whole point of the object: an external load balancer needs a
/// concrete `address:nodePort` to send traffic to, and only the control plane
/// knows which worker a container landed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointView {
    pub worker_name: String,
    pub address: String,
    pub node_port: u64,
    pub container_name: String,
}

impl EndpointView {
    fn to_document(&self) -> Value {
        serde_json::json!({
            "workerName": self.worker_name,
            "address": self.address,
            "nodePort": self.node_port,
            "containerName": self.container_name,
        })
    }
}

/// Each worker's first advertised address, keyed by worker name.
///
/// A worker that advertises none is simply absent: it is running containers
/// nothing outside the machine can reach, and inventing an address for it would
/// publish an endpoint that black-holes traffic.
pub fn worker_addresses(workers: &[StoredObject]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for w in workers {
        let address = w
            .document
            .pointer("/status/addresses")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find_map(Value::as_str));
        if let Some(address) = address {
            out.insert(w.name.clone(), address.to_string());
        }
    }
    out
}

/// Whether a container's labels satisfy every entry of a Service selector.
/// An empty selector matches nothing here — admission rejects one, and treating
/// it as "match all" would turn a bad Service into a cluster-wide publication.
fn selected(selector: &serde_json::Map<String, Value>, labels: &HashMap<String, String>) -> bool {
    if selector.is_empty() {
        return false;
    }
    selector.iter().all(|(k, v)| {
        v.as_str()
            .is_some_and(|v| labels.get(k).map(String::as_str) == Some(v))
    })
}

/// Pure: where one Service is reachable, given every container and the workers'
/// addresses. Returns the endpoints plus the number of otherwise-good backends
/// dropped because their worker advertises no address (Principle #6: bounded
/// results are counted, never silently dropped).
pub fn endpoints_for(
    service: &Value,
    containers: &[StoredObject],
    addresses: &HashMap<String, String>,
) -> (Vec<EndpointView>, usize) {
    let Some(selector) = service.pointer("/spec/selector").and_then(Value::as_object) else {
        return (Vec::new(), 0);
    };
    let node_ports: Vec<u64> = service
        .pointer("/spec/ports")
        .and_then(Value::as_array)
        .map(|ports| {
            ports
                .iter()
                .filter_map(|p| p.get("nodePort").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut unaddressed = 0usize;
    for c in containers {
        // Only a running container answers. A `Pending` one has no worker, and a
        // `Hibernated` one has a worker but nothing listening behind it.
        if phase(&c.document) != Some("Running") || !selected(selector, &c.labels) {
            continue;
        }
        let Some(worker) = worker_name(&c.document) else {
            continue;
        };
        let Some(address) = addresses.get(worker) else {
            unaddressed += node_ports.len();
            continue;
        };
        for node_port in &node_ports {
            out.push(EndpointView {
                worker_name: worker.to_string(),
                address: address.clone(),
                node_port: *node_port,
                container_name: c.name.clone(),
            });
        }
    }
    // Stable order, so an unchanged world produces an unchanged document and the
    // actuator below does not churn the resource version on every pass.
    out.sort_by(|a, b| (&a.container_name, a.node_port).cmp(&(&b.container_name, b.node_port)));
    (out, unaddressed)
}

/// Actuator: recompute every Service's endpoints and write back the ones that
/// changed. Returns how many Services were updated.
pub fn reconcile_service_endpoints(store: &dyn Store) -> Result<usize, StoreError> {
    let services = store.list("Service", &Selector::default())?;
    if services.is_empty() {
        return Ok(0);
    }
    let containers = store.list("Container", &Selector::default())?;
    let addresses = worker_addresses(&store.list("Worker", &Selector::default())?);

    let mut updated = 0;
    for svc in services {
        let (endpoints, unaddressed) = endpoints_for(&svc.document, &containers, &addresses);
        if unaddressed > 0 {
            tracing::warn!(
                service = %svc.name,
                dropped = unaddressed,
                "service endpoints omitted: their worker advertises no address"
            );
        }
        let next: Vec<Value> = endpoints.iter().map(EndpointView::to_document).collect();
        let current = svc.document.pointer("/status/endpoints");
        if current == Some(&Value::Array(next.clone())) {
            continue;
        }
        let rv = store.next_resource_version()?;
        let mut obj = svc.clone();
        obj.document["status"] = serde_json::json!({ "endpoints": next });
        set_rv(&mut obj.document, rv);
        obj.resource_version = rv;
        store.put(&obj)?;
        updated += 1;
    }
    Ok(updated)
}

// ---------------------------------------------------------------------------
// Loop wiring
// ---------------------------------------------------------------------------

/// Spawn the controller reconcile loops as background tokio tasks.
pub fn spawn(store: Arc<dyn Store>, cfg: ControllerConfig) {
    let sched_store = Arc::clone(&store);
    let sched_interval = cfg.schedule_interval;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(sched_interval);
        loop {
            tick.tick().await;
            if let Err(e) = reconcile_scheduling(sched_store.as_ref()) {
                tracing::warn!("scheduler reconcile failed: {e}");
            }
        }
    });

    let ep_store = Arc::clone(&store);
    let ep_interval = cfg.schedule_interval;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(ep_interval);
        loop {
            tick.tick().await;
            if let Err(e) = reconcile_service_endpoints(ep_store.as_ref()) {
                tracing::warn!("service-endpoints reconcile failed: {e}");
            }
        }
    });

    let life_interval = cfg.lifecycle_interval;
    let eviction = cfg.eviction_timeout;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(life_interval);
        loop {
            tick.tick().await;
            if let Err(e) = reconcile_node_lifecycle(store.as_ref(), Utc::now(), eviction) {
                tracing::warn!("node-lifecycle reconcile failed: {e}");
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use velos_store::SqliteStore;

    const GB: u64 = 1024 * 1024 * 1024;

    fn view(name: &str, cpu: u32, mem: u64) -> WorkerView {
        WorkerView {
            name: WorkerName(name.to_string()),
            ready: true,
            unschedulable: false,
            allocatable: ResourceRequest {
                cpu,
                memory_bytes: mem,
            },
            allocated: ResourceRequest {
                cpu: 0,
                memory_bytes: 0,
            },
            labels: std::collections::HashMap::new(),
            taints: Vec::new(),
        }
    }

    fn req(cpu: u32, mem: u64) -> PlacementRequest {
        PlacementRequest {
            resources: ResourceRequest {
                cpu,
                memory_bytes: mem,
            },
            node_name: None,
            node_selector: Vec::new(),
            required: Vec::new(),
            preferred: Vec::new(),
            tolerations: Vec::new(),
        }
    }

    fn worker_obj(name: &str, labels: &[(&str, &str)], taints: Value) -> StoredObject {
        let lbls: std::collections::HashMap<String, String> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        StoredObject {
            kind: "Worker".to_string(),
            name: name.to_string(),
            uid: uuid::Uuid::new_v4(),
            resource_version: 0,
            node_name: None,
            labels: lbls.clone(),
            document: serde_json::json!({
                "metadata": { "name": name, "labels": lbls },
                "spec": { "unschedulable": false, "taints": taints },
                "status": {
                    "allocatable": { "cpu": 8, "memoryBytes": 16u64 * GB },
                    "conditions": [{ "conditionType": "Ready", "status": true }],
                }
            }),
        }
    }

    fn container_obj(name: &str, phase: &str, worker: Option<&str>) -> StoredObject {
        let mut status = serde_json::json!({ "phase": phase });
        if let Some(w) = worker {
            status["workerName"] = serde_json::json!(w);
        }
        StoredObject {
            kind: "Container".to_string(),
            name: name.to_string(),
            uid: uuid::Uuid::new_v4(),
            resource_version: 0,
            // The index column mirrors the bound worker (status.workerName).
            node_name: worker.map(str::to_string),
            labels: std::collections::HashMap::new(),
            document: serde_json::json!({
                "metadata": { "name": name },
                "spec": { "image": "img", "resources": { "cpu": 1, "memoryBytes": 1024 } },
                "status": status,
            }),
        }
    }

    fn labelled_container(
        name: &str,
        phase: &str,
        worker: Option<&str>,
        labels: &[(&str, &str)],
    ) -> StoredObject {
        let mut obj = container_obj(name, phase, worker);
        obj.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        obj
    }

    fn addressed_worker(name: &str, address: Option<&str>) -> StoredObject {
        let mut obj = worker_obj(name, &[], serde_json::json!([]));
        obj.document["status"]["addresses"] = match address {
            Some(a) => serde_json::json!([a]),
            None => serde_json::json!([]),
        };
        obj
    }

    fn service_obj(name: &str, selector: &[(&str, &str)], node_ports: &[u64]) -> StoredObject {
        let sel: serde_json::Map<String, Value> = selector
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(v)))
            .collect();
        let ports: Vec<Value> = node_ports
            .iter()
            .map(|p| serde_json::json!({ "targetPort": 8080, "nodePort": p }))
            .collect();
        StoredObject {
            kind: "Service".to_string(),
            name: name.to_string(),
            uid: uuid::Uuid::new_v4(),
            resource_version: 0,
            node_name: None,
            labels: std::collections::HashMap::new(),
            document: serde_json::json!({
                "metadata": { "name": name },
                "spec": { "selector": sel, "ports": ports },
                "status": { "endpoints": [] },
            }),
        }
    }

    #[test]
    fn endpoints_are_the_running_selected_containers_worker_addresses() {
        let svc = service_obj("web", &[("app", "web")], &[31000]);
        let containers = [
            labelled_container("web-1", "Running", Some("w1"), &[("app", "web")]),
            labelled_container("web-2", "Running", Some("w2"), &[("app", "web")]),
            labelled_container("db-1", "Running", Some("w1"), &[("app", "db")]),
        ];
        let addresses = worker_addresses(&[
            addressed_worker("w1", Some("192.168.68.51")),
            addressed_worker("w2", Some("192.168.68.52")),
        ]);
        let (got, dropped) = endpoints_for(&svc.document, &containers, &addresses);
        assert_eq!(dropped, 0);
        assert_eq!(
            got.iter().map(|e| e.address.as_str()).collect::<Vec<_>>(),
            vec!["192.168.68.51", "192.168.68.52"],
            "the unselected container must not appear"
        );
        assert!(got.iter().all(|e| e.node_port == 31000));
    }

    #[test]
    fn only_a_running_container_is_an_endpoint() {
        // A Pending container has no worker at all, and a Hibernated one has a
        // worker with nothing listening behind it. Publishing either sends real
        // traffic into a black hole.
        let svc = service_obj("web", &[("app", "web")], &[31000]);
        let addresses = worker_addresses(&[addressed_worker("w1", Some("192.168.68.51"))]);
        for phase in ["Pending", "Hibernated", "Succeeded", "Failed"] {
            let containers = [labelled_container(
                "web-1",
                phase,
                Some("w1"),
                &[("app", "web")],
            )];
            let (got, _) = endpoints_for(&svc.document, &containers, &addresses);
            assert!(got.is_empty(), "{phase} must not be published");
        }
    }

    #[test]
    fn a_worker_without_an_address_is_counted_not_guessed() {
        // No-silent-caps: the backend is real but unreachable, and the operator
        // needs to know that rather than see a short endpoint list.
        let svc = service_obj("web", &[("app", "web")], &[31000, 31001]);
        let containers = [labelled_container(
            "web-1",
            "Running",
            Some("w1"),
            &[("app", "web")],
        )];
        let addresses = worker_addresses(&[addressed_worker("w1", None)]);
        let (got, dropped) = endpoints_for(&svc.document, &containers, &addresses);
        assert!(got.is_empty());
        assert_eq!(dropped, 2, "one per port that could not be published");
    }

    #[test]
    fn endpoints_reconcile_writes_once_and_then_settles() {
        let store = SqliteStore::in_memory().unwrap();
        store
            .put(&service_obj("web", &[("app", "web")], &[31000]))
            .unwrap();
        store
            .put(&labelled_container(
                "web-1",
                "Running",
                Some("w1"),
                &[("app", "web")],
            ))
            .unwrap();
        store
            .put(&addressed_worker("w1", Some("192.168.68.51")))
            .unwrap();

        assert_eq!(reconcile_service_endpoints(&store).unwrap(), 1);
        let stored = store.get("Service", "web").unwrap().unwrap();
        assert_eq!(
            stored.document.pointer("/status/endpoints/0/address"),
            Some(&serde_json::json!("192.168.68.51"))
        );

        // An unchanged world must not bump the resource version: every watcher
        // wakes on a write, and this loop runs on a timer.
        assert_eq!(
            reconcile_service_endpoints(&store).unwrap(),
            0,
            "a settled Service was rewritten"
        );
    }

    #[test]
    fn schedules_container_with_user_set_node_name() {
        // A Pending container that already carries spec.nodeName must still be
        // scheduled (it is a request, not a binding) while status.workerName is absent.
        let mut c = container_obj("c1", "Pending", None);
        c.document["spec"]["nodeName"] = serde_json::json!("w1");
        let workers = [worker_obj("w1", &[], serde_json::json!([]))];
        let pending = pending_containers(std::slice::from_ref(&c));
        assert_eq!(
            pending.len(),
            1,
            "user-pinned Pending container must be schedulable"
        );
        let views = build_worker_views(&workers, &[]);
        let out = plan_bindings(&pending, &views);
        assert!(matches!(out.as_slice(),
            [PlacementOutcome::Bind(b)] if b.worker == "w1"));
    }

    #[test]
    fn hibernated_container_keeps_holding_its_workers_resources() {
        // Waking must be guaranteed a slot, so the reservation outlives the
        // running micro-VM — otherwise a resume could find the worker full.
        let c = container_obj("c1", "Hibernated", Some("w1"));
        let used = usage_for(std::slice::from_ref(&c), "w1");
        assert_eq!(used.cpu, 1);
        assert_eq!(used.memory_bytes, 1024);
    }

    #[test]
    fn already_bound_container_is_not_pending() {
        // status.workerName present -> not a scheduling candidate.
        let c = container_obj("c1", "Scheduled", Some("w1"));
        assert!(pending_containers(std::slice::from_ref(&c)).is_empty());
    }

    #[test]
    fn parses_placement_from_spec() {
        let doc = serde_json::json!({
            "spec": {
                "resources": { "cpu": 2, "memoryBytes": 1024 },
                "nodeName": "w2",
                "nodeSelector": { "gpu": "true" },
                "affinity": {
                    "required": [ { "matchExpressions": [
                        { "key": "zone", "operator": "In", "values": ["us"] } ] } ],
                    "preferred": [ { "weight": 10, "preference": { "matchExpressions": [
                        { "key": "fast", "operator": "Exists", "values": [] } ] } } ]
                },
                "tolerations": [ { "key": "spot", "operator": "Exists" } ]
            }
        });
        let p = container_placement_request(&doc);
        assert_eq!(p.node_name, Some(WorkerName("w2".into())));
        assert_eq!(
            p.node_selector,
            vec![("gpu".to_string(), "true".to_string())]
        );
        assert_eq!(p.required.len(), 1);
        assert_eq!(p.preferred.len(), 1);
        assert_eq!(p.preferred[0].weight, 10);
        assert_eq!(p.tolerations.len(), 1);
    }

    #[test]
    fn parses_worker_labels_and_taints() {
        let workers = [worker_obj(
            "w1",
            &[("gpu", "true")],
            serde_json::json!([{ "key": "spot", "value": "", "effect": "NoSchedule" }]),
        )];
        let views = build_worker_views(&workers, &[]);
        assert_eq!(views[0].labels.get("gpu").map(String::as_str), Some("true"));
        assert_eq!(views[0].taints.len(), 1);
        assert_eq!(views[0].taints[0].effect, TaintEffect::NoSchedule);
    }

    #[test]
    fn plan_bindings_packs_until_capacity_then_leaves_pending() {
        let pending = vec![
            PendingContainer {
                name: "a".into(),
                request: req(2, GB),
            },
            PendingContainer {
                name: "b".into(),
                request: req(2, GB),
            },
            PendingContainer {
                name: "c".into(),
                request: req(2, GB),
            },
        ];
        // One worker with 4 cores fits exactly two 2-core containers.
        let workers = vec![view("w1", 4, 8 * GB)];
        let outcomes = plan_bindings(&pending, &workers);
        let bound: Vec<&str> = outcomes
            .iter()
            .filter_map(|o| match o {
                PlacementOutcome::Bind(b) => Some(b.worker.as_str()),
                PlacementOutcome::Unschedulable { .. } => None,
            })
            .collect();
        assert_eq!(bound, vec!["w1", "w1"]);
    }

    #[test]
    fn expired_workers_detects_stale_leases() {
        let now = DateTime::parse_from_rfc3339("2026-06-27T00:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let leases = vec![
            LeaseView {
                worker: "fresh".into(),
                renew_time: DateTime::parse_from_rfc3339("2026-06-27T00:00:55Z")
                    .unwrap()
                    .with_timezone(&Utc),
                duration: Duration::from_secs(40),
            },
            LeaseView {
                worker: "stale".into(),
                renew_time: DateTime::parse_from_rfc3339("2026-06-27T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                duration: Duration::from_secs(40),
            },
        ];
        assert_eq!(expired_workers(&leases, now), vec!["stale".to_string()]);
    }

    fn put_doc(store: &SqliteStore, kind: &str, name: &str, node: Option<&str>, doc: Value) {
        let rv = store.next_resource_version().unwrap();
        let mut doc = doc;
        set_rv(&mut doc, rv);
        store
            .put(&StoredObject {
                kind: kind.to_string(),
                name: name.to_string(),
                uid: uuid::Uuid::new_v4(),
                resource_version: rv,
                node_name: node.map(str::to_string),
                labels: std::collections::HashMap::new(),
                document: doc,
            })
            .unwrap();
    }

    fn ready_worker_doc(name: &str) -> Value {
        serde_json::json!({
            "metadata": { "name": name },
            "spec": { "unschedulable": false },
            "status": {
                "allocatable": { "cpu": 8, "memoryBytes": 16u64 * GB },
                "conditions": [],
            }
        })
    }

    #[test]
    fn reconcile_scheduling_binds_pending_to_ready_worker() {
        let store = SqliteStore::in_memory().unwrap();
        let mut w = ready_worker_doc("w1");
        w["status"]["conditions"] =
            serde_json::json!([{ "conditionType": "Ready", "status": true }]);
        put_doc(&store, "Worker", "w1", None, w);
        put_doc(
            &store,
            "Container",
            "c1",
            None,
            serde_json::json!({
                "metadata": { "name": "c1" },
                "spec": { "image": "img", "resources": { "cpu": 2, "memoryBytes": GB } },
                "status": { "phase": "Pending" }
            }),
        );

        let bound = reconcile_scheduling(&store).unwrap();
        assert_eq!(bound, 1);
        let c = store.get("Container", "c1").unwrap().unwrap();
        assert_eq!(c.node_name.as_deref(), Some("w1"));
        assert_eq!(phase(&c.document), Some("Scheduled"));
        assert_eq!(c.document["status"]["workerName"], "w1");
    }

    #[test]
    fn node_lifecycle_marks_ready_then_notready() {
        let store = SqliteStore::in_memory().unwrap();
        put_doc(&store, "Worker", "w1", None, ready_worker_doc("w1"));

        let t0 = DateTime::parse_from_rfc3339("2026-06-27T00:00:10Z")
            .unwrap()
            .with_timezone(&Utc);
        // Fresh lease → worker becomes Ready.
        put_doc(
            &store,
            "Lease",
            "w1",
            None,
            serde_json::json!({
                "metadata": { "name": "w1" },
                "spec": { "holderIdentity": "w1", "renewTime": "2026-06-27T00:00:05Z", "leaseDurationSeconds": 40 }
            }),
        );
        reconcile_node_lifecycle(&store, t0, Duration::from_secs(300)).unwrap();
        assert!(worker_ready(
            &store.get("Worker", "w1").unwrap().unwrap().document
        ));

        // Much later, lease is stale → worker becomes NotReady.
        let t1 = DateTime::parse_from_rfc3339("2026-06-27T00:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        reconcile_node_lifecycle(&store, t1, Duration::from_secs(300)).unwrap();
        assert!(!worker_ready(
            &store.get("Worker", "w1").unwrap().unwrap().document
        ));
    }

    #[test]
    fn node_lifecycle_evicts_containers_after_grace() {
        let store = SqliteStore::in_memory().unwrap();
        // Worker already NotReady since t0.
        let mut w = ready_worker_doc("w1");
        w["status"]["conditions"] = serde_json::json!([{
            "conditionType": "Ready", "status": false,
            "lastTransitionTime": "2026-06-27T00:00:00Z", "reason": "LeaseExpired"
        }]);
        put_doc(&store, "Worker", "w1", None, w);
        put_doc(
            &store,
            "Container",
            "c1",
            Some("w1"),
            serde_json::json!({
                "metadata": { "name": "c1" },
                "spec": { "image": "img", "nodeName": "w1" },
                "status": { "phase": "Running" }
            }),
        );

        // No lease at all → not fresh; 10 minutes past the NotReady transition.
        let now = DateTime::parse_from_rfc3339("2026-06-27T00:10:00Z")
            .unwrap()
            .with_timezone(&Utc);
        reconcile_node_lifecycle(&store, now, Duration::from_secs(300)).unwrap();
        let c = store.get("Container", "c1").unwrap().unwrap();
        assert_eq!(phase(&c.document), Some("Unknown"));
    }
}
