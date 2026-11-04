//! Tells the deployment-log follows open on a node when the workload of a deployment they follow
//! changes, so they can follow its new output without being reopened.
//!
//! A node's realizer publishes when a workload source of a deployment it hosts is created,
//! replaced or started again, and when the deployment is removed from the node. A follow
//! subscribes before it reads the deployment's current state, so no change between the two is
//! lost. Each deployment has its own channel, so a busy deployment never makes the follows of
//! another miss changes. Changes are not retained: one published while no follow listens reaches
//! nobody, and a follow that falls too far behind is told it missed some.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError, Weak},
};

use tokio::sync::broadcast;

/// Changes buffered per subscribed follow of one deployment before a slow one misses some.
const CHANGE_BUFFER: usize = 64;

/// Node-local broadcast of the workload changes of one owner kind's deployments, such as model
/// deployments. Clones share the same subscribers.
///
/// It also remembers which workload sources were announced since they last changed, so a
/// realizer that sees a workload running can announce it once when the action that created or
/// started it failed after acting.
#[derive(Clone)]
pub(crate) struct WorkloadLifecycle {
    state: Arc<Mutex<LifecycleState>>,
}

impl WorkloadLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::default(),
        }
    }

    /// Announces that a workload source was created, replaced or started again.
    pub(crate) fn started(&self, deployment_id: &str, source_name: &str) {
        self.lock()
            .announced
            .insert((deployment_id.to_owned(), source_name.to_owned()));
        self.publish(
            deployment_id,
            WorkloadChange::Started {
                source_name: source_name.to_owned(),
            },
        );
    }

    /// Announces that a workload source is running, unless it was announced since it last
    /// changed.
    pub(crate) fn running(&self, deployment_id: &str, source_name: &str) {
        let first = self
            .lock()
            .announced
            .insert((deployment_id.to_owned(), source_name.to_owned()));
        if first {
            self.publish(
                deployment_id,
                WorkloadChange::Started {
                    source_name: source_name.to_owned(),
                },
            );
        }
    }

    /// Forgets that a workload source was announced, before an action that may create, replace
    /// or start it, so it is announced again once it runs even when that action fails.
    pub(crate) fn forget(&self, deployment_id: &str, source_name: &str) {
        self.lock()
            .announced
            .remove(&(deployment_id.to_owned(), source_name.to_owned()));
    }

    /// Announces that the deployment's workloads were removed from the node, which ends its
    /// follows.
    pub(crate) fn removed(&self, deployment_id: &str) {
        self.lock()
            .announced
            .retain(|(announced, _)| announced != deployment_id);
        self.publish(deployment_id, WorkloadChange::Removed);
    }

    /// Subscribes to the changes of one deployment published from now on.
    pub(crate) fn subscribe(&self, deployment_id: &str) -> WorkloadChanges {
        let changes = self
            .lock()
            .channels
            .entry(deployment_id.to_owned())
            .or_insert_with(|| broadcast::channel(CHANGE_BUFFER).0)
            .subscribe();
        WorkloadChanges {
            deployment_id: deployment_id.to_owned(),
            changes,
            state: Arc::downgrade(&self.state),
        }
    }

    fn publish(&self, deployment_id: &str, change: WorkloadChange) {
        if let Some(channel) = self.lock().channels.get(deployment_id) {
            // A follow that stopped in between has no one to reach, which is not a failure.
            let _ = channel.send(change);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LifecycleState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One change of a deployment's workload on its hosting node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkloadChange {
    /// A workload source was created, replaced or started again, so its output may continue in
    /// a new stream.
    Started { source_name: String },

    /// The deployment's workloads were removed from the node, which ends its follows.
    Removed,
}

/// The changes of one deployment's workload published since the subscription, in order.
pub(crate) struct WorkloadChanges {
    deployment_id: String,
    changes: broadcast::Receiver<WorkloadChange>,
    state: Weak<Mutex<LifecycleState>>,
}

impl WorkloadChanges {
    /// Waits for the deployment's next change; `None` once nothing can publish anymore.
    pub(crate) async fn next(&mut self) -> Option<Result<WorkloadChange, MissedWorkloadChanges>> {
        match self.changes.recv().await {
            Ok(change) => Some(Ok(change)),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(Err(MissedWorkloadChanges)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

impl Drop for WorkloadChanges {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        // This subscription still counts, so a count of one means it is the last.
        if state
            .channels
            .get(&self.deployment_id)
            .is_some_and(|channel| channel.receiver_count() <= 1)
        {
            state.channels.remove(&self.deployment_id);
        }
    }
}

/// A subscriber fell behind and missed some workload changes, so it no longer knows its
/// deployment's current workload.
#[derive(Debug, thiserror::Error)]
#[error("workload changes were missed by a slow subscriber")]
pub(crate) struct MissedWorkloadChanges;

/// The channels of the deployments some follow subscribes to, and the workload sources announced
/// since they last changed.
#[derive(Default)]
struct LifecycleState {
    channels: BTreeMap<String, broadcast::Sender<WorkloadChange>>,
    announced: BTreeSet<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_subscription_receives_its_deployments_changes_in_order_until_it_falls_behind() {
        let lifecycle = WorkloadLifecycle::new();
        let earlier = lifecycle.subscribe("deployment-a");
        lifecycle.removed("deployment-a");
        let mut changes = lifecycle.subscribe("deployment-a");
        let mut behind = lifecycle.subscribe("deployment-a");
        let mut other = lifecycle.subscribe("deployment-b");
        drop(earlier);

        lifecycle.started("deployment-b", "model");
        lifecycle.started("deployment-a", "model");
        lifecycle.removed("deployment-a");
        let first = changes.next().await;
        let second = changes.next().await;
        // A busy deployment never makes the follows of another fall behind.
        for _ in 0..CHANGE_BUFFER {
            lifecycle.started("deployment-b", "model");
        }
        lifecycle.started("deployment-a", "model");
        let third = changes.next().await;
        for _ in 0..CHANGE_BUFFER {
            lifecycle.started("deployment-a", "model");
        }

        assert_eq!(
            first.map(Result::ok),
            Some(Some(started("model"))),
            "a change published before subscribing, or for another deployment, never arrives"
        );
        assert_eq!(second.map(Result::ok), Some(Some(WorkloadChange::Removed)));
        assert_eq!(third.map(Result::ok), Some(Some(started("model"))));
        assert!(matches!(
            behind.next().await,
            Some(Err(MissedWorkloadChanges))
        ));
        assert!(matches!(
            other.next().await,
            Some(Err(MissedWorkloadChanges))
        ));
    }

    #[test]
    fn a_deployment_channel_lives_only_while_a_follow_subscribes() {
        let lifecycle = WorkloadLifecycle::new();
        let first = lifecycle.subscribe("deployment-a");
        let second = lifecycle.subscribe("deployment-a");
        let other = lifecycle.subscribe("deployment-b");

        drop(first);
        let after_one = channel_names(&lifecycle);
        drop(second);
        drop(other);

        assert_eq!(after_one, ["deployment-a", "deployment-b"]);
        assert!(channel_names(&lifecycle).is_empty());
    }

    #[tokio::test]
    async fn a_running_source_is_announced_once_until_it_changes_or_is_removed() {
        let lifecycle = WorkloadLifecycle::new();
        let mut changes = lifecycle.subscribe("deployment-a");

        lifecycle.started("deployment-a", "web");
        lifecycle.running("deployment-a", "web");
        lifecycle.forget("deployment-a", "web");
        lifecycle.running("deployment-a", "web");
        lifecycle.running("deployment-a", "web");
        lifecycle.running("deployment-a", "cache");
        lifecycle.removed("deployment-a");
        lifecycle.running("deployment-a", "web");
        drop(lifecycle);

        let mut received = Vec::new();
        while let Some(change) = changes.next().await {
            received.push(change.expect("no missed change"));
        }
        assert_eq!(
            received,
            [
                started("web"),
                started("web"),
                started("cache"),
                WorkloadChange::Removed,
                started("web"),
            ]
        );
    }

    fn started(source_name: &str) -> WorkloadChange {
        WorkloadChange::Started {
            source_name: source_name.to_owned(),
        }
    }

    fn channel_names(lifecycle: &WorkloadLifecycle) -> Vec<String> {
        lifecycle.lock().channels.keys().cloned().collect()
    }
}
