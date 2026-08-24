//! Pure worker reconciliation (Principle #5).
//!
//! `reconcile(desired, observed) -> Vec<Action>` is a total function: it compares
//! the containers the server assigned to this worker against what the runtime
//! actually reports, and returns the actions that converge the two. All matching
//! is keyed by container **uid**, which makes the result idempotent across crashes.

use velos_runtime::{InstanceState, RunSpec};

/// Restart behavior for a container (mirrors `velos::RestartPolicy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

impl RestartPolicy {
    /// Parse the wire string; unknown values fail closed to `Never`.
    pub fn parse(s: &str) -> Self {
        match s {
            "Always" => RestartPolicy::Always,
            "OnFailure" => RestartPolicy::OnFailure,
            _ => RestartPolicy::Never,
        }
    }

    fn should_restart(self, exit_code: i32) -> bool {
        match self {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => exit_code != 0,
            RestartPolicy::Never => false,
        }
    }
}

/// The run state the user asked for (mirrors `velos::DesiredState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Hibernated,
}

impl DesiredState {
    /// Parse the wire string. Unknown values fail closed to `Running`: the
    /// server rejects anything else at admission, so a value we cannot read is
    /// never a licence to shut a user's container down.
    pub fn parse(s: &str) -> Self {
        match s {
            "Hibernated" => DesiredState::Hibernated,
            _ => DesiredState::Running,
        }
    }
}

/// A container the server has assigned to this worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredContainer {
    pub name: String,
    pub uid: String,
    pub image: String,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub restart_policy: RestartPolicy,
    pub desired_state: DesiredState,
    pub phase: String,
    pub marked_for_deletion: bool,
    pub has_finalizer: bool,
}

impl DesiredContainer {
    fn run_spec(&self) -> RunSpec {
        RunSpec {
            uid: self.uid.clone(),
            image: self.image.clone(),
            command: self.command.clone(),
            env: self.env.clone(),
        }
    }
}

/// What the runtime reports for one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedInstance {
    pub uid: String,
    pub state: InstanceState,
}

/// An intended action; the actuator turns these into runtime + server calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Launch the instance, then report `Running`.
    Start { name: String, spec: RunSpec },
    /// Remove the exited instance and launch a fresh one (restart policy).
    Restart {
        name: String,
        uid: String,
        spec: RunSpec,
    },
    /// Instance is running but status is stale → report `Running`.
    ReportRunning { name: String },
    /// Instance exited and won't restart → report `Succeeded`/`Failed`.
    ReportTerminal {
        name: String,
        phase: String,
        exit_code: i32,
    },
    /// Desired state is `Hibernated` and the instance is live → shut it down
    /// (keeping its disk), then report `Hibernated`.
    Hibernate { name: String, uid: String },
    /// Desired state is `Running` over a hibernated instance → boot the same
    /// instance back up and report `Running`.
    Resume { name: String, uid: String },
    /// Nothing is running and the container is meant to stay asleep → report
    /// `Hibernated`.
    ReportHibernated { name: String },
    /// Container is being deleted → stop+remove instance, optionally clear finalizer.
    Cleanup {
        name: String,
        uid: String,
        clear_finalizer: bool,
    },
    /// Container is being deleted, has no instance, but still holds our finalizer.
    ClearFinalizer { name: String },
    /// An instance with no matching assignment → reap the orphan.
    Reap { uid: String },
}

impl Action {
    /// The container this action converges, if it names one. Only [`Reap`] does
    /// not: it acts on an orphaned instance the server has no object for.
    ///
    /// [`Reap`]: Action::Reap
    pub fn container(&self) -> Option<&str> {
        match self {
            Action::Start { name, .. }
            | Action::Restart { name, .. }
            | Action::ReportRunning { name }
            | Action::ReportTerminal { name, .. }
            | Action::Hibernate { name, .. }
            | Action::Resume { name, .. }
            | Action::ReportHibernated { name }
            | Action::Cleanup { name, .. }
            | Action::ClearFinalizer { name } => Some(name),
            Action::Reap { .. } => None,
        }
    }

