//! The Velos scheduler: a pure decision function that binds an unscheduled
//! container to a worker.
//!
//! Principle #5 (pure core): `schedule` is a total function of its inputs with no
//! I/O. The controller that observes state and writes the binding lives elsewhere
//! (`velos-server`); this crate only decides.
//!
//! Placement is Kubernetes-shaped: **filter** (hard predicates a worker must
//! satisfy) → **score** (soft preferences) → **pick** (highest score, ties broken
//! by input order). With no placement fields set and no worker taints the result
//! equals a plain first-fit over the input order.

use std::collections::HashMap;

/// A worker's name — a semantic type, not a bare `String` (Principle #1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkerName(pub String);

/// A resource ask (or usage). cpu is a whole-core count; memory in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceRequest {
    pub cpu: u32,
    pub memory_bytes: u64,
}

/// Match operator for a node-affinity requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSelectorOperator {
    In,
    NotIn,
    Exists,
    DoesNotExist,
    Gt,
    Lt,
}

/// A single label requirement (`key <op> values`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSelectorRequirement {
    pub key: String,
    pub operator: NodeSelectorOperator,
    pub values: Vec<String>,
}

/// A conjunction of requirements (all must match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSelectorTerm {
    pub match_expressions: Vec<NodeSelectorRequirement>,
}

/// A weighted soft preference used during scoring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferredSchedulingTerm {
    pub weight: i32,
    pub preference: NodeSelectorTerm,
}

/// The effect a taint has on scheduling. `NoExecute` is intentionally absent in
/// Phase 1 (no eviction of running containers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaintEffect {
    NoSchedule,
    PreferNoSchedule,
}

/// A worker taint that repels containers unless tolerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Taint {
    pub key: String,
    pub value: String,
    pub effect: TaintEffect,
}

/// How a toleration matches a taint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TolerationOperator {
    Equal,
    Exists,
}

/// A container's tolerance for a taint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toleration {
    pub key: String,
    pub operator: TolerationOperator,
    pub value: String,
    pub effect: Option<TaintEffect>,
}

/// Everything the scheduler needs to place one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRequest {
    pub resources: ResourceRequest,
    pub node_name: Option<WorkerName>,
    pub node_selector: Vec<(String, String)>,
    pub required: Vec<NodeSelectorTerm>,
    pub preferred: Vec<PreferredSchedulingTerm>,
    pub tolerations: Vec<Toleration>,
}

/// The scheduler's view of one candidate worker: enough to decide fit, nothing
/// about wire or storage shapes.
#[derive(Debug, Clone)]
pub struct WorkerView {
    pub name: WorkerName,
    pub ready: bool,
    pub unschedulable: bool,
    /// Total schedulable resources on the node.
    pub allocatable: ResourceRequest,
    /// Resources already committed to containers bound here.
    pub allocated: ResourceRequest,
    pub labels: HashMap<String, String>,
    pub taints: Vec<Taint>,
}

impl WorkerView {
    /// Free cpu cores after accounting for already-allocated containers.
    fn free_cpu(&self) -> u32 {
        self.allocatable.cpu.saturating_sub(self.allocated.cpu)
    }

    /// Free memory bytes after accounting for already-allocated containers.
    fn free_memory(&self) -> u64 {
        self.allocatable
            .memory_bytes
            .saturating_sub(self.allocated.memory_bytes)
    }
}

/// Why a worker was filtered out — tallied into the unschedulable reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    NotReady,
    Cordoned,
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
            RejectReason::InsufficientCpu => "insufficient cpu",
            RejectReason::InsufficientMemory => "insufficient memory",
            RejectReason::NodeNameMismatch => "didn't match nodeName",
            RejectReason::LabelMismatch => "didn't match nodeSelector",
            RejectReason::AffinityMismatch => "didn't match node affinity",
            RejectReason::UntoleratedTaint => "had an untolerated taint",
        }
    }
}

/// The decision for one container: bound to a worker, or left pending with why.
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
    if w.free_cpu() < req.resources.cpu {
        return Err(RejectReason::InsufficientCpu);
    }
    if w.free_memory() < req.resources.memory_bytes {
        return Err(RejectReason::InsufficientMemory);
    }
    Ok(())
}

