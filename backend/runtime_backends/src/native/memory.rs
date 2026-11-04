//! Sums the memory a native workload's processes use: the held engine process and every process
//! it started, directly or not, but never the launcher holding the engine.
//!
//! Each process is re-checked against its start token after its memory and children are read,
//! so a process that exited meanwhile, or a PID another process reused, is never counted. Any read
//! failure makes the whole figure unavailable rather than reporting a partial sum.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use dotcore_host_process::{self as process, HostProcessTable, ProcessEntry, ProcessTable};
use dotcore_runtime::ProcessMemoryUnavailable;

/// Raw facts about host processes that the memory walk reads.
pub(super) trait ProcessMemoryFacts {
    /// Returns the process's parent and start token, or `None` when it is not running.
    fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>>;

    /// Returns the memory the process uses in bytes, or `None` when it is not running.
    fn memory_bytes(&self, process_id: u32) -> Result<Option<u64>>;

    /// Returns the PIDs of the process's direct children.
    fn children(&self, process_id: u32) -> Result<Vec<u32>>;
}

/// Facts of the processes on this host.
pub(super) struct HostProcessMemoryFacts;

impl ProcessMemoryFacts for HostProcessMemoryFacts {
    fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
        HostProcessTable.entry(process_id)
    }

    fn memory_bytes(&self, process_id: u32) -> Result<Option<u64>> {
        process::memory_bytes(process_id)
    }

    fn children(&self, process_id: u32) -> Result<Vec<u32>> {
        process::child_process_ids(process_id)
    }
}

/// Sums the memory of the engine process `(process_id, start_token)` and all its descendants.
pub(super) fn process_tree_memory(
    facts: &dyn ProcessMemoryFacts,
    process_id: u32,
    start_token: u64,
) -> Result<u64, ProcessMemoryUnavailable> {
    let mut total = 0_u64;
    let mut visited = BTreeSet::new();
    let mut pending = vec![(process_id, start_token)];
    while let Some((current, token)) = pending.pop() {
        let is_engine = current == process_id;
        if !visited.insert(current) {
            continue;
        }
        let memory = facts.memory_bytes(current).map_err(unreadable)?;
        let children = facts.children(current).map_err(unreadable)?;
        // Checked after the reads: facts of a process that exited or whose PID was reused in
        // between belong to no process of this workload.
        let still_running = facts
            .entry(current)
            .map_err(unreadable)?
            .is_some_and(|entry| entry.start_token == token);
        let Some(bytes) = memory.filter(|_| still_running) else {
            if is_engine {
                return Err(ProcessMemoryUnavailable::NotRunning);
            }
            continue;
        };
        total = total
            .checked_add(bytes)
            .context("workload process memory overflows")
            .map_err(ProcessMemoryUnavailable::Read)?;
        for child in children {
            // A listed child counts only while it still names this process as its parent, so a
            // child PID reused by an unrelated process is skipped.
            if let Some(entry) = facts
                .entry(child)
                .map_err(unreadable)?
                .filter(|entry| entry.parent_process_id == current)
            {
                pending.push((child, entry.start_token));
            }
        }
    }
    Ok(total)
}

