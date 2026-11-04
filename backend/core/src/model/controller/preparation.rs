//! Node-local model artifact-preparation cancellation and result fencing.
//!
//! Each deployment's preparation runs under a gate the realizer keeps in step with its input:
//! intent that no longer wants the deployment present cancels the active attempt and fences its
//! result, and intent that wants it present again admits a fresh attempt.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use dotcore_proto::dotcore::model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use super::{ModelLifecycleError, state::desired_state};

#[derive(Clone)]
pub(super) struct PreparationCoordinator {
    entries: Arc<Mutex<BTreeMap<String, Arc<PreparationEntry>>>>,
    next_incarnation: Arc<AtomicU64>,
}

impl PreparationCoordinator {
    pub(super) fn from_deployments(
        deployments: &[ModelDeploymentDeclaration],
    ) -> Result<Self, ModelLifecycleError> {
        let coordinator = Self {
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            next_incarnation: Arc::new(AtomicU64::new(1)),
        };
        for deployment in deployments {
            let gate = match desired_state(deployment)? {
                ModelDeploymentDesiredState::Present => PreparationGate::Open,
                ModelDeploymentDesiredState::Paused | ModelDeploymentDesiredState::Teardown => {
                    PreparationGate::Closed
                }
                ModelDeploymentDesiredState::Unspecified => unreachable!("desired state validated"),
            };
            coordinator.insert(deployment.model_deployment_id.clone(), gate);
        }
        Ok(coordinator)
    }

    pub(super) fn entry(&self, deployment_id: &str) -> Arc<PreparationEntry> {
        self.entry_or_insert(deployment_id).0
    }

    pub(super) fn entry_or_insert(&self, deployment_id: &str) -> (Arc<PreparationEntry>, bool) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if let Some(entry) = entries.get(deployment_id) {
            return (Arc::clone(entry), false);
        }
        let entry = Arc::new(PreparationEntry::new(
            self.next_incarnation.fetch_add(1, Ordering::Relaxed),
            PreparationGate::Open,
        ));
        entries.insert(deployment_id.to_owned(), Arc::clone(&entry));
        (entry, true)
    }

    pub(super) fn is_current(&self, deployment_id: &str, incarnation: u64) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .get(deployment_id)
            .is_some_and(|entry| entry.incarnation == incarnation)
    }

    pub(super) fn retire(&self, deployment_id: &str, incarnation: u64) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|value| value.into_inner());
        if entries
            .get(deployment_id)
            .is_some_and(|entry| entry.incarnation == incarnation && entry.is_quiescent())
            && let Some(entry) = entries.remove(deployment_id)
        {
            entry.retire();
        }
    }

    fn insert(&self, deployment_id: String, gate: PreparationGate) {
        let incarnation = self.next_incarnation.fetch_add(1, Ordering::Relaxed);
        self.entries
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .insert(
                deployment_id,
                Arc::new(PreparationEntry::new(incarnation, gate)),
            );
    }
}

pub(super) struct PreparationEntry {
    incarnation: u64,
    transition: Arc<AsyncMutex<()>>,
    state: Mutex<EntryState>,
}

impl PreparationEntry {
    fn new(incarnation: u64, gate: PreparationGate) -> Self {
        Self {
            incarnation,
            transition: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(EntryState {
                generation: 0,
                gate,
                cancellation: CancellationToken::new(),
                active: None,
                prepared_generation: None,
            }),
        }
    }

    pub(super) async fn transition(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.transition).lock_owned().await
    }

    pub(super) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub(super) fn register(
        self: &Arc<Self>,
        declaration: ModelDeploymentDeclaration,
    ) -> Option<PreparationLease> {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        if state.gate != PreparationGate::Open || state.active.is_some() {
            return None;
        }
        let completion = Arc::new(PreparationCompletion);
        state.active = Some(Arc::clone(&completion));
        Some(PreparationLease {
            entry: Arc::clone(self),
            incarnation: self.incarnation,
            generation: state.generation,
            declaration,
            cancellation: state.cancellation.clone(),
            completion,
        })
    }

    /// Closes preparation for intent that no longer wants the deployment present: the active
    /// attempt is cancelled and its result is never accepted.
    pub(super) fn withdraw(&self) {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        if state.gate != PreparationGate::Open {
            return;
        }
        state.generation = state
            .generation
            .checked_add(1)
            .expect("preparation generation overflow");
        state.gate = PreparationGate::Closed;
        state.cancellation.cancel();
    }

    /// Admits a fresh preparation once intent wants the deployment present again.
    pub(super) fn reopen(&self) {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        if state.gate != PreparationGate::Closed {
            return;
        }
        state.generation = state
            .generation
            .checked_add(1)
            .expect("preparation generation overflow");
        state.gate = PreparationGate::Open;
        state.cancellation = CancellationToken::new();
    }

    pub(super) fn accepts(&self, ticket: &PreparationTicket) -> bool {
        let state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        self.incarnation == ticket.incarnation
            && state.generation == ticket.generation
            && state.gate == PreparationGate::Open
    }

    pub(super) fn previously_prepared(&self, ticket: &PreparationTicket) -> bool {
        let state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        self.incarnation == ticket.incarnation
            && state.generation == ticket.generation
            && state.gate == PreparationGate::Open
            && state.prepared_generation == Some(ticket.generation)
    }

    pub(super) fn mark_prepared(&self, ticket: &PreparationTicket) {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        if self.incarnation == ticket.incarnation
            && state.generation == ticket.generation
            && state.gate == PreparationGate::Open
        {
            state.prepared_generation = Some(ticket.generation);
        }
    }

    pub(super) fn cancel_active(&self) {
        self.state
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .cancellation
            .cancel();
    }

    pub(super) fn refresh_after_drain(&self) {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        if state.gate == PreparationGate::Open
            && state.active.is_none()
            && state.cancellation.is_cancelled()
        {
            state.cancellation = CancellationToken::new();
        }
    }

    fn complete(&self, completion: &Arc<PreparationCompletion>) {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        if state
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, completion))
        {
            state.active = None;
        }
    }

    fn is_quiescent(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|value| value.into_inner())
            .active
            .is_none()
    }

    fn retire(&self) {
        let mut state = self.state.lock().unwrap_or_else(|value| value.into_inner());
        state.gate = PreparationGate::Retired;
        state.cancellation.cancel();
    }
}