/// Hard filter: a `node_name` pin must equal this worker.
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

/// Hard filter: every `node_selector` label must match this worker's labels.
fn matches_node_selector(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    if node_selector_matches(&req.node_selector, &w.labels) {
        Ok(())
    } else {
        Err(RejectReason::LabelMismatch)
    }
}

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
        NodeSelectorOperator::In => labels
            .get(&r.key)
            .map(|v| r.values.contains(v))
            .unwrap_or(false),
        NodeSelectorOperator::NotIn => labels
            .get(&r.key)
            .map(|v| !r.values.contains(v))
            .unwrap_or(true),
        NodeSelectorOperator::Exists => labels.contains_key(&r.key),
        NodeSelectorOperator::DoesNotExist => !labels.contains_key(&r.key),
        NodeSelectorOperator::Gt => cmp_int(labels.get(&r.key), r.values.first(), |a, b| a > b),
        NodeSelectorOperator::Lt => cmp_int(labels.get(&r.key), r.values.first(), |a, b| a < b),
    }
}

fn term_matches(term: &NodeSelectorTerm, labels: &HashMap<String, String>) -> bool {
    term.match_expressions
        .iter()
        .all(|r| requirement_matches(r, labels))
}

fn required_matches(terms: &[NodeSelectorTerm], labels: &HashMap<String, String>) -> bool {
    terms.is_empty() || terms.iter().any(|t| term_matches(t, labels))
}

/// Hard filter: if `required` is non-empty, at least one term must match.
fn matches_required_affinity(req: &PlacementRequest, w: &WorkerView) -> Result<(), RejectReason> {
    if required_matches(&req.required, &w.labels) {
        Ok(())
    } else {
        Err(RejectReason::AffinityMismatch)
    }
}

fn tolerates(tol: &Toleration, taint: &Taint) -> bool {
    if let Some(eff) = tol.effect
        && eff != taint.effect
    {
        return false;
    }
    match tol.operator {
        TolerationOperator::Exists => tol.key.is_empty() || tol.key == taint.key,
        TolerationOperator::Equal => tol.key == taint.key && tol.value == taint.value,
    }
}

/// Hard filter: every `NoSchedule` taint on the worker must be tolerated.
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

/// Each untolerated `PreferNoSchedule` taint subtracts this from a worker's score.
const PREFER_NO_SCHEDULE_PENALTY: i32 = 100;

/// Soft score for an admitted worker: matched `preferred` weights, minus a penalty
/// per untolerated `PreferNoSchedule` taint.
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

