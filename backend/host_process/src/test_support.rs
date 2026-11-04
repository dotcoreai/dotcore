//! In-memory host process table for unit tests of process ancestry and GPU attribution.
//!
//! Tests declare processes as PID, parent PID, and start token, then remove or replace entries to
//! simulate a process exiting or its PID being reused between reads.

use std::{collections::BTreeMap, sync::Mutex};

use anyhow::Result;

use crate::{ProcessEntry, ProcessTable};

/// Process table keyed by PID with `(parent PID, start token)` facts.
pub struct FakeProcessTable(Mutex<BTreeMap<u32, ProcessEntry>>);

impl FakeProcessTable {
    pub fn new<const N: usize>(processes: [(u32, u32, u64); N]) -> Self {
        Self(Mutex::new(
            processes
                .into_iter()
                .map(|(process_id, parent_process_id, start_token)| {
                    (process_id, entry(parent_process_id, start_token))
                })
                .collect(),
        ))
    }

    /// Replaces a process with `(parent PID, start token)` facts, or removes it with `None`.
    pub fn set(&self, process_id: u32, facts: Option<(u32, u64)>) {
        let mut processes = self.0.lock().unwrap();
        match facts {
            Some((parent_process_id, start_token)) => {
                processes.insert(process_id, entry(parent_process_id, start_token));
            }
            None => {
                processes.remove(&process_id);
            }
        }
    }
}

impl ProcessTable for FakeProcessTable {
    fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
        Ok(self.0.lock().unwrap().get(&process_id).copied())
    }
}

fn entry(parent_process_id: u32, start_token: u64) -> ProcessEntry {
    ProcessEntry {
        parent_process_id,
        start_token,
    }
}
