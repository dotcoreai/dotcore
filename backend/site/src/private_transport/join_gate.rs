//! Whether a node's private listener currently serves join requests from machines that hold no
//! site certificate yet.
//!
//! The component hosting joins opens the gate for as long as any join is active; the listener
//! checks it on every certificate-less request, so it refuses them in the same way as any other
//! unauthenticated request while no join is active.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Shared switch telling the private listener whether a join is active. Clones share the gate;
/// it starts closed.
#[derive(Clone, Debug, Default)]
pub struct JoinGate {
    active: Arc<AtomicUsize>,
}

impl JoinGate {
    /// Opens the gate until the returned guard drops. The gate stays open while any guard lives,
    /// so each active join holds its own.
    pub fn open(&self) -> OpenJoinGate {
        self.active.fetch_add(1, Ordering::SeqCst);
        OpenJoinGate {
            active: Arc::clone(&self.active),
        }
    }

    /// Reports whether any join is active.
    pub fn is_open(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0
    }
}

/// Keeps a [`JoinGate`] open while it lives.
#[derive(Debug)]
pub struct OpenJoinGate {
    active: Arc<AtomicUsize>,
}

impl Drop for OpenJoinGate {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}
