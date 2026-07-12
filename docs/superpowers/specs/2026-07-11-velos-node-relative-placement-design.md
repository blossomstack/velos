# Node-relative placement (scheduler Phase 1) — design

**Date:** 2026-07-11

Give users control over **where** a container/sandbox runs, using Kubernetes-style
node-relative placement primitives:

- **`nodeName`** — pin a container to one named worker.
- **`nodeSelector`** — require workers to carry matching labels.
- **Node affinity** — richer required (hard) and preferred (soft) label rules.
- **Taints / tolerations** — let workers *repel* containers unless explicitly tolerated.

This turns the scheduler from a first-fit function into the Kubernetes model —
**filter (hard predicates) → score (soft preferences) → pick highest** — which is
the extensible foundation later phases plug into.

This is **Phase 1** of a decomposed roadmap. Explicitly out of scope (own specs
later): inter-container affinity/anti-affinity, topology spread, and
priority/preemption. See *Future phases*.

## Background (current state)

- The scheduler (`crates/scheduler/src/lib.rs`) is a pure, I/O-free
  `fn schedule(req: &ResourceRequest, workers: &[WorkerView]) -> Option<WorkerName>`
  doing **first-fit**: the first `ready && !unschedulable` worker with a free
  container slot and enough free cpu/memory. Its doc comment already anticipates
  this work: *"This seam later absorbs affinity, bin-packing, and taints without
  changing callers."* Users have no say in placement today.
- `ContainerSpec` already has `node_name: Option<String>`, but it is used as the
  scheduler's **binding output**, not a user request. `ObjectMeta` on both
  `Container` and `Worker` already carries `labels` / `annotations`; nothing
  matches against worker labels yet.
- The store (`crates/store`) persists each object as an opaque JSON `document`
  plus index columns (`name`, `uid`, `resource_version`, `node_name`, `labels`).
  Per **Principle #7**, `node_name` currently indexes `spec.nodeName`
  (`server/src/lib.rs::extract_node_name`).
- Scheduling flow in `crates/server/src/controllers.rs`:
  - `pending_containers` = `phase == "Pending" && node_name.is_none()` (the
    `node_name.is_none()` check is what makes a set `spec.nodeName` mean
    "already placed").
  - `build_worker_views` parses each worker's readiness/capacity/allocation into a
    `WorkerView`; `container_request` parses `spec.resources` into a
    `ResourceRequest`. **The controller already translates JSON documents into the
    scheduler's own domain types** — the pattern this design extends.
  - `plan_bindings` places pending containers in a single pass, accounting for
    prior placements so one worker is not overcommitted within the tick.
  - `reconcile_scheduling` writes `spec.nodeName`, `status.workerName`,
    `status.phase = "Scheduled"`, and the `node_name` index column on bind.
- veloslet fetches the containers assigned to it via a field selector on
  `spec.nodeName` (`server/src/lib.rs` maps `fieldSelector=spec.nodeName=<w>` →
  `Selector.node_name` → the index column). Once a container is Scheduled/Running,
  placement is never re-evaluated.

## Goals

1. Users can express placement on a container via `spec.nodeName`,
   `spec.nodeSelector`, `spec.affinity` (node affinity), and `spec.tolerations`.
