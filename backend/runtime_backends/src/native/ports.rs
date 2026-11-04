//! Assigns loopback ports to the ports a native workload declares, for example `http` to
//! `127.0.0.1:52001`, by asking the host for free ones.
//!
//! A relaunch keeps each previously assigned port that is still free, so a restarted workload is
//! reachable where it was. Every port of one assignment is distinct, because all of them stay
//! bound until the assignment is complete.

use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddr},
};

use dotcore_runtime::WorkloadPort;
use tokio::net::TcpSocket;

/// Assigns a distinct free loopback port to every declared port, preferring the previous
/// assignment of each port while it is still free.
pub(super) fn assign_ports(
    declared: &[WorkloadPort],
    previous: &BTreeMap<String, u16>,
) -> io::Result<BTreeMap<String, u16>> {
    let mut reserved = Vec::with_capacity(declared.len());
    let mut assigned = BTreeMap::new();
    for port in declared {
        let socket = previous
            .get(&port.name)
            .and_then(|previous| reserve(*previous).ok())
            .map_or_else(|| reserve(0), Ok)?;
        assigned.insert(port.name.clone(), socket.local_addr()?.port());
        reserved.push(socket);
    }
    Ok(assigned)
}

/// Binds a loopback port without `SO_REUSEADDR`, which the standard listener sets: with it,
/// macOS lets a loopback bind succeed while another process listens on the same port of every
/// address.
fn reserve(port: u16) -> io::Result<TcpSocket> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_previous_ports_are_kept_and_taken_ones_are_reassigned() {
        let declared = [port("http"), port("metrics")];
        let first = assign_ports(&declared, &BTreeMap::new()).expect("first assignment");
        let taken =
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, first["metrics"])).expect("take");

        let second = assign_ports(&declared, &first).expect("second assignment");

        assert_ne!(first["http"], first["metrics"]);
        assert_eq!(second["http"], first["http"]);
        assert_ne!(second["metrics"], first["metrics"]);
        assert_ne!(second["metrics"], second["http"]);
        drop(taken);
    }

    #[test]
    fn a_port_another_process_listens_on_for_every_address_is_not_reused() {
        let declared = [port("http")];
        let first = assign_ports(&declared, &BTreeMap::new()).expect("first assignment");
        let wildcard = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, first["http"]))
            .expect("listen on every address");

        let second = assign_ports(&declared, &first).expect("second assignment");

        assert_ne!(second["http"], first["http"]);
        drop(wildcard);
    }

    fn port(name: &str) -> WorkloadPort {
        WorkloadPort {
            name: name.to_owned(),
            container_port: 8080,
        }
    }
}