    /// The `status.reason` to publish when this action fails, or `None` when a
    /// failure is not the container's to carry.
    ///
    /// Only the actions that drive the runtime toward the user's desired state
    /// get one. The `Report*` actions are excluded because their only failure
    /// mode is the control plane being unreachable — the very thing a status
    /// write would need — and the teardown actions because the object they
    /// would annotate is on its way out.
    pub fn failure_reason(&self) -> Option<&'static str> {
        match self {
            Action::Start { .. } => Some("StartFailed"),
            Action::Restart { .. } => Some("RestartFailed"),
            Action::Resume { .. } => Some("ResumeFailed"),
            Action::Hibernate { .. } => Some("HibernateFailed"),
            Action::ReportRunning { .. }
            | Action::ReportTerminal { .. }
            | Action::ReportHibernated { .. }
            | Action::Cleanup { .. }
            | Action::ClearFinalizer { .. }
            | Action::Reap { .. } => None,
        }
    }
}

/// `ContainerPhase::Hibernated` on the wire.
const HIBERNATED: &str = "Hibernated";

fn is_terminal(phase: &str) -> bool {
    matches!(phase, "Succeeded" | "Failed")
}

fn terminal_phase(exit_code: i32) -> &'static str {
    if exit_code == 0 {
        "Succeeded"
    } else {
        "Failed"
    }
}

