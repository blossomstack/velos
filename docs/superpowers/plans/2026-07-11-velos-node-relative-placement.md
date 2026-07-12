# Node-relative Placement (Scheduler Phase 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users control where a container runs via Kubernetes-style `nodeName`, `nodeSelector`, node affinity, and taints/tolerations, honored by a pure filter→score→pick scheduler.

**Architecture:** Extend the wire schema with placement fields; rewrite the pure `velos-scheduler` crate from first-fit into filter (hard predicates) → score (soft preferences) → pick (input-order tie-break); teach the `velos-server` controller to translate placement JSON into the scheduler's own domain types; split "desired" (`spec.nodeName`) from "actual" (`status.workerName`) so a user-set pin is a request, not a binding.

**Tech Stack:** Rust (workspace crates), fluorite IDL (`.fl` → generated wire types via `velos-models` build.rs), axum server, SQLite store, `serde_json::Value` for server/controller logic.

## Global Constraints

- Clippy (production code, mandatory): `unwrap_used = deny`, `expect_used = deny`, `panic = deny`. Tests opt out with `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` at the top of the test module/file (see existing tests).
- No wildcard (`_ =>`) match arms on domain enums (Principle #2) — match all variants explicitly. (`_` is fine for non-exhaustive parsing of untyped `serde_json::Value`.)
- Fluorite types are the **wire contract only** (Principle #7). Scheduler/controller logic uses its **own** domain types, never fluorite types. `.fl` field names are `snake_case`; the wire (JSON) is `camelCase`.
- Semantic types over `String`/`u64` (Principle #1): worker identity is `WorkerName`, not `String`.
- Scheduling decisions are **pure functions** with no I/O (Principle #5). Side effects (store writes) live only in `reconcile_scheduling`.
- Fail closed (Principle #6): reject malformed specs at admission; never silently drop — unschedulable containers record a reason.
- Editing `crates/models/fluorite/velos.fl` regenerates Rust types automatically on `cargo build` (velos-models `build.rs` runs `fluorite_codegen`). No manual codegen command.
- Gate before every commit: `cargo fmt --all` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` must pass.

---

## File Structure

- `crates/models/fluorite/velos.fl` — **modify**: new placement enums/structs; new `ContainerSpec`/`WorkerSpec` fields.
- `crates/models/src/lib.rs` — **modify**: widen `Container::pending`'s `ContainerSpec::new` call.
- `crates/scheduler/src/lib.rs` — **modify (major)**: domain types, `WorkerView`/`PlacementRequest`, filter→score→pick, `Placement`/`RejectReason`.
- `crates/server/src/controllers.rs` — **modify**: parse placement JSON → `PlacementRequest`; worker labels/taints → `WorkerView`; `pending_containers` keys off `status.workerName`; `reconcile_scheduling` writes bindings + unschedulable reasons.
- `crates/server/src/lib.rs` — **modify**: `extract_node_name` reads `status.workerName`; `replace_status` refreshes the index column; `parse_selector` accepts `status.workerName`.
- `crates/veloslet/src/client.rs` — **modify**: `list_assigned` queries `status.workerName`.
- `crates/tests/tests/e2e.rs` — **modify**: add placement e2e scenarios.

---

## Task 1: Wire schema — placement types + spec fields

**Files:**
- Modify: `crates/models/fluorite/velos.fl`
- Modify: `crates/models/src/lib.rs`
- Test: `crates/models/src/lib.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces (wire, camelCase JSON): `ContainerSpec.nodeSelector` (object), `ContainerSpec.affinity` (`{required:[…], preferred:[…]}` or null), `ContainerSpec.tolerations` (array), `WorkerSpec.taints` (array). Enums serialize as their variant name string (`"In"`, `"NoSchedule"`, …).

- [ ] **Step 1: Write the failing test**

Add to `crates/models/src/lib.rs` `mod tests`:

```rust
#[test]
fn container_spec_carries_placement_fields_in_camel_case() {
    let c = Container::pending("c1", "alpine:latest");
    let json = serde_json::to_string(&c).unwrap();
    assert!(json.contains("\"nodeSelector\""), "json was: {json}");
    assert!(json.contains("\"tolerations\""), "json was: {json}");
    // affinity defaults to null (Option), still present as a key:
    assert!(json.contains("\"affinity\""), "json was: {json}");
    let back: Container = serde_json::from_str(&json).unwrap();
    assert_eq!(c, back);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p velos-models container_spec_carries_placement_fields_in_camel_case`
Expected: FAIL to compile — `ContainerSpec::new` arity mismatch / fields absent.

- [ ] **Step 3: Add the schema types**

In `crates/models/fluorite/velos.fl`, add these types (place near `ResourceReqs`):

```
enum NodeSelectorOperator { In, NotIn, Exists, DoesNotExist, Gt, Lt }

struct NodeSelectorRequirement {
    key: String,
    operator: NodeSelectorOperator,
    values: Vec<String>,
}

struct NodeSelectorTerm {
    match_expressions: Vec<NodeSelectorRequirement>,
}

struct PreferredSchedulingTerm {
    weight: i32,
    preference: NodeSelectorTerm,
}

struct NodeAffinity {
    required: Vec<NodeSelectorTerm>,
    preferred: Vec<PreferredSchedulingTerm>,
}

enum TaintEffect { NoSchedule, PreferNoSchedule }

struct Taint {
    key: String,
    value: String,
    effect: TaintEffect,
}

enum TolerationOperator { Equal, Exists }

struct Toleration {
    key: String,
    operator: TolerationOperator,
    value: String,
    effect: Option<TaintEffect>,
}
```

Then extend the existing `ContainerSpec` (keep field order; append the three new fields after `node_name`):

```
struct ContainerSpec {
    image: String,
    command: Vec<String>,
    env: Map<String, String>,
    resources: ResourceReqs,
    restart_policy: RestartPolicy,
    node_name: Option<String>,
    node_selector: Map<String, String>,
    affinity: Option<NodeAffinity>,
    tolerations: Vec<Toleration>,
}
```

And extend `WorkerSpec` (append `taints`):

```
struct WorkerSpec {
    unschedulable: bool,
    taints: Vec<Taint>,
}
```

- [ ] **Step 4: Update the convenience constructor**

In `crates/models/src/lib.rs`, `Container::pending`, widen the `ContainerSpec::new` call to pass the three new fields (order matches the schema):

```rust
ContainerSpec::new(
    image.into(),
    Vec::new(),
    HashMap::new(),
    ResourceReqs::new(1, 512 * 1024 * 1024),
    RestartPolicy::Never,
    None,           // node_name
    HashMap::new(), // node_selector
    None,           // affinity
    Vec::new(),     // tolerations
),
```

(If `WorkerSpec::new` is constructed anywhere in Rust, add a trailing `Vec::new()` for `taints`. `grep -rn "WorkerSpec::new" crates/` to confirm — currently none.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p velos-models`
Expected: PASS (both the new test and the existing round-trip tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/models/fluorite/velos.fl crates/models/src/lib.rs
git commit -m "models: add node-relative placement wire types"
```

---

## Task 2: Scheduler skeleton — domain types, filter→pick, base admittance

Rewrites the scheduler onto the new `PlacementRequest`/`Placement` API with **only base admittance** wired in (zero behavior change), and updates the controller so the workspace compiles. Placement predicates come in Tasks 3–7; controller parsing in Task 8.

**Files:**
- Modify: `crates/scheduler/src/lib.rs` (full types + `schedule`/`evaluate`/`plan_bindings` callers)
- Modify: `crates/server/src/controllers.rs` (adapt to new API, empty placement fields)
- Test: `crates/scheduler/src/lib.rs` `mod tests`

**Interfaces:**
- Produces:
  - `pub struct WorkerName(pub String)` — unchanged.
  - `pub struct ResourceRequest { pub cpu: u32, pub memory_bytes: u64 }` — unchanged.
  - `pub struct PlacementRequest { pub resources: ResourceRequest, pub node_name: Option<WorkerName>, pub node_selector: Vec<(String, String)>, pub required: Vec<NodeSelectorTerm>, pub preferred: Vec<PreferredSchedulingTerm>, pub tolerations: Vec<Toleration> }`
  - `pub struct WorkerView { pub name: WorkerName, pub ready: bool, pub unschedulable: bool, pub allocatable: ResourceRequest, pub allocated: ResourceRequest, pub running_containers: u32, pub max_containers: u32, pub labels: HashMap<String, String>, pub taints: Vec<Taint> }`
  - `pub enum Placement { Scheduled(WorkerName), Unschedulable { reason: String } }`
  - `pub fn schedule(req: &PlacementRequest, workers: &[WorkerView]) -> Placement`
  - Domain mirror types: `NodeSelectorOperator`, `NodeSelectorRequirement`, `NodeSelectorTerm`, `PreferredSchedulingTerm`, `TaintEffect`, `Taint`, `TolerationOperator`, `Toleration` (definitions below).

- [ ] **Step 1: Write the failing test (base admittance ported)**

Replace the scheduler's `mod tests` with tests on the new API. The `worker` helper now takes labels/taints defaults:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::too_many_arguments)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn wv(name: &str, ready: bool, unsched: bool, cpu: u32, mem: u64,
          used_cpu: u32, used_mem: u64, running: u32, max: u32) -> WorkerView {
        WorkerView {
            name: WorkerName(name.into()),
            ready, unschedulable: unsched,
            allocatable: ResourceRequest { cpu, memory_bytes: mem },
            allocated: ResourceRequest { cpu: used_cpu, memory_bytes: used_mem },
            running_containers: running, max_containers: max,
            labels: std::collections::HashMap::new(),
            taints: Vec::new(),
        }
    }

    fn req(cpu: u32, mem: u64) -> PlacementRequest {
        PlacementRequest {
            resources: ResourceRequest { cpu, memory_bytes: mem },
            node_name: None, node_selector: Vec::new(),
            required: Vec::new(), preferred: Vec::new(), tolerations: Vec::new(),
        }
    }

    #[test]
    fn picks_first_fitting_ready_worker() {
        let workers = vec![
            wv("w1", true, false, 1, 8 * GB, 0, 0, 0, 10),
            wv("w2", true, false, 4, 8 * GB, 0, 0, 0, 10),
        ];
        assert_eq!(schedule(&req(2, 2 * GB), &workers),
                   Placement::Scheduled(WorkerName("w2".into())));
    }

    #[test]
    fn skips_not_ready_and_unschedulable() {
        let workers = vec![
            wv("w1", false, false, 8, 16 * GB, 0, 0, 0, 10),
            wv("w2", true, true, 8, 16 * GB, 0, 0, 0, 10),
            wv("w3", true, false, 8, 16 * GB, 0, 0, 0, 10),
        ];
        assert_eq!(schedule(&req(1, GB), &workers),
                   Placement::Scheduled(WorkerName("w3".into())));
    }

    #[test]
    fn none_when_nothing_fits() {
        let workers = vec![wv("w1", true, false, 8, 16 * GB, 0, 0, 0, 10)];
        assert!(matches!(schedule(&req(64, 256 * GB), &workers),
                         Placement::Unschedulable { .. }));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p velos-scheduler`
Expected: FAIL to compile (new types/`Placement` not defined).

- [ ] **Step 3: Rewrite the scheduler types + core**

Replace the top of `crates/scheduler/src/lib.rs` (keep the module doc comment) with:

```rust
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceRequest {
    pub cpu: u32,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSelectorOperator { In, NotIn, Exists, DoesNotExist, Gt, Lt }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSelectorRequirement {
    pub key: String,
    pub operator: NodeSelectorOperator,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSelectorTerm {
    pub match_expressions: Vec<NodeSelectorRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferredSchedulingTerm {
    pub weight: i32,
    pub preference: NodeSelectorTerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaintEffect { NoSchedule, PreferNoSchedule }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taint {
    pub key: String,
    pub value: String,
    pub effect: TaintEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TolerationOperator { Equal, Exists }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toleration {
    pub key: String,
    pub operator: TolerationOperator,
    pub value: String,
    pub effect: Option<TaintEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRequest {
    pub resources: ResourceRequest,
    pub node_name: Option<WorkerName>,
    pub node_selector: Vec<(String, String)>,
    pub required: Vec<NodeSelectorTerm>,
    pub preferred: Vec<PreferredSchedulingTerm>,
    pub tolerations: Vec<Toleration>,
}

#[derive(Debug, Clone)]
pub struct WorkerView {
    pub name: WorkerName,
    pub ready: bool,
    pub unschedulable: bool,
    pub allocatable: ResourceRequest,
    pub allocated: ResourceRequest,
    pub running_containers: u32,
    pub max_containers: u32,
    pub labels: HashMap<String, String>,
    pub taints: Vec<Taint>,
}

impl WorkerView {
    fn free_cpu(&self) -> u32 {
        self.allocatable.cpu.saturating_sub(self.allocated.cpu)
    }
    fn free_memory(&self) -> u64 {
        self.allocatable.memory_bytes.saturating_sub(self.allocated.memory_bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    NotReady,
    Cordoned,
    Full,
    InsufficientCpu,
    InsufficientMemory,
    NodeNameMismatch,
    LabelMismatch,
    AffinityMismatch,
    UntoleratedTaint,
}

impl RejectReason {
    fn describe(&self) -> &'static str {
        match self {
            RejectReason::NotReady => "NotReady",
            RejectReason::Cordoned => "cordoned",
            RejectReason::Full => "at container capacity",
            RejectReason::InsufficientCpu => "insufficient cpu",
            RejectReason::InsufficientMemory => "insufficient memory",
            RejectReason::NodeNameMismatch => "didn't match nodeName",
            RejectReason::LabelMismatch => "didn't match nodeSelector",
            RejectReason::AffinityMismatch => "didn't match node affinity",
            RejectReason::UntoleratedTaint => "had an untolerated taint",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Scheduled(WorkerName),
    Unschedulable { reason: String },
}

/// Base admittance: readiness, cordon, slot count, and resource fit.
fn admits_base(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    if !w.ready {
        return Err(RejectReason::NotReady);
    }
    if w.unschedulable {
        return Err(RejectReason::Cordoned);
    }
    if w.running_containers >= w.max_containers {
        return Err(RejectReason::Full);
    }
    if w.free_cpu() < req.resources.cpu {
        return Err(RejectReason::InsufficientCpu);
    }
    if w.free_memory() < req.resources.memory_bytes {
        return Err(RejectReason::InsufficientMemory);
    }
    Ok(())
}

/// Total decision for one worker: `Ok(score)` if it admits `req`, else why not.
/// Later tasks add predicates before the `Ok` and scoring in place of `0`.
fn evaluate(req: &PlacementRequest, w: &WorkerView) -> Result<i32, RejectReason> {
    admits_base(req, w)?;
    Ok(0)
}

fn unschedulable_reason(total: usize, _reasons: &[RejectReason]) -> String {
    format!("0/{total} workers available")
}

/// Filter → score → pick. Highest score wins; ties break by input order, so with
/// no soft preferences the result equals the previous first-fit behavior.
pub fn schedule(req: &PlacementRequest, workers: &[WorkerView]) -> Placement {
    let mut best: Option<(WorkerName, i32)> = None;
    let mut reasons: Vec<RejectReason> = Vec::new();
    for w in workers {
        match evaluate(req, w) {
            Ok(score) => {
                let better = match &best {
                    Some((_, bs)) => score > *bs,
                    None => true,
                };
                if better {
                    best = Some((w.name.clone(), score));
                }
            }
            Err(r) => reasons.push(r),
        }
    }
    match best {
        Some((name, _)) => Placement::Scheduled(name),
        None => Placement::Unschedulable {
            reason: unschedulable_reason(workers.len(), &reasons),
        },
    }
}
```

Note: `unschedulable_reason`'s `_reasons` param is unused until Task 7; the leading underscore keeps clippy quiet.

- [ ] **Step 4: Adapt the controller so the workspace compiles**

In `crates/server/src/controllers.rs`:

Update the import:

```rust
use velos_scheduler::{
    Placement, PlacementRequest, ResourceRequest, WorkerName, WorkerView, schedule,
};
```

Change `PendingContainer.request` to hold a `PlacementRequest`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct PendingContainer {
    pub name: String,
    pub request: PlacementRequest,
}
```

Add a wrapper that (for now) fills only resources — real parsing lands in Task 8:

```rust
fn container_placement_request(doc: &Value) -> PlacementRequest {
    PlacementRequest {
        resources: container_request(doc),
        node_name: None,
        node_selector: Vec::new(),
        required: Vec::new(),
        preferred: Vec::new(),
        tolerations: Vec::new(),
    }
}
```

In `pending_containers`, build the request via the wrapper:

```rust
.map(|c| PendingContainer {
    name: c.name.clone(),
    request: container_placement_request(&c.document),
})
```

In `build_worker_views`, add the two new `WorkerView` fields (empty for now — Task 8 populates them):

```rust
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
                running_containers: running,
                max_containers: u64_at(&w.document, &["status", "allocatable", "maxContainers"])
                    .map(|c| c as u32)
                    .unwrap_or(0),
                labels: std::collections::HashMap::new(),
                taints: Vec::new(),
            }
```

Update `plan_bindings` to the new return of `schedule` and `resources` field:

```rust
pub fn plan_bindings(pending: &[PendingContainer], workers: &[WorkerView]) -> Vec<Binding> {
    let mut views = workers.to_vec();
    let mut out = Vec::new();
    for p in pending {
        if let Placement::Scheduled(WorkerName(name)) = schedule(&p.request, &views) {
            if let Some(w) = views.iter_mut().find(|w| w.name.0 == name) {
                w.allocated.cpu += p.request.resources.cpu;
                w.allocated.memory_bytes += p.request.resources.memory_bytes;
                w.running_containers += 1;
            }
            out.push(Binding {
                container: p.name.clone(),
                worker: name,
            });
        }
    }
    out
}
```

If `controllers.rs` tests construct `WorkerView`/`ResourceRequest` directly (they do — a `view`/`req` helper near line 432), update those helpers to add `labels: HashMap::new(), taints: Vec::new()` and to wrap `req` resources in a `PlacementRequest` the same way as the scheduler test helpers above.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p velos-scheduler -p velos-server`
Expected: PASS (behavior unchanged; only types/plumbing moved).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/scheduler/src/lib.rs crates/server/src/controllers.rs
git commit -m "scheduler: filter/score/pick skeleton with base admittance"
```

---

## Task 3: Hard filters — nodeName and nodeSelector

**Files:**
- Modify: `crates/scheduler/src/lib.rs`
- Test: `crates/scheduler/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `PlacementRequest.node_name`, `PlacementRequest.node_selector`, `WorkerView.labels`.
- Produces: `evaluate` now rejects on `NodeNameMismatch` / `LabelMismatch`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` (uses a helper to set labels/pins):

```rust
fn wv_labeled(name: &str, labels: &[(&str, &str)]) -> WorkerView {
    let mut w = wv(name, true, false, 8, 16 * GB, 0, 0, 0, 10);
    w.labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    w
}

#[test]
fn honors_node_name_pin() {
    let workers = vec![wv("w1", true, false, 8, 16 * GB, 0, 0, 0, 10),
                       wv("w2", true, false, 8, 16 * GB, 0, 0, 0, 10)];
    let mut r = req(1, GB);
    r.node_name = Some(WorkerName("w2".into()));
    assert_eq!(schedule(&r, &workers), Placement::Scheduled(WorkerName("w2".into())));
}

#[test]
fn pin_to_full_worker_is_unschedulable_not_forced() {
    let workers = vec![wv("w1", true, false, 8, 16 * GB, 0, 0, 0, 10),
                       wv("w2", true, false, 1, GB, 1, GB, 1, 1)]; // w2 full
    let mut r = req(1, GB);
    r.node_name = Some(WorkerName("w2".into()));
    assert!(matches!(schedule(&r, &workers), Placement::Unschedulable { .. }));
}

#[test]
fn node_selector_excludes_non_matching() {
    let workers = vec![wv_labeled("w1", &[("gpu", "false")]),
                       wv_labeled("w2", &[("gpu", "true")])];
    let mut r = req(1, GB);
    r.node_selector = vec![("gpu".into(), "true".into())];
    assert_eq!(schedule(&r, &workers), Placement::Scheduled(WorkerName("w2".into())));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-scheduler honors_node_name_pin node_selector_excludes_non_matching pin_to_full`
Expected: FAIL — `honors_node_name_pin` picks `w1` (pin ignored), selector test picks `w1`.

- [ ] **Step 3: Add the predicates**

Add these functions above `evaluate`:

```rust
fn matches_node_name(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    match &req.node_name {
        Some(n) if *n != w.name => Err(RejectReason::NodeNameMismatch),
        _ => Ok(()),
    }
}

fn node_selector_matches(sel: &[(String, String)], labels: &HashMap<String, String>) -> bool {
    sel.iter()
        .all(|(k, v)| labels.get(k).map(|x| x == v).unwrap_or(false))
}

fn matches_node_selector(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    if node_selector_matches(&req.node_selector, &w.labels) {
        Ok(())
    } else {
        Err(RejectReason::LabelMismatch)
    }
}
```

Update `evaluate`:

```rust
fn evaluate(req: &PlacementRequest, w: &WorkerView) -> Result<i32, RejectReason> {
    admits_base(req, w)?;
    matches_node_name(req, w)?;
    matches_node_selector(req, w)?;
    Ok(0)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-scheduler`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/scheduler/src/lib.rs
git commit -m "scheduler: honor nodeName pin and nodeSelector"
```

---

## Task 4: Node affinity (required) — all operators

**Files:**
- Modify: `crates/scheduler/src/lib.rs`
- Test: `crates/scheduler/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `PlacementRequest.required` (`Vec<NodeSelectorTerm>`), `WorkerView.labels`.
- Produces: `evaluate` rejects on `AffinityMismatch`; helper `required_matches` reused by scoring in Task 6.

- [ ] **Step 1: Write the failing tests (table-driven, all operators)**

```rust
fn term(key: &str, op: NodeSelectorOperator, vals: &[&str]) -> NodeSelectorTerm {
    NodeSelectorTerm {
        match_expressions: vec![NodeSelectorRequirement {
            key: key.into(),
            operator: op,
            values: vals.iter().map(|s| s.to_string()).collect(),
        }],
    }
}

#[test]
fn required_affinity_operators() {
    use NodeSelectorOperator::*;
    let w = wv_labeled("w", &[("zone", "us"), ("cores", "8")]);
    let cases: &[(NodeSelectorTerm, bool)] = &[
        (term("zone", In, &["us", "eu"]), true),
        (term("zone", In, &["eu"]), false),
        (term("zone", NotIn, &["eu"]), true),
        (term("zone", NotIn, &["us"]), false),
        (term("gpu", NotIn, &["true"]), true), // absent key: NotIn matches
        (term("zone", Exists, &[]), true),
        (term("gpu", Exists, &[]), false),
        (term("gpu", DoesNotExist, &[]), true),
        (term("zone", DoesNotExist, &[]), false),
        (term("cores", Gt, &["4"]), true),
        (term("cores", Gt, &["8"]), false),
        (term("cores", Lt, &["16"]), true),
        (term("zone", Gt, &["4"]), false), // non-integer label: no match
    ];
    for (t, want) in cases {
        let mut r = req(1, GB);
        r.required = vec![t.clone()];
        let got = matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Scheduled(_));
        assert_eq!(got, *want, "term {t:?}");
    }
}

#[test]
fn required_terms_are_ored_expressions_anded() {
    let w = wv_labeled("w", &[("a", "1"), ("b", "2")]);
    // Two terms OR'd: second matches -> scheduled.
    let mut r = req(1, GB);
    r.required = vec![term("a", NodeSelectorOperator::In, &["9"]),
                      term("b", NodeSelectorOperator::In, &["2"])];
    assert!(matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Scheduled(_)));
    // One term, two expressions AND'd: one fails -> unschedulable.
    r.required = vec![NodeSelectorTerm { match_expressions: vec![
        NodeSelectorRequirement { key: "a".into(), operator: NodeSelectorOperator::In, values: vec!["1".into()] },
        NodeSelectorRequirement { key: "b".into(), operator: NodeSelectorOperator::In, values: vec!["9".into()] },
    ]}];
    assert!(matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Unschedulable { .. }));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-scheduler required_affinity_operators required_terms_are_ored`
Expected: FAIL (required is ignored so all "false" cases schedule).

- [ ] **Step 3: Implement the matchers**

Add above `evaluate`:

```rust
fn cmp_int(label: Option<&String>, want: Option<&String>, f: impl Fn(i64, i64) -> bool) -> bool {
    match (
        label.and_then(|s| s.parse::<i64>().ok()),
        want.and_then(|s| s.parse::<i64>().ok()),
    ) {
        (Some(a), Some(b)) => f(a, b),
        _ => false,
    }
}

fn requirement_matches(r: &NodeSelectorRequirement, labels: &HashMap<String, String>) -> bool {
    match r.operator {
        NodeSelectorOperator::In => labels.get(&r.key).map(|v| r.values.contains(v)).unwrap_or(false),
        NodeSelectorOperator::NotIn => labels.get(&r.key).map(|v| !r.values.contains(v)).unwrap_or(true),
        NodeSelectorOperator::Exists => labels.contains_key(&r.key),
        NodeSelectorOperator::DoesNotExist => !labels.contains_key(&r.key),
        NodeSelectorOperator::Gt => cmp_int(labels.get(&r.key), r.values.first(), |a, b| a > b),
        NodeSelectorOperator::Lt => cmp_int(labels.get(&r.key), r.values.first(), |a, b| a < b),
    }
}

fn term_matches(term: &NodeSelectorTerm, labels: &HashMap<String, String>) -> bool {
    term.match_expressions.iter().all(|r| requirement_matches(r, labels))
}

fn required_matches(terms: &[NodeSelectorTerm], labels: &HashMap<String, String>) -> bool {
    terms.is_empty() || terms.iter().any(|t| term_matches(t, labels))
}

fn matches_required_affinity(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    if required_matches(&req.required, &w.labels) {
        Ok(())
    } else {
        Err(RejectReason::AffinityMismatch)
    }
}
```

Update `evaluate`:

```rust
fn evaluate(req: &PlacementRequest, w: &WorkerView) -> Result<i32, RejectReason> {
    admits_base(req, w)?;
    matches_node_name(req, w)?;
    matches_node_selector(req, w)?;
    matches_required_affinity(req, w)?;
    Ok(0)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-scheduler`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/scheduler/src/lib.rs
git commit -m "scheduler: required node affinity with In/NotIn/Exists/Gt/Lt"
```

---

## Task 5: Taints and tolerations (NoSchedule filter)

**Files:**
- Modify: `crates/scheduler/src/lib.rs`
- Test: `crates/scheduler/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `WorkerView.taints`, `PlacementRequest.tolerations`.
- Produces: `evaluate` rejects on `UntoleratedTaint`; helper `tolerates` reused by scoring in Task 6.

- [ ] **Step 1: Write the failing tests (toleration matrix)**

```rust
fn wv_tainted(name: &str, taints: Vec<Taint>) -> WorkerView {
    let mut w = wv(name, true, false, 8, 16 * GB, 0, 0, 0, 10);
    w.taints = taints;
    w
}

#[test]
fn no_schedule_taint_blocks_unless_tolerated() {
    let taint = Taint { key: "gpu".into(), value: "true".into(), effect: TaintEffect::NoSchedule };
    let w = wv_tainted("w", vec![taint.clone()]);

    // No toleration -> blocked.
    assert!(matches!(schedule(&req(1, GB), std::slice::from_ref(&w)), Placement::Unschedulable { .. }));

    // Equal match -> ok.
    let mut r = req(1, GB);
    r.tolerations = vec![Toleration { key: "gpu".into(), operator: TolerationOperator::Equal,
        value: "true".into(), effect: Some(TaintEffect::NoSchedule) }];
    assert!(matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Scheduled(_)));

    // Equal wrong value -> blocked.
    let mut r = req(1, GB);
    r.tolerations = vec![Toleration { key: "gpu".into(), operator: TolerationOperator::Equal,
        value: "false".into(), effect: None }];
    assert!(matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Unschedulable { .. }));

    // Exists by key -> ok.
    let mut r = req(1, GB);
    r.tolerations = vec![Toleration { key: "gpu".into(), operator: TolerationOperator::Exists,
        value: String::new(), effect: None }];
    assert!(matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Scheduled(_)));

    // Empty-key Exists tolerates everything -> ok.
    let mut r = req(1, GB);
    r.tolerations = vec![Toleration { key: String::new(), operator: TolerationOperator::Exists,
        value: String::new(), effect: None }];
    assert!(matches!(schedule(&r, std::slice::from_ref(&w)), Placement::Scheduled(_)));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-scheduler no_schedule_taint_blocks_unless_tolerated`
Expected: FAIL (taints ignored — untolerated case still schedules).

- [ ] **Step 3: Implement toleration matching**

Add above `evaluate`:

```rust
fn tolerates(tol: &Toleration, taint: &Taint) -> bool {
    if let Some(eff) = tol.effect {
        if eff != taint.effect {
            return false;
        }
    }
    match tol.operator {
        TolerationOperator::Exists => tol.key.is_empty() || tol.key == taint.key,
        TolerationOperator::Equal => tol.key == taint.key && tol.value == taint.value,
    }
}

fn tolerates_taints(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    for taint in &w.taints {
        if taint.effect == TaintEffect::NoSchedule
            && !req.tolerations.iter().any(|t| tolerates(t, taint))
        {
            return Err(RejectReason::UntoleratedTaint);
        }
    }
    Ok(())
}
```

Update `evaluate`:

```rust
fn evaluate(req: &PlacementRequest, w: &WorkerView) -> Result<i32, RejectReason> {
    admits_base(req, w)?;
    matches_node_name(req, w)?;
    matches_node_selector(req, w)?;
    matches_required_affinity(req, w)?;
    tolerates_taints(req, w)?;
    Ok(0)
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-scheduler`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/scheduler/src/lib.rs
git commit -m "scheduler: NoSchedule taints with Equal/Exists tolerations"
```

---

## Task 6: Scoring — preferred affinity and PreferNoSchedule penalty

**Files:**
- Modify: `crates/scheduler/src/lib.rs`
- Test: `crates/scheduler/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `PlacementRequest.preferred`, `WorkerView.taints` (`PreferNoSchedule`), helpers `term_matches`/`tolerates`.
- Produces: `evaluate` returns a real score; higher score wins, ties by input order.

- [ ] **Step 1: Write the failing tests**

```rust
fn pref(weight: i32, t: NodeSelectorTerm) -> PreferredSchedulingTerm {
    PreferredSchedulingTerm { weight, preference: t }
}

#[test]
fn preferred_affinity_picks_higher_score() {
    let w1 = wv_labeled("w1", &[("fast", "false")]);
    let w2 = wv_labeled("w2", &[("fast", "true")]);
    let mut r = req(1, GB);
    r.preferred = vec![pref(50, term("fast", NodeSelectorOperator::In, &["true"]))];
    // w1 first in order but w2 scores higher -> w2 wins.
    assert_eq!(schedule(&r, &[w1, w2]), Placement::Scheduled(WorkerName("w2".into())));
}

#[test]
fn zero_preference_keeps_first_fit_order() {
    let w1 = wv_labeled("w1", &[("fast", "true")]);
    let w2 = wv_labeled("w2", &[("fast", "true")]);
    // No preferred terms -> both score 0 -> first in order wins.
    assert_eq!(schedule(&req(1, GB), &[w1, w2]), Placement::Scheduled(WorkerName("w1".into())));
}

#[test]
fn prefer_no_schedule_penalizes_but_allows() {
    let clean = wv("w1", true, false, 8, 16 * GB, 0, 0, 0, 10);
    let tainted = wv_tainted("w2", vec![Taint {
        key: "spot".into(), value: String::new(), effect: TaintEffect::PreferNoSchedule }]);
    // Put tainted first; penalty should still make the clean worker win.
    assert_eq!(schedule(&req(1, GB), &[tainted, clean]),
               Placement::Scheduled(WorkerName("w1".into())));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-scheduler preferred_affinity_picks_higher_score prefer_no_schedule_penalizes`
Expected: FAIL (score always 0 → input-order wins, so `w1`/`tainted` win).

- [ ] **Step 3: Implement scoring**

Add above `evaluate`:

```rust
const PREFER_NO_SCHEDULE_PENALTY: i32 = 100;

fn score(req: &PlacementRequest, w: &WorkerView) -> i32 {
    let mut s = 0;
    for term in &req.preferred {
        if term_matches(&term.preference, &w.labels) {
            s += term.weight;
        }
    }
    for taint in &w.taints {
        if taint.effect == TaintEffect::PreferNoSchedule
            && !req.tolerations.iter().any(|t| tolerates(t, taint))
        {
            s -= PREFER_NO_SCHEDULE_PENALTY;
        }
    }
    s
}
```

Change `evaluate`'s final line from `Ok(0)` to:

```rust
    Ok(score(req, w))
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-scheduler`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/scheduler/src/lib.rs
git commit -m "scheduler: score preferred affinity and PreferNoSchedule taints"
```

---

## Task 7: Unschedulable reason strings

**Files:**
- Modify: `crates/scheduler/src/lib.rs`
- Test: `crates/scheduler/src/lib.rs` `mod tests`

**Interfaces:**
- Produces: `Placement::Unschedulable { reason }` carries a K8s-style breakdown (`"0/N workers available: …"`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn unschedulable_reason_tallies_causes() {
    let workers = vec![
        wv("w1", false, false, 8, 16 * GB, 0, 0, 0, 10),           // NotReady
        wv_labeled("w2", &[("gpu", "false")]),                     // LabelMismatch
        wv("w3", true, false, 1, GB, 0, 0, 0, 10),                 // InsufficientCpu
    ];
    let mut r = req(2, GB);
    r.node_selector = vec![("gpu".into(), "true".into())];
    let Placement::Unschedulable { reason } = schedule(&r, &workers) else {
        panic!("expected unschedulable");
    };
    assert!(reason.starts_with("0/3 workers available:"), "{reason}");
    assert!(reason.contains("NotReady"), "{reason}");
    assert!(reason.contains("didn't match nodeSelector"), "{reason}");
    assert!(reason.contains("insufficient cpu"), "{reason}");
}

#[test]
fn no_workers_registered_reason() {
    let Placement::Unschedulable { reason } = schedule(&req(1, GB), &[]) else {
        panic!("expected unschedulable");
    };
    assert_eq!(reason, "no workers registered");
}
```

Note: base admittance short-circuits before nodeSelector, so `w3` (fits cpu? needs 2, has 1) reports `InsufficientCpu`; ensure the request needs 2 cpu as above. `w2` passes base admittance (8 cpu) then fails nodeSelector.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-scheduler unschedulable_reason_tallies_causes no_workers_registered_reason`
Expected: FAIL (current reason is the bare `"0/N workers available"`).

- [ ] **Step 3: Implement the breakdown**

Replace `unschedulable_reason`:

```rust
fn unschedulable_reason(total: usize, reasons: &[RejectReason]) -> String {
    if total == 0 {
        return "no workers registered".to_string();
    }
    let mut counts: Vec<(RejectReason, usize)> = Vec::new();
    for r in reasons {
        match counts.iter_mut().find(|(k, _)| k == r) {
            Some((_, c)) => *c += 1,
            None => counts.push((*r, 1)),
        }
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(r, c)| format!("{c} {}", r.describe()))
        .collect();
    format!("0/{total} workers available: {}", parts.join(", "))
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-scheduler`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/scheduler/src/lib.rs
git commit -m "scheduler: K8s-style unschedulable reason breakdown"
```

---

## Task 8: Controller parses placement fields from documents

Fills the placement request and worker views from real JSON (the wrappers from Task 2 return empty). After this the scheduler honors placement end-to-end at the controller layer.

**Files:**
- Modify: `crates/server/src/controllers.rs`
- Test: `crates/server/src/controllers.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: scheduler domain types (`NodeSelectorOperator`, `NodeSelectorRequirement`, `NodeSelectorTerm`, `PreferredSchedulingTerm`, `TaintEffect`, `Taint`, `TolerationOperator`, `Toleration`).
- Produces: `container_placement_request(doc) -> PlacementRequest` fully populated; `build_worker_views` populates `labels`/`taints`.

- [ ] **Step 1: Write the failing test**

Add to `controllers.rs` `mod tests`:

```rust
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
    assert_eq!(p.node_selector, vec![("gpu".to_string(), "true".to_string())]);
    assert_eq!(p.required.len(), 1);
    assert_eq!(p.preferred.len(), 1);
    assert_eq!(p.preferred[0].weight, 10);
    assert_eq!(p.tolerations.len(), 1);
}

#[test]
fn parses_worker_labels_and_taints() {
    let workers = [make_worker_doc("w1", &[("gpu", "true")],
        serde_json::json!([{ "key": "spot", "value": "", "effect": "NoSchedule" }]))];
    let views = build_worker_views(&workers, &[]);
    assert_eq!(views[0].labels.get("gpu").map(String::as_str), Some("true"));
    assert_eq!(views[0].taints.len(), 1);
    assert_eq!(views[0].taints[0].effect, TaintEffect::NoSchedule);
}
```

Add a small test helper near the existing worker-doc helper (`make_worker_doc`) that builds a `StoredObject` with `metadata.labels` and `spec.taints`; model it on the existing worker helper around line 505–525 (which already sets `spec.unschedulable`). It must set `metadata.labels` and `spec.taints` in the document and populate `StoredObject.labels`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-server parses_placement_from_spec parses_worker_labels_and_taints`
Expected: FAIL (wrapper returns empty; worker views have empty labels/taints).

- [ ] **Step 3: Implement the parsers**

Extend the scheduler import in `controllers.rs`:

```rust
use velos_scheduler::{
    NodeSelectorOperator, NodeSelectorRequirement, NodeSelectorTerm, Placement, PlacementRequest,
    PreferredSchedulingTerm, ResourceRequest, Taint, TaintEffect, Toleration, TolerationOperator,
    WorkerName, WorkerView, schedule,
};
```

Replace the `container_placement_request` stub and add parsers:

```rust
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
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    Some(NodeSelectorRequirement { key, operator, values })
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
    let effect = v.get("effect").and_then(Value::as_str).and_then(parse_taint_effect);
    Some(Toleration {
        key: v.get("key").and_then(Value::as_str).unwrap_or("").to_string(),
        operator,
        value: v.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
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
                        value: t.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
                        effect,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

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
```

In `build_worker_views`, replace the two placeholder lines with real parsing:

```rust
                labels: w.labels.clone(),
                taints: w
                    .document
                    .get("spec")
                    .map(parse_taints)
                    .unwrap_or_default(),
```

(`StoredObject.labels` is already the parsed `metadata.labels` map — reuse it.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/server/src/controllers.rs
git commit -m "server: parse placement fields into scheduler requests and worker views"
```

---

## Task 9: Desired/actual split in the controller

Makes `spec.nodeName` a user *request* (not "already bound") and records unschedulable reasons. "Needs scheduling" becomes `phase == Pending && status.workerName` absent; `reconcile_scheduling` stops writing `spec.nodeName` and writes `status.message` only when it changes.

**Files:**
- Modify: `crates/server/src/controllers.rs`
- Test: `crates/server/src/controllers.rs` `mod tests`

**Interfaces:**
- Consumes: `schedule` returning `Placement`.
- Produces: `plan_bindings` returns `Vec<PlacementOutcome>`; `reconcile_scheduling` binds and/or records reasons.
  - `pub enum PlacementOutcome { Bind(Binding), Unschedulable { container: String, reason: String } }`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn schedules_container_with_user_set_node_name() {
    // A Pending container that already carries spec.nodeName must still be scheduled
    // (it is a request, not a binding) as long as status.workerName is absent.
    let mut c = make_container_doc("c1", "Pending", None); // helper: no status.workerName
    c.document["spec"]["nodeName"] = serde_json::json!("w1");
    let workers = [make_worker_doc("w1", &[], serde_json::json!([]))];
    let pending = pending_containers(std::slice::from_ref(&c));
    assert_eq!(pending.len(), 1, "user-pinned Pending container must be schedulable");
    let views = build_worker_views(&workers, &[]);
    let out = plan_bindings(&pending, &views);
    assert!(matches!(out.as_slice(),
        [PlacementOutcome::Bind(b)] if b.worker == "w1"));
}

#[test]
fn already_bound_container_is_not_pending() {
    // status.workerName present -> not a scheduling candidate.
    let c = make_container_doc("c1", "Scheduled", Some("w1"));
    assert!(pending_containers(std::slice::from_ref(&c)).is_empty());
}
```

Add/extend a `make_container_doc(name, phase, worker_name: Option<&str>)` helper (near the existing container helper ~line 505) that sets `status.phase` and, when `worker_name` is `Some`, `status.workerName`. `StoredObject.node_name` in the helper should mirror `worker_name` (the bound-worker invariant), not `spec.nodeName`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-server schedules_container_with_user_set_node_name already_bound_container_is_not_pending`
Expected: FAIL — `pending_containers` still filters on `node_name.is_none()`; `plan_bindings` returns `Vec<Binding>` not `Vec<PlacementOutcome>`.

- [ ] **Step 3: Implement the split**

Add near `Binding`:

```rust
/// The outcome for one pending container in a scheduling pass.
#[derive(Debug, Clone, PartialEq)]
pub enum PlacementOutcome {
    Bind(Binding),
    Unschedulable { container: String, reason: String },
}
```

Add a helper to read `status.workerName`:

```rust
fn worker_name(doc: &Value) -> Option<&str> {
    str_at(doc, &["status", "workerName"])
}
```

Change `pending_containers` to key off `status.workerName`:

```rust
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
```

Change `plan_bindings` to return outcomes:

```rust
pub fn plan_bindings(pending: &[PendingContainer], workers: &[WorkerView]) -> Vec<PlacementOutcome> {
    let mut views = workers.to_vec();
    let mut out = Vec::new();
    for p in pending {
        match schedule(&p.request, &views) {
            Placement::Scheduled(WorkerName(name)) => {
                if let Some(w) = views.iter_mut().find(|w| w.name.0 == name) {
                    w.allocated.cpu += p.request.resources.cpu;
                    w.allocated.memory_bytes += p.request.resources.memory_bytes;
                    w.running_containers += 1;
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
```

Rewrite `reconcile_scheduling` to consume outcomes. Bind writes `status.workerName`, `status.phase=Scheduled`, and the bound-worker index column — **not** `spec.nodeName`. Unschedulable writes `status.message` only when it changes:

```rust
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
                set_phase(&mut obj.document, "Scheduled");
                if let Some(status) = obj.document.get_mut("status").and_then(Value::as_object_mut) {
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
                let current = str_at(&obj.document, &["status", "message"]);
                if current == Some(reason.as_str()) {
                    continue; // no change -> avoid write churn / RV inflation
                }
                let rv = store.next_resource_version()?;
                if let Some(status) = obj.document.get_mut("status").and_then(Value::as_object_mut) {
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
```

(`set_phase` already initializes `status` when absent; the Bind arm relies on it before inserting `workerName`.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-server`
Expected: PASS. Fix any other call sites of `plan_bindings` the compiler flags (match on `PlacementOutcome`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/server/src/controllers.rs
git commit -m "server: split desired nodeName from bound workerName; record unschedulable reasons"
```

---

## Task 10: Server/store bound-worker index + veloslet selector

Repoints the `node_name` index column at the **bound** worker (`status.workerName`), so a user-set `spec.nodeName` no longer marks a container as placed, and renames veloslet's field selector to match.

**Files:**
- Modify: `crates/server/src/lib.rs` (`extract_node_name`, `replace_status`, `parse_selector`)
- Modify: `crates/veloslet/src/client.rs` (`list_assigned`)
- Test: `crates/server/src/lib.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: container documents with `status.workerName`.
- Produces: index column mirrors `status.workerName`; `fieldSelector=status.workerName=<w>` supported (and legacy `spec.nodeName=<w>` accepted as a deprecated alias).

**Migration note:** No data backfill is required. Existing *bound* containers already have `node_name == status.workerName` (the old `reconcile_scheduling` wrote both `spec.nodeName` and `status.workerName` to the bound worker). Only future user-pinned, not-yet-scheduled containers differ, and none exist yet.

- [ ] **Step 1: Write the failing test**

Add to `crates/server/src/lib.rs` `mod tests`:

```rust
#[test]
fn index_column_tracks_bound_worker_not_desired_pin() {
    // A user pins spec.nodeName=w9 but nothing is bound yet: column must be None.
    let doc = serde_json::json!({ "spec": { "nodeName": "w9" }, "status": { "phase": "Pending" } });
    assert_eq!(extract_node_name(&doc), None);
    // Once bound (status.workerName set), the column follows it.
    let doc = serde_json::json!({ "spec": { "nodeName": "w9" }, "status": { "workerName": "w3" } });
    assert_eq!(extract_node_name(&doc).as_deref(), Some("w3"));
}

#[test]
fn field_selector_accepts_status_worker_name() {
    let mut params = std::collections::HashMap::new();
    params.insert("fieldSelector".to_string(), "status.workerName=w3".to_string());
    assert_eq!(parse_selector(&params).unwrap().node_name.as_deref(), Some("w3"));
    // Legacy alias still works.
    let mut params = std::collections::HashMap::new();
    params.insert("fieldSelector".to_string(), "spec.nodeName=w3".to_string());
    assert_eq!(parse_selector(&params).unwrap().node_name.as_deref(), Some("w3"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-server index_column_tracks_bound_worker field_selector_accepts_status_worker_name`
Expected: FAIL (`extract_node_name` reads `spec.nodeName`; selector rejects `status.workerName`).

- [ ] **Step 3: Implement**

In `crates/server/src/lib.rs`, change `extract_node_name` to read the bound worker:

```rust
fn extract_node_name(doc: &Value) -> Option<String> {
    doc.get("status")?
        .get("workerName")?
        .as_str()
        .map(str::to_string)
}
```

In `parse_selector`, accept the new key and keep the old as an alias:

```rust
            if k == "status.workerName" || k == "spec.nodeName" {
                sel.node_name = Some(v.to_string());
            } else {
```

In `replace_status`, refresh the index column from the new status (so status writes keep the column consistent). After inserting the new status and before `state.store.put(&existing)`:

```rust
    existing.node_name = extract_node_name(&existing.document);
```

In `crates/veloslet/src/client.rs`, update `list_assigned`:

```rust
    /// List containers bound to `node` (`fieldSelector=status.workerName=node`).
    pub async fn list_assigned(&self, node: &str) -> Result<Vec<Value>, ClientError> {
        let url = format!(
            "{}/api/v1/containers?fieldSelector=status.workerName={node}",
            self.base
        );
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-server -p veloslet`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/server/src/lib.rs crates/veloslet/src/client.rs
git commit -m "server: index column tracks bound workerName; add status.workerName field selector"
```

---

## Task 11: Admission validation (fail closed)

Reject malformed placement specs at create/replace with `422`, rather than letting the scheduler silently skip them (Principle #6).

**Files:**
- Modify: `crates/server/src/lib.rs` (validate in `create` and `replace` for `Container`/`Worker`)
- Test: `crates/server/src/lib.rs` `mod tests`

**Interfaces:**
- Produces: `fn validate_placement(kind: &str, doc: &Value) -> Result<(), ApiError>` returning `ApiError::BadRequest` (maps to 422/400) on malformed placement.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn rejects_unknown_affinity_operator() {
    let doc = serde_json::json!({ "spec": { "affinity": { "required": [
        { "matchExpressions": [ { "key": "z", "operator": "Nope", "values": [] } ] } ] } } });
    assert!(validate_placement("Container", &doc).is_err());
}

#[test]
fn rejects_gt_with_non_integer_value() {
    let doc = serde_json::json!({ "spec": { "affinity": { "required": [
        { "matchExpressions": [ { "key": "z", "operator": "Gt", "values": ["abc"] } ] } ] } } });
    assert!(validate_placement("Container", &doc).is_err());
}

#[test]
fn rejects_preferred_weight_out_of_range() {
    let doc = serde_json::json!({ "spec": { "affinity": { "preferred": [
        { "weight": 500, "preference": { "matchExpressions": [] } } ] } } });
    assert!(validate_placement("Container", &doc).is_err());
}

#[test]
fn accepts_well_formed_placement() {
    let doc = serde_json::json!({ "spec": { "affinity": { "required": [
        { "matchExpressions": [ { "key": "z", "operator": "In", "values": ["us"] } ] } ],
        "preferred": [ { "weight": 10, "preference": { "matchExpressions": [] } } ] },
        "tolerations": [ { "key": "s", "operator": "Exists" } ] } });
    assert!(validate_placement("Container", &doc).is_ok());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p velos-server rejects_unknown_affinity_operator rejects_gt_with_non_integer_value rejects_preferred_weight_out_of_range accepts_well_formed_placement`
Expected: FAIL (`validate_placement` undefined).

- [ ] **Step 3: Implement validation**

Add to `crates/server/src/lib.rs`:

```rust
fn validate_placement(kind: &str, doc: &Value) -> Result<(), ApiError> {
    let Some(spec) = doc.get("spec") else { return Ok(()) };

    if kind == "Container" {
        if let Some(aff) = spec.get("affinity").filter(|v| v.is_object()) {
            for term in aff.get("required").and_then(Value::as_array).into_iter().flatten() {
                validate_term(term)?;
            }
            for p in aff.get("preferred").and_then(Value::as_array).into_iter().flatten() {
                let weight = p.get("weight").and_then(Value::as_i64).ok_or_else(|| {
                    ApiError::BadRequest("preferred term requires integer weight".into())
                })?;
                if !(1..=100).contains(&weight) {
                    return Err(ApiError::BadRequest("preferred weight must be 1..=100".into()));
                }
                if let Some(pref) = p.get("preference") {
                    validate_term(pref)?;
                }
            }
        }
        for t in spec.get("tolerations").and_then(Value::as_array).into_iter().flatten() {
            match t.get("operator").and_then(Value::as_str) {
                Some("Equal") | Some("Exists") | None => {}
                Some(op) => return Err(ApiError::BadRequest(format!("bad toleration operator: {op}"))),
            }
        }
    }

    if kind == "Worker" {
        for t in spec.get("taints").and_then(Value::as_array).into_iter().flatten() {
            if t.get("key").and_then(Value::as_str).unwrap_or("").is_empty() {
                return Err(ApiError::BadRequest("taint requires a non-empty key".into()));
            }
            match t.get("effect").and_then(Value::as_str) {
                Some("NoSchedule") | Some("PreferNoSchedule") => {}
                other => return Err(ApiError::BadRequest(format!("bad taint effect: {other:?}"))),
            }
        }
    }
    Ok(())
}

fn validate_term(term: &Value) -> Result<(), ApiError> {
    for e in term.get("matchExpressions").and_then(Value::as_array).into_iter().flatten() {
        let op = e.get("operator").and_then(Value::as_str)
            .ok_or_else(|| ApiError::BadRequest("matchExpression requires operator".into()))?;
        match op {
            "In" | "NotIn" | "Exists" | "DoesNotExist" => {}
            "Gt" | "Lt" => {
                let ok = e.get("values").and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(|s| s.parse::<i64>().is_ok())
                    .unwrap_or(false);
                if !ok {
                    return Err(ApiError::BadRequest(format!("{op} requires one integer value")));
                }
            }
            other => return Err(ApiError::BadRequest(format!("bad operator: {other}"))),
        }
    }
    Ok(())
}
```

Call it in `create` (after `extract_name`, before building `StoredObject`) and in `replace` (after computing `body`, before building `StoredObject`):

```rust
    validate_placement(kind, &body)?;
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p velos-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/server/src/lib.rs
git commit -m "server: validate placement specs at admission (fail closed)"
```

---

## Task 12: End-to-end placement scenarios

**Files:**
- Modify: `crates/tests/tests/e2e.rs`
- Test: same file

**Interfaces:**
- Consumes: the public REST API + `controllers::reconcile_scheduling` (driven directly, as the existing e2e does).

- [ ] **Step 1: Write the failing tests**

Add to `crates/tests/tests/e2e.rs` (reuse the `start`, `post`, `get_container` helpers). Post a worker with labels and capacity, a second plain worker, then containers exercising pin/selector/taint. After creating, drive `controllers::reconcile_scheduling(&*store)` and assert `status.workerName`.

```rust
#[tokio::test]
async fn pins_and_selectors_place_containers() {
    let (base, store) = start().await;
    let http = reqwest::Client::new();

    // Two ready workers; w-gpu is labeled + tainted.
    for (name, labels, taints) in [
        ("w-plain", serde_json::json!({}), serde_json::json!([])),
        ("w-gpu",
         serde_json::json!({ "gpu": "true" }),
         serde_json::json!([{ "key": "gpu", "value": "true", "effect": "NoSchedule" }])),
    ] {
        post(&http, &base, "workers", serde_json::json!({
            "metadata": { "name": name, "labels": labels },
            "spec": { "unschedulable": false, "taints": taints },
            "status": {
                "capacity": { "cpu": 8, "memoryBytes": 17179869184, "maxContainers": 10 },
                "allocatable": { "cpu": 8, "memoryBytes": 17179869184, "maxContainers": 10 },
                "conditions": [{ "conditionType": "Ready", "status": true,
                    "lastTransitionTime": "2026-07-11T00:00:00Z", "reason": "LeaseRenewed" }],
                "addresses": [], "containerRuntimeVersion": "test"
            }
        })).await;
    }

    // (a) nodeSelector gpu=true + toleration -> must land on w-gpu.
    post(&http, &base, "containers", serde_json::json!({
        "metadata": { "name": "c-gpu" },
        "spec": { "image": "img", "resources": { "cpu": 1, "memoryBytes": 1024 },
            "restartPolicy": "Never", "nodeSelector": { "gpu": "true" },
            "tolerations": [{ "key": "gpu", "operator": "Exists" }] },
        "status": { "phase": "Pending" }
    })).await;

    // (b) pin to w-plain.
    post(&http, &base, "containers", serde_json::json!({
        "metadata": { "name": "c-pin" },
        "spec": { "image": "img", "resources": { "cpu": 1, "memoryBytes": 1024 },
            "restartPolicy": "Never", "nodeName": "w-plain" },
        "status": { "phase": "Pending" }
    })).await;

    controllers::reconcile_scheduling(&*store).unwrap();

    let c_gpu = get_container(&http, &base, "c-gpu").await;
    assert_eq!(c_gpu["status"]["workerName"], serde_json::json!("w-gpu"));
    let c_pin = get_container(&http, &base, "c-pin").await;
    assert_eq!(c_pin["status"]["workerName"], serde_json::json!("w-plain"));
}

#[tokio::test]
async fn untolerated_taint_leaves_container_pending_with_reason() {
    let (base, store) = start().await;
    let http = reqwest::Client::new();

    post(&http, &base, "workers", serde_json::json!({
        "metadata": { "name": "w-tainted", "labels": {} },
        "spec": { "unschedulable": false,
            "taints": [{ "key": "gpu", "value": "true", "effect": "NoSchedule" }] },
        "status": {
            "capacity": { "cpu": 8, "memoryBytes": 17179869184, "maxContainers": 10 },
            "allocatable": { "cpu": 8, "memoryBytes": 17179869184, "maxContainers": 10 },
            "conditions": [{ "conditionType": "Ready", "status": true,
                "lastTransitionTime": "2026-07-11T00:00:00Z", "reason": "LeaseRenewed" }],
            "addresses": [], "containerRuntimeVersion": "test"
        }
    })).await;

    post(&http, &base, "containers", serde_json::json!({
        "metadata": { "name": "c-notol" },
        "spec": { "image": "img", "resources": { "cpu": 1, "memoryBytes": 1024 },
            "restartPolicy": "Never" },
        "status": { "phase": "Pending" }
    })).await;

    controllers::reconcile_scheduling(&*store).unwrap();

    let c = get_container(&http, &base, "c-notol").await;
    assert_eq!(c["status"]["phase"], serde_json::json!("Pending"));
    assert!(c["status"]["message"].as_str().unwrap().contains("untolerated taint"),
            "message was: {}", c["status"]["message"]);
}
```

- [ ] **Step 2: Run to verify failure (then pass)**

Run: `cargo test -p velos-tests --test e2e pins_and_selectors_place_containers untolerated_taint_leaves_container_pending_with_reason`
Expected: after Tasks 1–11 are implemented, these PASS. If red, the failure pinpoints the integration gap.

(If the tests crate package name differs, use `cargo test --test e2e <name>`; confirm with `cargo test -p velos-tests --test e2e` or `grep name crates/tests/Cargo.toml`.)

- [ ] **Step 3: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings
git add crates/tests/tests/e2e.rs
git commit -m "tests: e2e node-relative placement (pin, selector, taint)"
```

---

## Final verification

- [ ] **Full workspace gate:**

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all green.

- [ ] **Docs:** update `README.md` / `docs/getting-started.md` with a short "Placement" subsection (nodeName, nodeSelector, affinity, taints/tolerations) — one paragraph plus a `nodeSelector` example. Commit as `docs: document node-relative placement`.

---

## Self-Review

**Spec coverage:**
- Desired/actual split → Tasks 9 (controller) + 10 (server/store/veloslet). ✓
- Wire schema (nodeSelector, affinity, tolerations, taints) → Task 1. ✓
- Scheduler filter→score→pick, pure, zero-config parity → Tasks 2–7 (parity asserted in Task 6 `zero_preference_keeps_first_fit_order`). ✓
- Unsatisfiable → Pending + reason in `status.message` → Task 7 (string) + Task 9 (write, no churn). ✓
- Pin validated not forced → Task 3 `pin_to_full_worker_is_unschedulable_not_forced`. ✓
- Admission fail-closed → Task 11. ✓
- Field-selector rename + deprecated alias → Task 10. ✓
- e2e (pin, selector, taint) → Task 12. ✓
- Out-of-scope (inter-container affinity, topology, preemption, NoExecute) → not implemented, by design. ✓

**Placeholder scan:** no TBD/TODO; every code step shows complete code. ✓

**Type consistency:** `PlacementRequest`, `WorkerView` (with `labels`/`taints`), `Placement`, `PlacementOutcome`, `RejectReason`, and the domain mirror types are defined in Task 2/4/5 and consumed with the same names/fields in Tasks 6–12. `schedule` returns `Placement` throughout; `plan_bindings` returns `Vec<Binding>` in Task 2 then `Vec<PlacementOutcome>` from Task 9 on (call sites updated in the same task). ✓
