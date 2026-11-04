//! Walks the ancestry of a host process lifetime, so a GPU process can be related to the workload
//! that spawned it, for example a worker child whose parent is a container's root process.
//!
//! A lifetime pairs a PID with its start token, so a process that reuses a PID is never mistaken
//! for an earlier one. The walk is bounded and stops at any parent that started after its child.

use anyhow::{Context, Result};

use crate::{HostProcessTable, ProcessTable};

/// Maximum number of parents followed from one process, bounding malformed or cyclic tables.
const MAX_ANCESTRY_DEPTH: usize = 32;

/// One process lifetime on the local host: a PID plus the start token that distinguishes it from
/// any later process reusing the same PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcessLifetime {
    pub process_id: u32,
    pub start_token: u64,
}

/// Reads the start token of a running process, failing when it is not running.
pub fn read_process_start_token(process_id: u32) -> Result<u64> {
    HostProcessTable
        .entry(process_id)?
        .map(|entry| entry.start_token)
        .context("process is not running")
}

/// Returns the ancestors of a process lifetime, nearest parent first.
///
/// The walk stops at the first missing process, at a parent whose start token is larger than its
/// child's (its PID was reused), or at the depth bound. An empty result means the leaf itself no
/// longer matches its start token or has no parent. Callers match ancestors by exact lifetime, so
/// PID reuse along the chain can only hide a relation, never invent one.
pub fn process_ancestors(
    table: &dyn ProcessTable,
    leaf: ProcessLifetime,
) -> Result<Vec<ProcessLifetime>> {
    let Some(mut entry) = table.entry(leaf.process_id)? else {
        return Ok(Vec::new());
    };
    if entry.start_token != leaf.start_token {
        return Ok(Vec::new());
    }

    let mut ancestors = Vec::new();
    let mut child_start_token = leaf.start_token;
    while ancestors.len() < MAX_ANCESTRY_DEPTH && entry.parent_process_id != 0 {
        let process_id = entry.parent_process_id;
        let Some(parent) = table.entry(process_id)? else {
            break;
        };
        if parent.start_token > child_start_token {
            break;
        }
        ancestors.push(ProcessLifetime {
            process_id,
            start_token: parent.start_token,
        });
        child_start_token = parent.start_token;
        entry = parent;
    }
    Ok(ancestors)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
        time::Duration,
    };

    use super::*;
    use crate::{HostProcessTable, ProcessEntry, test_support::FakeProcessTable};

    #[test]
    fn current_process_has_a_stable_nonzero_lifetime_discriminator() {
        let process_id = std::process::id();

        let first = read_process_start_token(process_id).expect("current process start token");
        let second = read_process_start_token(process_id).expect("current process start token");

        assert!(first > 0);
        assert_eq!(first, second);
    }

    #[test]
    fn current_process_parent_matches_the_operating_system() {
        let entry = HostProcessTable
            .entry(std::process::id())
            .unwrap()
            .expect("current process entry");

        assert_eq!(entry.parent_process_id, std::os::unix::process::parent_id());
    }

    #[test]
    fn missing_process_has_no_entry() {
        // Neither platform hands out PIDs beyond i32::MAX, so this PID cannot be running.
        assert_eq!(HostProcessTable.entry(u32::MAX).unwrap(), None);
        assert_eq!(
            read_process_start_token(u32::MAX).unwrap_err().to_string(),
            "process is not running"
        );
    }

    #[test]
    fn ancestors_follow_parents_nearest_first_until_the_table_root() {
        let table = FakeProcessTable::new([(1275, 1200, 600), (1200, 1, 500), (1, 0, 1)]);

        let ancestors = process_ancestors(&table, lifetime(1275, 600)).unwrap();

        assert_eq!(ancestors, vec![lifetime(1200, 500), lifetime(1, 1)]);
    }

    #[test]
    fn ancestors_stop_at_a_missing_parent() {
        let table = FakeProcessTable::new([(1275, 1200, 600)]);

        assert_eq!(
            process_ancestors(&table, lifetime(1275, 600)).unwrap(),
            vec![]
        );
    }

    #[test]
    fn ancestors_stop_at_a_parent_started_after_its_child() {
        let table = FakeProcessTable::new([(1275, 1200, 600), (1200, 1, 900), (1, 0, 1)]);

        assert_eq!(
            process_ancestors(&table, lifetime(1275, 600)).unwrap(),
            vec![]
        );
    }

    #[test]
    fn ancestors_are_empty_when_the_leaf_was_replaced() {
        let table = FakeProcessTable::new([(1275, 1200, 700), (1200, 1, 500)]);

        assert_eq!(
            process_ancestors(&table, lifetime(1275, 600)).unwrap(),
            vec![]
        );
    }

    #[test]
    fn ancestors_are_bounded_on_cyclic_tables() {
        let table = FakeProcessTable::new([(10, 20, 5), (20, 10, 5)]);

        let ancestors = process_ancestors(&table, lifetime(10, 5)).unwrap();

        assert_eq!(ancestors.len(), MAX_ANCESTRY_DEPTH);
        assert_eq!(ancestors[..2], [lifetime(20, 5), lifetime(10, 5)]);
    }

    #[test]
    fn host_ancestors_walk_a_spawned_grandchild_up_to_this_process() {
        let mut shell = Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn shell");
        let mut line = String::new();
        BufReader::new(shell.stdout.take().unwrap())
            .read_line(&mut line)
            .expect("read grandchild PID");
        let grandchild_id = line.trim().parse::<u32>().expect("grandchild PID");

        let expected = [host_lifetime(shell.id()), host_lifetime(std::process::id())];
        let ancestors = process_ancestors(&HostProcessTable, host_lifetime(grandchild_id)).unwrap();

        // SAFETY: kill takes a PID and a signal and has no memory effects.
        unsafe { libc::kill(grandchild_id as libc::pid_t, libc::SIGKILL) };
        shell.wait().unwrap();
        assert_eq!(ancestors[..2], expected);
    }

    #[test]
    fn host_ancestors_stop_at_a_parent_pid_reused_after_the_child_started() {
        let mut child = sleeping_child();
        // Linux tokens count clock ticks, so the later process must start at least one tick later.
        std::thread::sleep(Duration::from_millis(50));
        let mut later = sleeping_child();
        let table = ReparentedTable {
            child: child.id(),
            parent: later.id(),
        };

        let leaf = host_lifetime(child.id());
        let ancestors = process_ancestors(&table, leaf).unwrap();
        let later_token = host_lifetime(later.id()).start_token;

        for process in [&mut child, &mut later] {
            process.kill().unwrap();
            process.wait().unwrap();
        }
        assert!(later_token > leaf.start_token);
        assert_eq!(ancestors, vec![]);
    }

    fn lifetime(process_id: u32, start_token: u64) -> ProcessLifetime {
        ProcessLifetime {
            process_id,
            start_token,
        }
    }

    fn host_lifetime(process_id: u32) -> ProcessLifetime {
        lifetime(
            process_id,
            read_process_start_token(process_id).expect("running process start token"),
        )
    }

    fn sleeping_child() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn sleep")
    }

    /// Host process table that reports `parent` as the parent of `child`, as if the child's real
    /// parent exited and its PID went to a later process.
    struct ReparentedTable {
        child: u32,
        parent: u32,
    }

    impl ProcessTable for ReparentedTable {
        fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
            Ok(HostProcessTable.entry(process_id)?.map(|mut entry| {
                if process_id == self.child {
                    entry.parent_process_id = self.parent;
                }
                entry
            }))
        }
    }
}
