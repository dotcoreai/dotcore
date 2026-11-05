//! Moves a backend subprocess's manual clock forward safely, so a stopped worker's contact lease
//! and serving targets expire on the site manager without waiting for them in real time.
//!
//! Every advance first proves that the nodes still running have recently renewed their contact,
//! so no healthy node, including the manager's own, can expire because time jumped, and that
//! every node meant to expire has already fallen silent. A node's status arrives on its control
//! stream and stays current while the node is live, so only contact is checked.

use std::{collections::BTreeMap, time::Duration};

use dotcore::testing::{LIVENESS_TTL, ManualClock, NodeLiveness, NodeLivenessAges};

/// Longest age of a live node's contact at an advance; what is left of its lease after the
/// advance is the real time it has to renew again. A live node sends a message at least every
/// node heartbeat interval (3s), so most of each interval qualifies.
const RENEWAL_AGE: Duration = Duration::from_secs(2);

/// Shortest silence of an expiring node at an advance; nodes still running renew well within it,
/// since it exceeds the node heartbeat interval.
const SILENCE_AGE: Duration = Duration::from_secs(4);

/// How often an advance rechecks whether every live node has renewed.
const RENEWAL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Manual clock of one site manager subprocess and the liveness it tracks on that clock.
pub struct ManagerClock {
    clock: ManualClock,
    liveness: NodeLiveness,
}

impl ManagerClock {
    pub fn new(clock: ManualClock, liveness: NodeLiveness) -> Self {
        Self { clock, liveness }
    }

    /// Advances the manager's clock under [`advance`]'s conditions, reading its current ages.
    pub fn advance(
        &self,
        by: Duration,
        expiring: &[String],
        deadline: Duration,
    ) -> Result<(), String> {
        advance(&self.clock, || self.liveness.ages(), by, expiring, deadline)
    }
}

/// Moves `clock` `by` ahead, once every node except `expiring` has renewed its contact within the
/// last two seconds of the `ages` it reads.
///
/// Every `expiring` node must be known and silent for at least four seconds. `by` must leave a
/// renewing node live, so expiring a silent node takes two advances. Blocks until the conditions
/// hold, failing with the last observed ages after `deadline`.
pub fn advance(
    clock: &ManualClock,
    ages: impl Fn() -> BTreeMap<String, NodeLivenessAges>,
    by: Duration,
    expiring: &[String],
    deadline: Duration,
) -> Result<(), String> {
    if by + RENEWAL_AGE >= LIVENESS_TTL {
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
        match renewal_problem(&observed, expiring, elapsed) {
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
            Some(ages) if ages.contact < SILENCE_AGE => {
                return Some(format!("expiring node {node_id} is not silent yet"));
            }
            Some(_) => {}
        }
    }
    ages.iter()
        .filter(|(node_id, _)| !expiring.contains(node_id))
        .find(|(_, ages)| ages.contact > RENEWAL_AGE || ages.contact > elapsed)
        .map(|(node_id, _)| format!("live node {node_id} has not renewed its contact yet"))
}
