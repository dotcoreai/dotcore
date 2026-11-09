//! Moves a backend subprocess's manual clock forward safely, so a stopped worker's contact lease
//! and serving targets expire on the site manager without waiting for them in real time.
//!
//! Every advance first proves that the nodes still running have recently renewed their contact,
//! so no healthy node, including the manager's own, can expire because time jumped, and that
//! every node meant to expire has already fallen silent. A node's status arrives on its control
//! stream and stays current while the node is live, so only contact is checked.

use std::{collections::BTreeMap, time::Duration};

use dotcore::testing::{ManualClock, NodeLiveness, NodeLivenessAges, Timing};

/// How often an advance rechecks whether every live node has renewed.
const RENEWAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Manual clock of one site manager subprocess and the liveness it tracks on that clock.
pub struct ManagerClock {
    clock: ManualClock,
    liveness: NodeLiveness,
    timing: Timing,
}

impl ManagerClock {
    /// The clock of a manager whose nodes run on `timing`.
    pub fn new(clock: ManualClock, liveness: NodeLiveness, timing: Timing) -> Self {
        Self {
            clock,
            liveness,
            timing,
        }
    }

    /// Advances the manager's clock under [`advance`]'s conditions, reading its current ages.
    pub fn advance(
        &self,
        by: Duration,
        expiring: &[String],
        deadline: Duration,
    ) -> Result<(), String> {
        advance(
            &self.clock,
            &self.timing,
            || self.liveness.ages(),
            by,
            expiring,
            deadline,
        )
    }
}

/// Moves `clock` `by` ahead, once every node except `expiring` has renewed its contact within its
/// [`renewal_age`] of the `ages` it reads, under the nodes' `timing`.
///
/// Every `expiring` node must be known and silent for at least its [`silence_age`]. `by` must
/// leave a renewing node live, so expiring a silent node takes two advances. Blocks until the
/// conditions hold, failing with the last observed ages after `deadline`.
pub fn advance(
    clock: &ManualClock,
    timing: &Timing,
    ages: impl Fn() -> BTreeMap<String, NodeLivenessAges>,
    by: Duration,
    expiring: &[String],
    deadline: Duration,
) -> Result<(), String> {
    if by + renewal_age(timing) >= timing.node_control.node_silence {
        return Err(format!(
            "advancing the manager clock by {by:?} could expire a node that keeps renewing"
        ));
    }
    // Ages since this call began are measured on the same clock as the observed ages.
    let started = clock.offset();
    let real_started = std::time::Instant::now();
    loop {
        let observed = ages();
        let elapsed = real_started.elapsed() + (clock.offset() - started);
        match renewal_problem(timing, &observed, expiring, elapsed) {
            None => {
                clock.advance(by);
                return Ok(());
            }
            Some(problem) if real_started.elapsed() >= deadline => {
                return Err(format!("{problem}; node liveness ages {observed:?}"));
            }
            Some(_) => std::thread::sleep(RENEWAL_POLL_INTERVAL),
        }
    }
}

/// Returns why the observed `ages` do not allow an advance yet, `elapsed` after it was requested.
pub fn renewal_problem(
    timing: &Timing,
    ages: &BTreeMap<String, NodeLivenessAges>,
    expiring: &[String],
    elapsed: Duration,
) -> Option<String> {
    for node_id in expiring {
        match ages.get(node_id) {
            None => {
                return Some(format!(
                    "expiring node {node_id} has no current incarnation"
                ));
            }
            Some(ages) if ages.contact < silence_age(timing) => {
                return Some(format!("expiring node {node_id} is not silent yet"));
            }
            Some(_) => {}
        }
    }
    ages.iter()
        .filter(|(node_id, _)| !expiring.contains(node_id))
        .find(|(_, ages)| ages.contact > renewal_age(timing) || ages.contact > elapsed)
        .map(|(node_id, _)| format!("live node {node_id} has not renewed its contact yet"))
}

/// Longest age of a live node's contact at an advance; what is left of its lease after the
/// advance is the real time it has to renew again. A live node sends a message at least every
/// node heartbeat interval, so most of each interval qualifies.
pub fn renewal_age(timing: &Timing) -> Duration {
    timing.node_control.heartbeat_interval * 2 / 3
}

/// Shortest silence of an expiring node at an advance; nodes still running renew well within it,
/// since it exceeds the node heartbeat interval.
pub fn silence_age(timing: &Timing) -> Duration {
    timing.node_control.heartbeat_interval * 4 / 3
}