fn unreadable(error: anyhow::Error) -> ProcessMemoryUnavailable {
    ProcessMemoryUnavailable::Read(error.context("read workload process facts"))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Mutex};

    use super::*;

    #[test]
    fn the_engine_and_all_descendants_are_summed_but_not_its_launcher() {
        let facts = FakeFacts::new([
            (1, Process::new(0, 10, 1_000_000).children([2])),
            (2, Process::new(1, 20, 100).children([3])),
            (3, Process::new(2, 30, 10).children([4])),
            (4, Process::new(3, 40, 1)),
        ]);

        assert_eq!(process_tree_memory(&facts, 2, 20).unwrap(), 111);
    }

    #[test]
    fn exited_children_and_reused_child_pids_are_not_counted() {
        let facts = FakeFacts::new([
            (2, Process::new(1, 20, 100).children([3, 4, 5])),
            // Listed, then gone before its facts were read.
            (4, Process::new(2, 40, 50).gone()),
            // Its PID now belongs to a process with another parent.
            (5, Process::new(9, 50, 70)),
            (3, Process::new(2, 30, 10)),
        ]);
        // Reused between the memory read and the start-token re-check.
        facts.reuse_after_memory_read(3, 31);

        assert_eq!(process_tree_memory(&facts, 2, 20).unwrap(), 100);
    }

    #[test]
    fn an_engine_that_is_gone_or_reused_is_not_running() {
        let gone = FakeFacts::new([(2, Process::new(1, 20, 100).gone())]);
        let reused = FakeFacts::new([(2, Process::new(1, 21, 100))]);

        for facts in [gone, reused] {
            assert!(matches!(
                process_tree_memory(&facts, 2, 20),
                Err(ProcessMemoryUnavailable::NotRunning)
            ));
        }
    }

    #[test]
    fn an_unreadable_process_makes_the_whole_figure_unavailable() {
        let facts = FakeFacts::new([
            (2, Process::new(1, 20, 100).children([3])),
            (3, Process::new(2, 30, 10).unreadable()),
        ]);

        assert!(matches!(
            process_tree_memory(&facts, 2, 20),
            Err(ProcessMemoryUnavailable::Read(_))
        ));
    }

    /// One fake process: parent, start token, memory, children, and how reads behave.
    #[derive(Clone)]
    struct Process {
        parent: u32,
        token: u64,
        bytes: u64,
        children: Vec<u32>,
        gone: bool,
        unreadable: bool,
    }

    impl Process {
        fn new(parent: u32, token: u64, bytes: u64) -> Self {
            Self {
                parent,
                token,
                bytes,
                children: Vec::new(),
                gone: false,
                unreadable: false,
            }
        }

        fn children<const N: usize>(mut self, children: [u32; N]) -> Self {
            self.children = children.to_vec();
            self
        }

        fn gone(mut self) -> Self {
            self.gone = true;
            self
        }

        fn unreadable(mut self) -> Self {
            self.unreadable = true;
            self
        }
    }

    /// Process facts keyed by PID, with an optional PID reuse right after a memory read.
    struct FakeFacts {
        processes: Mutex<BTreeMap<u32, Process>>,
        reuse: Mutex<Option<(u32, u64)>>,
    }

    impl FakeFacts {
        fn new<const N: usize>(processes: [(u32, Process); N]) -> Self {
            Self {
                processes: Mutex::new(processes.into_iter().collect()),
                reuse: Mutex::new(None),
            }
        }

        fn reuse_after_memory_read(&self, process_id: u32, token: u64) {
            *self.reuse.lock().unwrap() = Some((process_id, token));
        }

        fn process(&self, process_id: u32) -> Result<Option<Process>> {
            let process = self.processes.lock().unwrap().get(&process_id).cloned();
            match process {
                Some(process) if process.unreadable => Err(anyhow::anyhow!("permission denied")),
                Some(process) if process.gone => Ok(None),
                process => Ok(process),
            }
        }
    }

    impl ProcessMemoryFacts for FakeFacts {
        fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
            Ok(self.process(process_id)?.map(|process| ProcessEntry {
                parent_process_id: process.parent,
                start_token: process.token,
            }))
        }

        fn memory_bytes(&self, process_id: u32) -> Result<Option<u64>> {
            let bytes = self.process(process_id)?.map(|process| process.bytes);
            if let Some((reused, token)) = *self.reuse.lock().unwrap()
                && reused == process_id
                && let Some(process) = self.processes.lock().unwrap().get_mut(&process_id)
            {
                process.token = token;
            }
            Ok(bytes)
        }

        fn children(&self, process_id: u32) -> Result<Vec<u32>> {
            Ok(self
                .process(process_id)?
                .map(|process| process.children)
                .unwrap_or_default())
        }
    }
}
