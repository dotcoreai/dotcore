//! Host memory sampling for node hardware collection.
//!
//! It reports the host's total memory and the memory a workload can obtain without evicting other
//! applications' working memory. Reclaimable file cache counts as available, so used memory is the
//! total minus available: application, wired and compressed memory.
//!
//! Example: a host with 64 GiB total, 20 GiB of application memory and 30 GiB of file cache
//! reports 44 GiB available and 30 GiB cached.

use sysinfo::System;

/// Total and available host memory in bytes, with the reclaimable file cache when the host reports
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostMemory {
    pub(crate) total_bytes: u64,
    pub(crate) available_bytes: u64,
    pub(crate) cached_bytes: Option<u64>,
}

impl HostMemory {
    /// Samples the host memory with a freshly refreshed memory reading, or `None` when the host
    /// reports no total memory.
    pub(crate) fn read() -> Option<Self> {
        let mut system = System::new();
        system.refresh_memory();

        Self::from_system(&system)
    }

    /// Derives the host memory from a system whose memory was refreshed.
    pub(crate) fn from_system(system: &System) -> Option<Self> {
        // `used_memory` is the operating system's own used figure: total minus MemAvailable on
        // Linux and app, wired and compressed memory on macOS. Deriving available from it keeps
        // active file cache available on both platforms.
        Self::from_used(
            system.total_memory(),
            system.used_memory(),
            read_cached_bytes(),
        )
    }

    fn from_used(total_bytes: u64, used_bytes: u64, cached_bytes: Option<u64>) -> Option<Self> {
        if total_bytes == 0 {
            return None;
        }

        Some(Self {
            total_bytes,
            available_bytes: total_bytes.saturating_sub(used_bytes),
            cached_bytes,
        })
    }
}

#[cfg(target_os = "linux")]
fn read_cached_bytes() -> Option<u64> {
    parse_cached_bytes(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

// macOS exposes no file cache figure through sysinfo.
#[cfg(not(target_os = "linux"))]
fn read_cached_bytes() -> Option<u64> {
    None
}

/// Returns the file cache in bytes from `/proc/meminfo` text: page cache and buffers plus
/// reclaimable slab, minus shared memory that the page cache also holds.
#[cfg(any(target_os = "linux", test))]
fn parse_cached_bytes(meminfo: &str) -> Option<u64> {
    let field = |name: &str| -> Option<u64> {
        let line = meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))?;
        let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;

        Some(kib.saturating_mul(1024))
    };

    let cached = field("Cached")?;
    let buffers = field("Buffers").unwrap_or(0);
    let reclaimable = field("SReclaimable").unwrap_or(0);
    let shared = field("Shmem").unwrap_or(0);

    Some(
        cached
            .saturating_add(buffers)
            .saturating_add(reclaimable)
            .saturating_sub(shared),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       65536000 kB\n\
        MemFree:         1000000 kB\n\
        MemAvailable:   50000000 kB\n\
        Buffers:          500000 kB\n\
        Cached:         30000000 kB\n\
        SReclaimable:    2000000 kB\n\
        Shmem:           1000000 kB\n";

    #[test]
    fn cached_bytes_add_cache_buffers_and_reclaimable_slab_minus_shared() {
        // (30_000_000 + 500_000 + 2_000_000 - 1_000_000) kB
        assert_eq!(parse_cached_bytes(MEMINFO), Some(31_500_000 * 1024));
    }

    #[test]
    fn cached_bytes_saturate_when_shared_memory_exceeds_cache() {
        let meminfo = "Cached: 100 kB\nShmem: 900 kB\n";

        assert_eq!(parse_cached_bytes(meminfo), Some(0));
    }

    #[test]
    fn cached_bytes_are_unknown_without_a_cache_line() {
        assert_eq!(parse_cached_bytes("MemTotal: 10 kB\n"), None);
    }

    #[test]
    fn available_is_total_minus_used() {
        let memory = HostMemory::from_used(64_000, 20_000, Some(30_000)).unwrap();

        assert_eq!(
            memory,
            HostMemory {
                total_bytes: 64_000,
                available_bytes: 44_000,
                cached_bytes: Some(30_000),
            }
        );
    }

    #[test]
    fn available_never_goes_negative_when_used_exceeds_total() {
        let memory = HostMemory::from_used(1_000, 1_500, None).unwrap();

        assert_eq!(memory.available_bytes, 0);
    }

    #[test]
    fn missing_total_is_unavailable() {
        assert_eq!(HostMemory::from_used(0, 0, None), None);
    }
}
