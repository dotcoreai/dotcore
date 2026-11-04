//! Chooses the host port an application deployment's primary endpoint is published on, so the
//! deployment knows its own URL before its containers start and keeps it for its lifetime.
//!
//! Ports come from one fixed range and are unique within a site: a new deployment gets the first
//! free port at or after a start point derived from its own deployment ID, wrapping around the
//! range. Deployment IDs are never reused, so a port freed by a removed deployment is rarely the
//! next one handed out, and an outside caller still using the old URL does not reach a new
//! application. Allocation only reads the ports retained deployments already hold; it never probes
//! the network.

use std::{collections::BTreeSet, num::NonZeroU16, ops::RangeInclusive};

/// Host ports application deployments are published on: below the Linux (32768) and macOS (49152)
/// ephemeral ranges, so the operating system does not hand them out to other processes.
pub(crate) const PUBLISHED_PORT_RANGE: RangeInclusive<u16> = 30_000..=32_767;

/// Returns the first port of `range` not in `in_use`, scanning from the position `start_offset`
/// selects and wrapping around, or `None` when every port is taken.
pub(crate) fn allocate_published_port(
    range: RangeInclusive<u16>,
    start_offset: u64,
    in_use: &BTreeSet<u16>,
) -> Option<NonZeroU16> {
    let first = u64::from(*range.start());
    let size = u64::from(*range.end()).checked_sub(first)? + 1;
    (0..size)
        .map(|step| first + (start_offset % size + step) % size)
        .filter_map(|port| u16::try_from(port).ok())
        .find(|port| !in_use.contains(port))
        .and_then(NonZeroU16::new)
}

/// Returns a stable scan start for `deployment_id`, so each deployment usually starts at a
/// different port, on one site or across sites sharing a machine.
pub(crate) fn deployment_start_offset(deployment_id: &str) -> u64 {
    // FNV-1a keeps the offset stable across builds and processes, unlike the std hasher.
    deployment_id
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// Reports whether `port` is a published host port a deployment may retain.
pub(crate) fn valid_published_port(port: u32) -> bool {
    u16::try_from(port).is_ok_and(|port| PUBLISHED_PORT_RANGE.contains(&port))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        PUBLISHED_PORT_RANGE, allocate_published_port, deployment_start_offset,
        valid_published_port,
    };

    #[test]
    fn allocation_starts_at_the_offset_and_skips_ports_in_use() {
        let allocate = |offset, in_use: &[u16]| {
            allocate_published_port(10..=14, offset, &in_use.iter().copied().collect())
                .map(|port| port.get())
        };

        assert_eq!(allocate(0, &[]), Some(10));
        assert_eq!(allocate(2, &[]), Some(12));
        assert_eq!(allocate(7, &[]), Some(12));
        assert_eq!(allocate(u64::MAX, &[]), Some(10));
        assert_eq!(allocate(2, &[12, 13]), Some(14));
    }

    #[test]
    fn allocation_wraps_around_and_reports_an_exhausted_range() {
        let in_use = |ports: &[u16]| ports.iter().copied().collect::<BTreeSet<_>>();

        assert_eq!(
            allocate_published_port(10..=14, 3, &in_use(&[13, 14])).map(|port| port.get()),
            Some(10)
        );
        assert_eq!(
            allocate_published_port(10..=14, 3, &in_use(&[10, 11, 12, 13, 14])),
            None
        );
    }

    #[test]
    fn the_full_range_allocates_inside_it_from_a_stable_deployment_offset() {
        let offset = deployment_start_offset("n8n-261015t143000z-00a1b2c3d4");
        assert_eq!(
            offset,
            deployment_start_offset("n8n-261015t143000z-00a1b2c3d4")
        );
        assert_ne!(
            offset,
            deployment_start_offset("n8n-261015t143000z-00a1b2c3d5")
        );

        let port = allocate_published_port(PUBLISHED_PORT_RANGE, offset, &BTreeSet::new())
            .expect("free port");
        assert!(PUBLISHED_PORT_RANGE.contains(&port.get()));
        assert!(valid_published_port(u32::from(port.get())));
    }

    #[test]
    fn a_new_deployment_does_not_take_the_port_a_removed_one_freed() {
        let allocate = |deployment_id| {
            allocate_published_port(
                PUBLISHED_PORT_RANGE,
                deployment_start_offset(deployment_id),
                &BTreeSet::new(),
            )
            .expect("free port")
        };

        let removed = allocate("n8n-261015t143000z-00a1b2c3d4");
        let next = allocate("n8n-261015t143500z-00e5f6a7b8");

        assert_ne!(removed, next);
    }

    #[test]
    fn retained_ports_must_lie_inside_the_range() {
        assert!(valid_published_port(30_000));
        assert!(valid_published_port(32_767));
        assert!(!valid_published_port(0));
        assert!(!valid_published_port(29_999));
        assert!(!valid_published_port(32_768));
        assert!(!valid_published_port(70_000));
    }
}
