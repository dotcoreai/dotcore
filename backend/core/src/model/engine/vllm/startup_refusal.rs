//! Recognizes vLLM's refusal to start when the device has less free memory than its startup
//! memory budget, for example `Free memory on device cuda:0 (111.66/121.63 GiB) on startup is
//! less than desired GPU memory utilization (0.97, 117.98 GiB)`.
//!
//! The refusal is reported with the memory vLLM needed and the memory that was free, with or
//! without weights held by another process. Only fixed marker text is matched, so reworded or
//! malformed lines are not recognized. When the line repeats, the last occurrence wins.

use super::startup_facts::gib_bytes;
use crate::model::engine::startup_refusal::StartupRefusal;

/// Start of vLLM's refusal when free device memory is below its startup memory budget.
const MEMORY_REFUSAL_MARKER: &str = "Free memory on device ";

/// Text between the free memory figures and the budget in that refusal.
const MEMORY_REFUSAL_COMPARISON: &str = " GiB) on startup is less than ";

/// Returns vLLM's refusal in startup output, in chronological order with one entry per line.
pub(super) fn startup_refusal(output: &str) -> Option<StartupRefusal> {
    output.lines().rev().find_map(memory_refusal)
}

/// Reads the free figure from `(free/total GiB)` and the budget from the last
/// `(utilization, budget GiB)` of one refusal line.
fn memory_refusal(line: &str) -> Option<StartupRefusal> {
    let refusal = &line[line.find(MEMORY_REFUSAL_MARKER)?..];
    let (free, budget) = refusal.split_once(MEMORY_REFUSAL_COMPARISON)?;
    let free_bytes = gib_bytes(free.rsplit_once('(')?.1.split_once('/')?.0)?;
    let needed_bytes = gib_bytes(budget.split_once(" GiB)")?.0.rsplit_once(", ")?.1)?;
    Some(StartupRefusal::InsufficientFreeMemory {
        needed_bytes,
        free_bytes,
        line: line.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal captured from vLLM 0.32 on a DGX Spark, as raised in its engine core process,
    /// followed by the traceback and shutdown warnings it prints after.
    const MEMORY_REFUSAL: &str = "\
(EngineCore pid=212) INFO 11-04 09:12:40 [gpu_model_runner.py:5483] Model loading took 52.18 GiB memory and 41.204417 seconds
(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080] EngineCore failed to start.
(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080] Traceback (most recent call last):
(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080]   File \"[REDACTED]\", line 260, in determine_available_memory
(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080]     self.requested_memory = request_memory(init_snapshot, self.cache_config)
(EngineCore pid=212) ERROR 11-04 09:12:52 [core.py:1080] ValueError: Free memory on device cuda:0 (111.66/121.63 GiB) on startup is less than desired GPU memory utilization (0.97, 117.98 GiB). Decrease GPU memory utilization or reduce GPU memory used by other processes.
(APIServer pid=1) RuntimeError: Engine core initialization failed. See root cause above. Failed core proc(s): {}";

    fn gib(value: &str) -> u64 {
        gib_bytes(value).expect("GiB figure")
    }

    #[test]
    fn memory_refusal_reports_needed_and_free_memory() {
        assert_eq!(
            startup_refusal(MEMORY_REFUSAL),
            Some(StartupRefusal::InsufficientFreeMemory {
                needed_bytes: gib("117.98"),
                free_bytes: gib("111.66"),
                line: MEMORY_REFUSAL
                    .lines()
                    .nth(5)
                    .expect("refusal line")
                    .to_owned(),
            })
        );
    }

    #[test]
    fn memory_refusal_with_externally_held_weights_reports_the_engine_budget() {
        let line = "ValueError: Free memory on device cuda:0 (20.10/31.36 GiB) on startup is less than the engine's budget after excluding external process's weights (0.9, 24.50 GiB). Decrease GPU memory utilization or reduce GPU memory used by other processes.";

        assert_eq!(
            startup_refusal(line),
            Some(StartupRefusal::InsufficientFreeMemory {
                needed_bytes: gib("24.50"),
                free_bytes: gib("20.10"),
                line: line.to_owned(),
            })
        );
    }

    #[test]
    fn other_startup_failures_have_no_refusal() {
        let output = "ValueError: Free memory on device cuda:0 (lots/31.36 GiB) on startup is less than desired GPU memory utilization (0.9, 24.50 GiB).\nRuntimeError: Engine core initialization failed. See root cause above.";

        assert_eq!(startup_refusal(output), None);
    }
}