2. Workers can carry `spec.taints`.
3. The scheduler honors all of the above as **filter → score → pick**, remaining a
   pure, unit-testable function (Principle #5).
4. A container that cannot be placed **stays `Pending`** and records a
   human-readable reason in `status.message` (Principle #6: no silent drops).
5. **Zero-config behavior is unchanged.** With no placement fields set and no
   worker taints, placement is byte-for-byte identical to today's first-fit.

## Non-goals (Phase 1)

- Inter-container (pod) affinity/anti-affinity, topology spread constraints.
- Priority classes, preemption, eviction. Consequently taint effect `NoExecute`
  is **not** modeled (no running container is ever evicted).
- Re-scheduling a bound container when labels/affinity change
  ("IgnoredDuringExecution" is the only mode — the only one velos needs).
- First-class velosctl flags / dashboard form fields for the new spec fields. They
  flow through as JSON automatically; ergonomic surfacing is a follow-up, noted
  under *Future phases*.

## Approaches considered

- **SQL-engine placement** (push selection into SQLite JSON queries). Attractive
  because labels/documents are already JSON in SQLite, but it moves the scheduling
  *decision* into the store (breaking Principle #5's pure core) and the
  single-pass, don't-overcommit accounting in `plan_bindings` is stateful and
  awkward as SQL. **Rejected.**
- **Minimal `nodeName` + `nodeSelector` only.** Smallest change, but not the "full
  K8s placement" the user asked for, and it doesn't build the filter/score seam
  everything else needs. **Rejected as the endpoint** (it is a strict subset of
  this design).
- **Full K8s parity in one spec** (adds inter-container affinity, topology spread,
  preemption). A multi-subsystem, multi-month program. **Deferred** — decomposed
  into later phases so Phase 1 ships high-value node-relative placement first.
- **Chosen: node-relative filter→score pipeline.** All Phase-1 primitives are
  node-relative (need only the container spec + each worker's labels/taints), which
  fits the existing pure-scheduler-over-`WorkerView` shape and yields the
  extensible pipeline.

## Design

### 1. Desired/actual split (the enabling refactor)

The core change that lets a user *set* `spec.nodeName` without it meaning "already
bound." Split **desired** (spec) from **actual** (status):

| Concept | Field | Set by |
|---|---|---|
| Desired pin | `spec.nodeName` | user (optional) |
| Bound worker | `status.workerName` | scheduler |
| Bound-worker index column | `node_name` column → mirrors **`status.workerName`** | store, on write |
| "needs scheduling" | `phase == "Pending" && status.workerName` is absent | — |

Concrete changes:

- `server/src/lib.rs::extract_node_name` reads **`status.workerName`** (bound
  worker) for the index column, not `spec.nodeName`. The column's meaning becomes
  "the worker this container is bound to."
- The public field selector veloslet uses is renamed
  `spec.nodeName` → **`status.workerName`** (mapped to the same index column). This
  keeps the selector name honest; velos controls all clients (veloslet), so the
  rename is safe. The old form may be accepted as a deprecated alias for one
  release if desired.
- `controllers.rs::pending_containers` filters on
  `phase == "Pending" && status.workerName absent` — it no longer consults the
  index column, so a user-set `spec.nodeName` no longer excludes a container from
  scheduling.
- The scheduler reads the **desired pin from the document** (`spec.nodeName`), not
  the index column (the controller already loads full documents).

Result: `spec.nodeName` = what the user wants; `status.workerName` / index column =
where it actually landed. This is the K8s spec-vs-status separation and satisfies
Principle #7 (index columns track a well-defined field).

### 2. Wire schema (`crates/models/fluorite/velos.fl`)

New types (snake_case in `.fl`, camelCase on the wire; all names unique
project-wide per fluorite convention):

```
enum NodeSelectorOperator { In, NotIn, Exists, DoesNotExist, Gt, Lt }

struct NodeSelectorRequirement {
    key: String,
    operator: NodeSelectorOperator,
    values: Vec<String>,          // empty for Exists / DoesNotExist
}

struct NodeSelectorTerm {
    match_expressions: Vec<NodeSelectorRequirement>,   // AND within a term
}

struct PreferredSchedulingTerm {
    weight: i32,                  // 1..=100
    preference: NodeSelectorTerm,
}

struct NodeAffinity {
    required: Vec<NodeSelectorTerm>,            // OR across terms; empty = no hard rule
    preferred: Vec<PreferredSchedulingTerm>,    // soft, additive to score
}

enum TaintEffect { NoSchedule, PreferNoSchedule }   // no NoExecute in Phase 1

struct Taint {
    key: String,
    value: String,
    effect: TaintEffect,
}

enum TolerationOperator { Equal, Exists }

struct Toleration {
    key: String,                  // empty + Exists tolerates all taints
    operator: TolerationOperator,
    value: String,
    effect: Option<TaintEffect>,  // None matches any effect
}
```

Additions to existing structs:

- `ContainerSpec` (keep `node_name`): add
  `node_selector: Map<String, String>`,
  `affinity: Option<NodeAffinity>`,
  `tolerations: Vec<Toleration>`.
- `WorkerSpec` (keep `unschedulable`): add `taints: Vec<Taint>`.
  (Worker **labels** already live in `metadata.labels` — no new field.)

Because no Rust logic consumes fluorite types directly (server/veloslet use
`serde_json::Value`), extending the schema cannot break typed constructors. Update
the `Container::pending` / `ObjectMeta` convenience constructors in
`crates/models/src/lib.rs` for the widened `ContainerSpec`.

### 3. Scheduler: first-fit → filter → score → pick (`crates/scheduler`)

The scheduler keeps its own **domain types** (Principle #7 — no fluorite in logic).
Mirror the wire enums/structs as plain scheduler types
(`NodeSelectorOperator`, `NodeSelectorRequirement`, `NodeSelectorTerm`,
`PreferredSchedulingTerm`, `Taint`, `TaintEffect`, `Toleration`,
`TolerationOperator`). `WorkerView` gains:

```rust
pub labels: HashMap<String, String>,
pub taints:  Vec<Taint>,
```

Replace the bare `ResourceRequest` argument with a placement request:

```rust
pub struct PlacementRequest {
    pub resources:     ResourceRequest,
    pub node_name:     Option<WorkerName>,
    pub node_selector: Vec<(String, String)>,     // all must match (AND)
    pub required:      Vec<NodeSelectorTerm>,      // node affinity, OR across terms
    pub preferred:     Vec<PreferredSchedulingTerm>,
    pub tolerations:   Vec<Toleration>,
}
```

New return type carrying a reason on failure:

```rust
pub enum Placement {
    Scheduled(WorkerName),
    Unschedulable { reason: String },
}

pub fn schedule(req: &PlacementRequest, workers: &[WorkerView]) -> Placement;
```

**Filter (hard — every predicate must pass), in order:**

1. Base admittance (unchanged): `ready && !unschedulable && running < max &&
   free_cpu >= req.cpu && free_memory >= req.memory_bytes`.
2. `node_name`: if `Some`, the worker's name must equal it.
3. `node_selector`: every `(k, v)` must equal `worker.labels[k]`.
4. Node affinity `required`: if non-empty, **at least one** `NodeSelectorTerm`
   matches (a term matches iff **all** its `match_expressions` match, evaluated
   against `worker.labels`). Operator semantics:
   - `In` / `NotIn`: key present and value ∈ / ∉ `values`.
   - `Exists` / `DoesNotExist`: key present / absent (`values` empty).
   - `Gt` / `Lt`: `values` has one element; both parse as integers; label > / < it.
5. Taints: for every worker taint with effect `NoSchedule`, some toleration must
   match (`Exists` → key matches, any value; `Equal` → key and value match; effect
   `None` matches any; empty key + `Exists` tolerates everything).

Each rejected worker yields a typed `RejectReason` (NotReady, Unschedulable, Full,
InsufficientCpu, InsufficientMemory, NodeNameMismatch, LabelMismatch,
AffinityMismatch, UntoleratedTaint).

**Score (soft) over surviving candidates:**

- `+ weight` for each `preferred` term that matches the worker.
- `− penalty` for each worker `PreferNoSchedule` taint not tolerated.
- Base score `0`, so the zero-preference case leaves all candidates tied.

**Pick:** highest score wins; **ties broken by input order** — so with no preferred
terms and no PreferNoSchedule taints, the result is the first admitting worker in
order == today's first-fit (Goal 5).

**Unschedulable reason (Principle #6):** when no worker survives, build a K8s-style
message from the tallied `RejectReason`s, e.g.
`0/3 workers available: 1 NotReady, 1 didn't match nodeSelector, 1 insufficient cpu`.
`schedule` stays pure — the reason is a pure function of inputs.

`plan_bindings` keeps its single-pass allocation accounting, now returning, per
pending container, either a `Binding` or the `Unschedulable { reason }`.

### 4. Controller wiring (`crates/server/src/controllers.rs`)

- `container_request` → `container_placement_request`: additionally parse
  `spec.nodeName`, `spec.nodeSelector`, `spec.affinity`, `spec.tolerations` from
  the document into a `PlacementRequest`.
- `build_worker_views`: additionally parse `metadata.labels` and `spec.taints`.
- `pending_containers`: change filter to
  `phase == "Pending" && status.workerName absent` (see §1).
- `reconcile_scheduling`:
  - For a `Binding`: set `status.workerName`, `status.phase = "Scheduled"`, and the
    `node_name` index column (= bound worker). It **no longer** sets `spec.nodeName`
    (that is now user-owned; a bound container may or may not have had a pin).
  - For an `Unschedulable { reason }`: write `status.message = reason` **only when
    it differs** from the current value (avoid per-tick write churn / RV inflation).
    Leave phase `Pending`.

### 5. Actuation (veloslet) & admission

- veloslet is unchanged except for the field-selector rename (§1): it lists
  containers where `status.workerName == <self>`.
- Admission (`fail closed`, Principle #6): reject malformed placement specs
  (unknown operator, `Gt`/`Lt` with non-integer or non-single `values`, weight out
  of `1..=100`, toleration with `Equal` + empty key) at the API boundary with 422,
  rather than letting the scheduler silently skip them.

### 6. Store migration

The `node_name` index column changes meaning (desired → bound) and its populating
expression (`extract_node_name`) changes. Existing rows: on the next write each
object re-derives the column; a one-shot backfill (`UPDATE objects SET node_name =
json_extract(document,'$.status.workerName')`) can be run at startup, guarded so it
runs once. No new column is required. If a separate desired-pin index is ever
needed, add it later; Phase 1 reads the pin from the document.

## Testing

**Scheduler crate — pure unit tests (the bulk of coverage):**

- Each operator: `In`, `NotIn`, `Exists`, `DoesNotExist`, `Gt`, `Lt` (incl.
  non-integer `Gt`/`Lt` → no match).
- Required affinity: OR across terms, AND within a term.
- Preferred affinity: weighted scoring picks the higher-scoring worker; ties fall
  back to input order.
- `nodeName` pin: places on the pin when it fits; `Unschedulable` (not force-placed)
  when the pinned worker is NotReady / full / missing.
- `nodeSelector`: excludes non-matching workers.
- Taint × toleration matrix: `NoSchedule` blocks unless tolerated; `Equal` vs
  `Exists`; empty-key `Exists` tolerates all; `PreferNoSchedule` only penalizes.
- Zero-config equivalence: no placement fields, no taints ⇒ identical to first-fit.
- Unschedulable reason strings contain the right tallies.

**Controller / integration:**

- `pending_containers` schedules a container that has a user-set `spec.nodeName`.
- `reconcile_scheduling` writes `status.workerName` + the bound-worker index column
  and does **not** rewrite `status.message` when the reason is unchanged.

**e2e (`crates/tests`):** a pinned container lands on its pin; a `nodeSelector`
container avoids non-matching workers; a tainted worker is skipped until a
toleration is added; the bound container is actuated by the right veloslet.

## Future phases (own specs)

- **Phase 2 — topology-aware:** inter-container affinity/anti-affinity and topology
  spread. Requires topology keys on workers and evaluating placement against where
  other containers already landed.
- **Phase 3 — priority/preemption:** priority classes and eviction (introduces
  taint effect `NoExecute`).
- **Ergonomics:** velosctl flags and dashboard form fields for the placement
  spec; a `velosctl taint` / `velosctl label` for workers.

## Risks / open questions

- **Field-selector rename** (`spec.nodeName` → `status.workerName`): confirm no
  external consumer depends on the old name (velos controls veloslet; dashboard
  reads containers but does not filter by this selector). Keep a deprecated alias
  for one release if in doubt.
- **`Gt`/`Lt` typing:** labels are strings; comparison is defined only for
  integer-parseable values. Non-integer ⇒ no match (documented), not an error.
- **Scoring model:** Phase 1 keeps scoring intentionally minimal (preferred-affinity
  sum minus PreferNoSchedule penalty). Bin-packing vs. spread as an explicit base
  score is deliberately deferred — input-order tie-break preserves current behavior.
