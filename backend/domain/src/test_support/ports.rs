//! Reserves TCP ports that a test hands to a backend process before that process binds them, for
//! example a worker's private port recorded at join and bound when the worker starts.
//!
//! Ports come from a fixed pool outside the host's ephemeral range, so no `bind(…, 0)` or
//! outbound connection anywhere on the host can be given one. Each reservation holds a file lock
//! that excludes every other test process on the host until it is dropped or its process exits.

use std::{
    fs::{self, File, OpenOptions},
    net::{Ipv4Addr, SocketAddr},
    ops::RangeInclusive,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use tokio::net::TcpSocket;

/// Ports test reservations come from, below the Linux (32768–60999) and macOS (49152–65535)
/// default ephemeral ranges.
const RESERVED_PORTS: RangeInclusive<u16> = 20000..=20999;

/// One reserved port, kept from other test processes while this value lives. Nothing is bound
/// on it, so a process that stops listening on it refuses connections as usual.
#[derive(Debug)]
pub struct ReservedPort {
    port: u16,
    _claim: File,
}

impl ReservedPort {
    /// Returns the reserved port number.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Reserves a port of [`RESERVED_PORTS`] that no other test process holds and nothing on the host
/// is bound to. Panics when the pool overlaps the host's ephemeral range or is exhausted.
pub fn reserve_port() -> ReservedPort {
    static CHECKED: OnceLock<()> = OnceLock::new();
    CHECKED.get_or_init(|| {
        check_outside_ephemeral(&RESERVED_PORTS, &ephemeral_ports())
            .unwrap_or_else(|message| panic!("{message}"))
    });

    // A random start spreads concurrent processes over the pool instead of contending for its
    // first ports.
    let pool: Vec<u16> = RESERVED_PORTS.collect();
    let start = getrandom::u32().expect("random reserved port start") as usize % pool.len();
    let candidates: Vec<u16> = pool[start..]
        .iter()
        .chain(&pool[..start])
        .copied()
        .collect();
    reserve_from(&candidates, claim_directory()).unwrap_or_else(|| {
        panic!(
            "every port of {RESERVED_PORTS:?} is held by another test process or bound on this host"
        )
    })
}

/// Reserves the first of `candidates` whose claim in `claims` is free and that nothing on the
/// host is bound to, or returns `None` when there is none.
fn reserve_from(candidates: &[u16], claims: &Path) -> Option<ReservedPort> {
    candidates.iter().find_map(|&port| {
        // The lock file is never deleted: unlinking a file another process holds locked would let
        // a third process lock a new file of the same name.
        let claim = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(claims.join(port.to_string()))
            .unwrap_or_else(|error| panic!("open port claim {port} in {claims:?}: {error}"));
        // SAFETY: the descriptor belongs to `claim`, which outlives the call.
        if unsafe { libc::flock(claim.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return None;
        }
        port_is_free(port).then_some(ReservedPort {
            port,
            _claim: claim,
        })
    })
}

/// Reports whether the port can be bound on every IPv4 address. The probe skips `SO_REUSEADDR`,
/// which the standard listener sets, so a listener on any loopback alias, a dual-stack wildcard
/// listener or a lingering connection on the port all make it fail.
fn port_is_free(port: u16) -> bool {
    let socket = TcpSocket::new_v4().expect("port probe socket");
    socket
        .bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
        .is_ok()
}

/// Returns an error naming the setting to change when `pool` overlaps `ephemeral`.
fn check_outside_ephemeral(
    pool: &RangeInclusive<u16>,
    ephemeral: &RangeInclusive<u16>,
) -> Result<(), String> {
    if pool.start() <= ephemeral.end() && ephemeral.start() <= pool.end() {
        return Err(format!(
            "reserved test ports {pool:?} overlap this host's ephemeral ports {ephemeral:?}, so \
             the host may hand them to any socket; move the ephemeral range above {} \
             (`net.ipv4.ip_local_port_range` on Linux, `net.inet.ip.portrange.first` on macOS)",
            pool.end()
        ));
    }
    Ok(())
}

/// Shared claim directory of every test process of this user, falling back to the process's
/// temporary directory where `/tmp` is not writable, as in a build sandbox.
fn claim_directory() -> &'static Path {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let name = format!("dotcore-test-ports-{}", unsafe { libc::geteuid() });
        let shared = Path::new("/tmp").join(&name);
        if fs::create_dir_all(&shared).is_ok() {
            return shared;
        }
        let fallback = std::env::temp_dir().join(name);
        fs::create_dir_all(&fallback)
            .unwrap_or_else(|error| panic!("create port claim directory {fallback:?}: {error}"));
        fallback
    })
}