pub(super) struct PreparationLease {
    entry: Arc<PreparationEntry>,
    pub(super) incarnation: u64,
    pub(super) generation: u64,
    pub(super) declaration: ModelDeploymentDeclaration,
    cancellation: CancellationToken,
    completion: Arc<PreparationCompletion>,
}

impl PreparationLease {
    pub(super) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(super) fn ticket(&self) -> PreparationTicket {
        PreparationTicket {
            entry: Arc::clone(&self.entry),
            incarnation: self.incarnation,
            generation: self.generation,
            declaration: self.declaration.clone(),
        }
    }
}

impl Drop for PreparationLease {
    fn drop(&mut self) {
        self.entry.complete(&self.completion);
    }
}

#[derive(Clone)]
pub(super) struct PreparationTicket {
    pub(super) entry: Arc<PreparationEntry>,
    pub(super) incarnation: u64,
    pub(super) generation: u64,
    pub(super) declaration: ModelDeploymentDeclaration,
}

/// Identity of one registered attempt, cleared from its entry when the attempt's lease drops.
struct PreparationCompletion;

struct EntryState {
    generation: u64,
    gate: PreparationGate,
    cancellation: CancellationToken,
    active: Option<Arc<PreparationCompletion>>,
    prepared_generation: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreparationGate {
    Open,

    Closed,

    Retired,
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState;

    use super::PreparationCoordinator;
    use crate::model::controller::test_support::test_desired_deployment;

    #[test]
    fn prepared_generation_mutes_only_repeated_work_for_the_same_declaration() {
        let coordinator = PreparationCoordinator::from_deployments(&[]).expect("coordinator");
        let entry = coordinator.entry("deployment");
        let lease = entry
            .register(present_declaration())
            .expect("first preparation");
        let ticket = lease.ticket();
        assert!(!entry.previously_prepared(&ticket));
        entry.mark_prepared(&ticket);
        assert!(entry.previously_prepared(&ticket));

        drop(lease);
        entry.withdraw();
        entry.reopen();
        let next = entry
            .register(present_declaration())
            .expect("new generation");
        assert!(!entry.previously_prepared(&next.ticket()));
        assert!(!entry.previously_prepared(&ticket));
    }

    #[test]
    fn withdrawing_cancels_the_active_attempt_and_fences_its_result() {
        let coordinator = PreparationCoordinator::from_deployments(&[]).expect("coordinator");
        let entry = coordinator.entry("deployment");
        let lease = entry
            .register(present_declaration())
            .expect("registered preparation");
        let cancellation = lease.cancellation();
        let ticket = lease.ticket();

        entry.withdraw();

        assert!(cancellation.is_cancelled());
        assert!(!entry.accepts(&ticket));
        drop(lease);
        assert!(entry.register(present_declaration()).is_none());
    }

    #[test]
    fn withdraw_and_reopen_rejects_the_stale_generation_result() {
        let coordinator = PreparationCoordinator::from_deployments(&[]).expect("coordinator");
        let entry = coordinator.entry("deployment");
        let old = entry.register(present_declaration()).expect("old lease");
        let old_ticket = old.ticket();
        drop(old);

        entry.withdraw();
        entry.reopen();

        assert!(!entry.accepts(&old_ticket));
        let fresh = entry.register(present_declaration()).expect("fresh lease");
        assert!(entry.accepts(&fresh.ticket()));
        assert!(!fresh.cancellation().is_cancelled());
    }

    #[test]
    fn reopening_an_open_gate_keeps_the_active_attempt() {
        let coordinator = PreparationCoordinator::from_deployments(&[]).expect("coordinator");
        let entry = coordinator.entry("deployment");
        let lease = entry.register(present_declaration()).expect("active lease");

        entry.reopen();

        assert!(entry.accepts(&lease.ticket()));
        assert!(!lease.cancellation().is_cancelled());
    }

    #[tokio::test]
    async fn teardown_retirement_gives_reused_textual_id_a_new_incarnation() {
        let coordinator = PreparationCoordinator::from_deployments(&[]).expect("coordinator");
        let old = coordinator.entry("deployment");
        let old_incarnation = old.incarnation();
        coordinator.retire("deployment", old_incarnation);
        let fresh = coordinator.entry("deployment");

        assert_ne!(fresh.incarnation(), old_incarnation);
        assert!(!coordinator.is_current("deployment", old_incarnation));
    }

    fn present_declaration() -> dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
        test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present)
    }
}