/// Total decision for one worker: `Ok(score)` if it admits `req`, else why not.
fn evaluate(req: &PlacementRequest, w: &WorkerView) -> Result<i32, RejectReason> {
    admits_base(req, w)?;
    matches_node_name(req, w)?;
    matches_node_selector(req, w)?;
    matches_required_affinity(req, w)?;
    tolerates_taints(req, w)?;
    Ok(score(req, w))
}

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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_arguments
)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn wv(
        name: &str,
        ready: bool,
        unsched: bool,
        cpu: u32,
        mem: u64,
        used_cpu: u32,
        used_mem: u64,
    ) -> WorkerView {
        WorkerView {
            name: WorkerName(name.into()),
            ready,
            unschedulable: unsched,
            allocatable: ResourceRequest {
                cpu,
                memory_bytes: mem,
            },
            allocated: ResourceRequest {
                cpu: used_cpu,
                memory_bytes: used_mem,
            },
            labels: HashMap::new(),
            taints: Vec::new(),
        }
    }

    fn wv_labeled(name: &str, labels: &[(&str, &str)]) -> WorkerView {
        let mut w = wv(name, true, false, 8, 16 * GB, 0, 0);
        w.labels = labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        w
    }

    fn wv_tainted(name: &str, taints: Vec<Taint>) -> WorkerView {
        let mut w = wv(name, true, false, 8, 16 * GB, 0, 0);
        w.taints = taints;
        w
    }

    fn term(key: &str, op: NodeSelectorOperator, vals: &[&str]) -> NodeSelectorTerm {
        NodeSelectorTerm {
            match_expressions: vec![NodeSelectorRequirement {
                key: key.into(),
                operator: op,
                values: vals.iter().map(|s| s.to_string()).collect(),
            }],
        }
    }

    fn pref(weight: i32, t: NodeSelectorTerm) -> PreferredSchedulingTerm {
        PreferredSchedulingTerm {
            weight,
            preference: t,
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

    #[test]
    fn picks_first_fitting_ready_worker() {
        let workers = vec![
            wv("w1", true, false, 1, 8 * GB, 0, 0),
            wv("w2", true, false, 4, 8 * GB, 0, 0),
        ];
        assert_eq!(
            schedule(&req(2, 2 * GB), &workers),
            Placement::Scheduled(WorkerName("w2".into()))
        );
    }

    #[test]
    fn skips_not_ready_and_unschedulable() {
        let workers = vec![
            wv("w1", false, false, 8, 16 * GB, 0, 0),
            wv("w2", true, true, 8, 16 * GB, 0, 0),
            wv("w3", true, false, 8, 16 * GB, 0, 0),
        ];
        assert_eq!(
            schedule(&req(1, GB), &workers),
            Placement::Scheduled(WorkerName("w3".into()))
        );
    }

    #[test]
    fn none_when_nothing_fits() {
        let workers = vec![wv("w1", true, false, 8, 16 * GB, 0, 0)];
        assert!(matches!(
            schedule(&req(64, 256 * GB), &workers),
            Placement::Unschedulable { .. }
        ));
    }

    #[test]
    fn honors_node_name_pin() {
        let workers = vec![
            wv("w1", true, false, 8, 16 * GB, 0, 0),
            wv("w2", true, false, 8, 16 * GB, 0, 0),
        ];
        let mut r = req(1, GB);
        r.node_name = Some(WorkerName("w2".into()));
        assert_eq!(
            schedule(&r, &workers),
            Placement::Scheduled(WorkerName("w2".into()))
        );
    }

    #[test]
    fn pin_to_full_worker_is_unschedulable_not_forced() {
        // w2 has no free cpu (1 core, 1 used); the pin must not force placement.
        let workers = vec![
            wv("w1", true, false, 8, 16 * GB, 0, 0),
            wv("w2", true, false, 1, GB, 1, GB),
        ];
        let mut r = req(1, GB);
        r.node_name = Some(WorkerName("w2".into()));
        assert!(matches!(
            schedule(&r, &workers),
            Placement::Unschedulable { .. }
        ));
    }

    #[test]
    fn node_selector_excludes_non_matching() {
        let workers = vec![
            wv_labeled("w1", &[("gpu", "false")]),
            wv_labeled("w2", &[("gpu", "true")]),
        ];
        let mut r = req(1, GB);
        r.node_selector = vec![("gpu".into(), "true".into())];
        assert_eq!(
            schedule(&r, &workers),
            Placement::Scheduled(WorkerName("w2".into()))
        );
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
            let got = matches!(
                schedule(&r, std::slice::from_ref(&w)),
                Placement::Scheduled(_)
            );
            assert_eq!(got, *want, "term {t:?}");
        }
    }

    #[test]
    fn required_terms_are_ored_expressions_anded() {
        let w = wv_labeled("w", &[("a", "1"), ("b", "2")]);
        // Two terms OR'd: second matches -> scheduled.
        let mut r = req(1, GB);
        r.required = vec![
            term("a", NodeSelectorOperator::In, &["9"]),
            term("b", NodeSelectorOperator::In, &["2"]),
        ];
        assert!(matches!(
            schedule(&r, std::slice::from_ref(&w)),
            Placement::Scheduled(_)
        ));
        // One term, two expressions AND'd: one fails -> unschedulable.
        r.required = vec![NodeSelectorTerm {
            match_expressions: vec![
                NodeSelectorRequirement {
                    key: "a".into(),
                    operator: NodeSelectorOperator::In,
                    values: vec!["1".into()],
                },
                NodeSelectorRequirement {
                    key: "b".into(),
                    operator: NodeSelectorOperator::In,
                    values: vec!["9".into()],
                },
            ],
        }];
        assert!(matches!(
            schedule(&r, std::slice::from_ref(&w)),
            Placement::Unschedulable { .. }
        ));
    }

    #[test]
    fn no_schedule_taint_blocks_unless_tolerated() {
        let taint = Taint {
            key: "gpu".into(),
            value: "true".into(),
            effect: TaintEffect::NoSchedule,
        };
        let w = wv_tainted("w", vec![taint.clone()]);

        // No toleration -> blocked.
        assert!(matches!(
            schedule(&req(1, GB), std::slice::from_ref(&w)),
            Placement::Unschedulable { .. }
        ));

        // Equal match -> ok.
        let mut r = req(1, GB);
        r.tolerations = vec![Toleration {
            key: "gpu".into(),
            operator: TolerationOperator::Equal,
            value: "true".into(),
            effect: Some(TaintEffect::NoSchedule),
        }];
        assert!(matches!(
            schedule(&r, std::slice::from_ref(&w)),
            Placement::Scheduled(_)
        ));

        // Equal wrong value -> blocked.
        let mut r = req(1, GB);
        r.tolerations = vec![Toleration {
            key: "gpu".into(),
            operator: TolerationOperator::Equal,
            value: "false".into(),
            effect: None,
        }];
        assert!(matches!(
            schedule(&r, std::slice::from_ref(&w)),
            Placement::Unschedulable { .. }
        ));

        // Exists by key -> ok.
        let mut r = req(1, GB);
        r.tolerations = vec![Toleration {
            key: "gpu".into(),
            operator: TolerationOperator::Exists,
            value: String::new(),
            effect: None,
        }];
        assert!(matches!(
            schedule(&r, std::slice::from_ref(&w)),
            Placement::Scheduled(_)
        ));

        // Empty-key Exists tolerates everything -> ok.
        let mut r = req(1, GB);
        r.tolerations = vec![Toleration {
            key: String::new(),
            operator: TolerationOperator::Exists,
            value: String::new(),
            effect: None,
        }];
        assert!(matches!(
            schedule(&r, std::slice::from_ref(&w)),
            Placement::Scheduled(_)
        ));
    }

    #[test]
    fn preferred_affinity_picks_higher_score() {
        let w1 = wv_labeled("w1", &[("fast", "false")]);
        let w2 = wv_labeled("w2", &[("fast", "true")]);
        let mut r = req(1, GB);
        r.preferred = vec![pref(50, term("fast", NodeSelectorOperator::In, &["true"]))];
        // w1 is first in order but w2 scores higher -> w2 wins.
        assert_eq!(
            schedule(&r, &[w1, w2]),
            Placement::Scheduled(WorkerName("w2".into()))
        );
    }

    #[test]
    fn zero_preference_keeps_first_fit_order() {
        let w1 = wv_labeled("w1", &[("fast", "true")]);
        let w2 = wv_labeled("w2", &[("fast", "true")]);
        // No preferred terms -> both score 0 -> first in order wins.
        assert_eq!(
            schedule(&req(1, GB), &[w1, w2]),
            Placement::Scheduled(WorkerName("w1".into()))
        );
    }

    #[test]
    fn prefer_no_schedule_penalizes_but_allows() {
        let clean = wv("w1", true, false, 8, 16 * GB, 0, 0);
        let tainted = wv_tainted(
            "w2",
            vec![Taint {
                key: "spot".into(),
                value: String::new(),
                effect: TaintEffect::PreferNoSchedule,
            }],
        );
        // Put tainted first; the penalty should still make the clean worker win.
        assert_eq!(
            schedule(&req(1, GB), &[tainted, clean]),
            Placement::Scheduled(WorkerName("w1".into()))
        );
    }

    #[test]
    fn unschedulable_reason_tallies_causes() {
        let workers = vec![
            wv("w1", false, false, 8, 16 * GB, 0, 0), // NotReady
            wv_labeled("w2", &[("gpu", "false")]),    // LabelMismatch
            wv("w3", true, false, 1, GB, 0, 0),       // InsufficientCpu
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
}