/// Decide the actions that converge `observed` toward `desired`.
pub fn reconcile(desired: &[DesiredContainer], observed: &[ObservedInstance]) -> Vec<Action> {
    let mut actions = Vec::new();

    for d in desired {
        let obs = observed.iter().find(|o| o.uid == d.uid);

        if d.marked_for_deletion {
            match obs {
                Some(_) => actions.push(Action::Cleanup {
                    name: d.name.clone(),
                    uid: d.uid.clone(),
                    clear_finalizer: d.has_finalizer,
                }),
                None => {
                    if d.has_finalizer {
                        actions.push(Action::ClearFinalizer {
                            name: d.name.clone(),
                        });
                    }
                }
            }
            continue;
        }

        // Deletion aside, the user's desired run state picks the branch. It has
        // to come before the restart policy: a hibernated instance has exited
        // *on purpose*, and an `Always` policy would otherwise read that as a
        // crash and immediately undo the hibernation.
        match d.desired_state {
            DesiredState::Hibernated => {
                // A container that already ran to completion has nothing to
                // suspend; leave its terminal phase alone.
                if is_terminal(&d.phase) {
                    continue;
                }
                match obs.map(|o| &o.state) {
                    Some(InstanceState::Running) => actions.push(Action::Hibernate {
                        name: d.name.clone(),
                        uid: d.uid.clone(),
                    }),
                    // Already stopped, or never launched (created asleep):
                    // nothing to actuate, only a phase to record.
                    Some(InstanceState::Exited { .. }) | None => {
                        if d.phase != HIBERNATED {
                            actions.push(Action::ReportHibernated {
                                name: d.name.clone(),
                            });
                        }
                    }
                }
            }
            DesiredState::Running => match obs.map(|o| &o.state) {
                None => {
                    // Covers waking a container whose instance is gone (the
                    // worker was rebuilt, or an operator pruned it): its disk
                    // state is lost, so a fresh launch is the honest outcome.
                    if !is_terminal(&d.phase) {
                        actions.push(Action::Start {
                            name: d.name.clone(),
                            spec: d.run_spec(),
                        });
                    }
                }
                Some(InstanceState::Running) => {
                    if d.phase != "Running" {
                        actions.push(Action::ReportRunning {
                            name: d.name.clone(),
                        });
                    }
                }
                Some(InstanceState::Exited { exit_code }) => {
                    if d.phase == HIBERNATED {
                        actions.push(Action::Resume {
                            name: d.name.clone(),
                            uid: d.uid.clone(),
                        });
                    } else if d.restart_policy.should_restart(*exit_code) {
                        actions.push(Action::Restart {
                            name: d.name.clone(),
                            uid: d.uid.clone(),
                            spec: d.run_spec(),
                        });
                    } else {
                        let phase = terminal_phase(*exit_code);
                        if d.phase != phase {
                            actions.push(Action::ReportTerminal {
                                name: d.name.clone(),
                                phase: phase.to_string(),
                                exit_code: *exit_code,
                            });
                        }
                    }
                }
            },
        }
    }

    for o in observed {
        if !desired.iter().any(|d| d.uid == o.uid) {
            actions.push(Action::Reap { uid: o.uid.clone() });
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(name: &str, phase: &str, policy: RestartPolicy) -> DesiredContainer {
        DesiredContainer {
            name: name.to_string(),
            uid: format!("uid-{name}"),
            image: "img".to_string(),
            command: vec![],
            env: vec![],
            restart_policy: policy,
            desired_state: DesiredState::Running,
            phase: phase.to_string(),
            marked_for_deletion: false,
            has_finalizer: true,
        }
    }

    fn observed(name: &str, state: InstanceState) -> ObservedInstance {
        ObservedInstance {
            uid: format!("uid-{name}"),
            state,
        }
    }

    #[test]
    fn starts_pending_container_with_no_instance() {
        let d = vec![desired("c1", "Scheduled", RestartPolicy::Never)];
        let actions = reconcile(&d, &[]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::Start { name, .. } if name == "c1"));
    }

    #[test]
    fn reports_running_when_status_is_stale() {
        let d = vec![desired("c1", "Scheduled", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Running)];
        assert_eq!(
            reconcile(&d, &o),
            vec![Action::ReportRunning {
                name: "c1".to_string()
            }]
        );
    }

    #[test]
    fn no_action_when_running_and_reported() {
        let d = vec![desired("c1", "Running", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Running)];
        assert!(reconcile(&d, &o).is_empty());
    }

    #[test]
    fn reports_succeeded_on_clean_exit_with_never_policy() {
        let d = vec![desired("c1", "Running", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert_eq!(
            reconcile(&d, &o),
            vec![Action::ReportTerminal {
                name: "c1".to_string(),
                phase: "Succeeded".to_string(),
                exit_code: 0,
            }]
        );
    }

    #[test]
    fn restarts_on_failure_policy_when_exit_nonzero() {
        let d = vec![desired("c1", "Running", RestartPolicy::OnFailure)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 1 })];
        assert!(matches!(&reconcile(&d, &o)[0], Action::Restart { name, .. } if name == "c1"));
    }

    #[test]
    fn always_policy_restarts_even_on_clean_exit() {
        let d = vec![desired("c1", "Running", RestartPolicy::Always)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert!(matches!(&reconcile(&d, &o)[0], Action::Restart { .. }));
    }

    #[test]
    fn terminal_container_without_instance_is_left_alone() {
        let d = vec![desired("c1", "Succeeded", RestartPolicy::Never)];
        assert!(reconcile(&d, &[]).is_empty());
    }

    #[test]
    fn deletion_with_instance_cleans_up_and_clears_finalizer() {
        let mut d = desired("c1", "Running", RestartPolicy::Never);
        d.marked_for_deletion = true;
        let o = vec![observed("c1", InstanceState::Running)];
        assert_eq!(
            reconcile(&[d], &o),
            vec![Action::Cleanup {
                name: "c1".to_string(),
                uid: "uid-c1".to_string(),
                clear_finalizer: true,
            }]
        );
    }

    #[test]
    fn deletion_without_instance_just_clears_finalizer() {
        let mut d = desired("c1", "Running", RestartPolicy::Never);
        d.marked_for_deletion = true;
        assert_eq!(
            reconcile(&[d], &[]),
            vec![Action::ClearFinalizer {
                name: "c1".to_string()
            }]
        );
    }

    fn hibernating(name: &str, phase: &str, policy: RestartPolicy) -> DesiredContainer {
        let mut d = desired(name, phase, policy);
        d.desired_state = DesiredState::Hibernated;
        d
    }

    #[test]
    fn hibernating_a_running_container_stops_its_instance() {
        let d = vec![hibernating("c1", "Running", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Running)];
        assert_eq!(
            reconcile(&d, &o),
            vec![Action::Hibernate {
                name: "c1".to_string(),
                uid: "uid-c1".to_string(),
            }]
        );
    }

    #[test]
    fn stopped_instance_of_a_hibernating_container_is_reported_hibernated() {
        let d = vec![hibernating("c1", "Running", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert_eq!(
            reconcile(&d, &o),
            vec![Action::ReportHibernated {
                name: "c1".to_string()
            }]
        );
    }

    #[test]
    fn hibernated_container_is_settled_and_needs_no_further_action() {
        let d = vec![hibernating("c1", "Hibernated", RestartPolicy::Always)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert!(reconcile(&d, &o).is_empty());
    }

    #[test]
    fn restart_policy_never_reopens_a_hibernated_instance() {
        // The regression this guards: `Always` sees an exited instance and would
        // restart it, undoing the hibernation on the very next tick.
        let d = vec![hibernating("c1", "Running", RestartPolicy::Always)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        let actions = reconcile(&d, &o);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Restart { .. } | Action::Start { .. })),
            "hibernation must outrank the restart policy, got: {actions:?}"
        );
    }

    #[test]
    fn hibernating_a_container_created_asleep_only_records_the_phase() {
        // Never launched: nothing to stop, so the worker just states the fact.
        let d = vec![hibernating("c1", "Scheduled", RestartPolicy::Never)];
        assert_eq!(
            reconcile(&d, &[]),
            vec![Action::ReportHibernated {
                name: "c1".to_string()
            }]
        );
    }

    #[test]
    fn a_finished_container_is_not_dragged_out_of_its_terminal_phase() {
        let d = vec![hibernating("c1", "Succeeded", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert!(reconcile(&d, &o).is_empty());
    }

    #[test]
    fn waking_a_hibernated_container_resumes_its_instance() {
        // `Never` policy: without the Hibernated branch this exited instance
        // would be reported Succeeded instead of woken.
        let d = vec![desired("c1", "Hibernated", RestartPolicy::Never)];
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert_eq!(
            reconcile(&d, &o),
            vec![Action::Resume {
                name: "c1".to_string(),
                uid: "uid-c1".to_string(),
            }]
        );
    }

    #[test]
    fn waking_a_hibernated_container_whose_instance_is_gone_starts_a_fresh_one() {
        let d = vec![desired("c1", "Hibernated", RestartPolicy::Never)];
        assert_eq!(
            reconcile(&d, &[]),
            vec![Action::Start {
                name: "c1".to_string(),
                spec: d[0].run_spec(),
            }]
        );
    }

    #[test]
    fn deletion_outranks_hibernation() {
        let mut d = hibernating("c1", "Hibernated", RestartPolicy::Never);
        d.marked_for_deletion = true;
        let o = vec![observed("c1", InstanceState::Exited { exit_code: 0 })];
        assert_eq!(
            reconcile(&[d], &o),
            vec![Action::Cleanup {
                name: "c1".to_string(),
                uid: "uid-c1".to_string(),
                clear_finalizer: true,
            }]
        );
    }

    #[test]
    fn unknown_desired_state_falls_back_to_running() {
        assert_eq!(DesiredState::parse("Hibernated"), DesiredState::Hibernated);
        assert_eq!(DesiredState::parse("Running"), DesiredState::Running);
        assert_eq!(DesiredState::parse("hibernated"), DesiredState::Running);
        assert_eq!(DesiredState::parse(""), DesiredState::Running);
    }

    #[test]
    fn orphan_instance_is_reaped() {
        let o = vec![observed("ghost", InstanceState::Running)];
        assert_eq!(
            reconcile(&[], &o),
            vec![Action::Reap {
                uid: "uid-ghost".to_string()
            }]
        );
    }
}
