//! Controller dispatch of model and application runtime actions over exact keyed runtime adapters.
//! Each action starts only while its owner's site lifecycle fencing still admits it, which keeps
//! the runtime layer below it owner-neutral.

pub mod application;
pub mod model;

mod registry;

use std::{future::Future, task::Poll};

use dotcore_domain::deployment::DeploymentOwner;
use dotcore_site::view::fencing::{
    ActionPermitError, LifecycleOperationToken, RuntimeActionAuthority,
};

/// Condition an admitted adapter action must still satisfy at the moment it starts.
#[derive(Clone, Copy)]
pub(crate) enum Admission<'a> {
    /// The captured deployment operation is still current.
    Operation(&'a LifecycleOperationToken),

    /// No accepted deployment of `owner` owns the orphan workload.
    Orphan {
        owner: DeploymentOwner,
        owner_id: &'a str,
    },
}

/// Starts one adapter action only while `admission` holds under `authority`.
///
/// The action's first poll runs inside the authority's commit section, so an accepted-view
/// replacement or lease expiry either precedes the action or follows its start, never splits it.
/// A refused admission becomes the caller's error through `fenced`.
pub(crate) async fn dispatch_admitted<M, F, T, E>(
    authority: &RuntimeActionAuthority,
    admission: Admission<'_>,
    fenced: fn(ActionPermitError) -> E,
    make_future: M,
) -> Result<T, E>
where
    M: FnOnce() -> F,
    F: Future<Output = Result<T, E>>,
{
    let mut make_future = Some(make_future);
    let mut future: Option<std::pin::Pin<Box<F>>> = None;
    std::future::poll_fn(|context| {
        if let Some(future) = future.as_mut() {
            return future.as_mut().poll(context);
        }
        let start = || {
            let mut admitted = Box::pin(make_future
                .take()
                .expect("Runtime admission future is created once")(
            ));
            let result = admitted.as_mut().poll(context);
            future = Some(admitted);
            result
        };
        match admission {
            Admission::Operation(token) => authority.commit_operation(token, start),
            Admission::Orphan { owner, owner_id } => {
                authority.commit_orphan_action(owner, owner_id, start)
            }
        }
        .unwrap_or_else(|error| Poll::Ready(Err(fenced(error))))
    })
    .await
}