/// Reads the host's ephemeral port range.
#[cfg(target_os = "linux")]
fn ephemeral_ports() -> RangeInclusive<u16> {
    let path = "/proc/sys/net/ipv4/ip_local_port_range";
    let text = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    let bounds: Vec<u16> = text
        .split_whitespace()
        .map(|bound| bound.parse().expect("ephemeral port bound"))
        .collect();
    let [first, last] = bounds[..] else {
        panic!("{path} holds {text:?}, not two ports");
    };
    first..=last
}

/// Reads the host's ephemeral port range.
#[cfg(target_os = "macos")]
fn ephemeral_ports() -> RangeInclusive<u16> {
    let bound = |name: &std::ffi::CStr| {
        let mut value: libc::c_int = 0;
        let mut length = std::mem::size_of::<libc::c_int>();
        // SAFETY: the name is NUL-terminated and the buffer is one `c_int` of the given length.
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(
            result,
            0,
            "read {name:?}: {}",
            std::io::Error::last_os_error()
        );
        u16::try_from(value).expect("ephemeral port bound")
    };
    bound(c"net.inet.ip.portrange.first")..=bound(c"net.inet.ip.portrange.last")
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    /// A loopback alias that Linux binds by default and macOS only with an added alias.
    const OTHER_LOOPBACK_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

    #[test]
    fn concurrent_reservations_hold_distinct_ports() {
        let first = reserve_port();
        let second = reserve_port();

        assert_ne!(first.port(), second.port());
        assert!(RESERVED_PORTS.contains(&first.port()));
        assert!(RESERVED_PORTS.contains(&second.port()));
    }

    #[test]
    fn a_port_bound_on_any_ipv4_address_is_skipped() {
        let mut addresses = vec![Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED];
        if TcpListener::bind((OTHER_LOOPBACK_ADDRESS, 0)).is_ok() {
            addresses.push(OTHER_LOOPBACK_ADDRESS);
        }
        for address in addresses {
            // Both candidates are held in the shared claims, so no other test process uses them.
            let (taken, free) = (reserve_port(), reserve_port());
            let _listener = TcpListener::bind((address, taken.port())).expect("occupy the port");
            let claims = tempfile::tempdir().expect("claim directory");

            let reserved = reserve_from(&[taken.port(), free.port()], claims.path())
                .expect("the free candidate");

            assert_eq!(reserved.port(), free.port(), "listener on {address}");
        }
    }

    #[test]
    fn a_held_claim_excludes_until_dropped_and_keeps_its_file() {
        let candidate = reserve_port();
        let claims = tempfile::tempdir().expect("claim directory");
        let held = reserve_from(&[candidate.port()], claims.path()).expect("first claim");

        assert!(reserve_from(&[candidate.port()], claims.path()).is_none());

        drop(held);
        let again = reserve_from(&[candidate.port()], claims.path()).expect("released claim");
        assert_eq!(again.port(), candidate.port());
        assert!(claims.path().join(candidate.port().to_string()).exists());
    }

    #[test]
    fn exhausted_candidates_reserve_nothing() {
        let candidate = reserve_port();
        let claims = tempfile::tempdir().expect("claim directory");
        let _held = reserve_from(&[candidate.port()], claims.path()).expect("only claim");

        assert!(reserve_from(&[candidate.port()], claims.path()).is_none());
        assert!(reserve_from(&[], claims.path()).is_none());
    }

    #[test]
    fn the_pool_must_lie_outside_the_ephemeral_range() {
        assert_eq!(
            check_outside_ephemeral(&RESERVED_PORTS, &(32768..=60999)),
            Ok(())
        );
        assert_eq!(
            check_outside_ephemeral(&RESERVED_PORTS, &(49152..=65535)),
            Ok(())
        );
        assert_eq!(
            check_outside_ephemeral(&RESERVED_PORTS, &ephemeral_ports()),
            Ok(())
        );

        let error = check_outside_ephemeral(&RESERVED_PORTS, &(1024..=65535))
            .expect_err("a wide ephemeral range overlaps the pool");

        assert_eq!(
            error,
            "reserved test ports 20000..=20999 overlap this host's ephemeral ports 1024..=65535, \
             so the host may hand them to any socket; move the ephemeral range above 20999 \
             (`net.ipv4.ip_local_port_range` on Linux, `net.inet.ip.portrange.first` on macOS)"
        );
    }
}
