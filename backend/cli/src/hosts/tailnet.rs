//! This machine's tailnet status.
//!
//! Reads whether the machine is on a tailnet, its tailnet IPv4 addresses and its tailnet name from
//! the tailnet client. Having no tailnet is a normal setup, so every failure (no client, stopped or
//! logged-out client, slow answer, unexpected output) means no tailnet and leaves only a debug
//! trace; nothing reaches the operator.

use std::{
    ffi::OsStr,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use dotcore_domain::SiteNodeHost;
use serde_json::Value;

/// Longest wait for the tailnet client to report its status.
// Not a behavior duration: bound on a real tailnet client process.
const STATUS_DEADLINE: Duration = Duration::from_secs(2);

/// Absolute locations of the tailnet client searched after the executable search path, for
/// installs a minimal search path misses: application bundles, package managers and system
/// profiles.
const KNOWN_BINARY_PATHS: &[&str] = &[
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
    "/run/current-system/sw/bin/tailscale",
];

/// This machine's membership in a running tailnet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TailnetStatus {
    /// Tailnet name of this machine, present only when the tailnet provides names.
    pub dns_name: Option<SiteNodeHost>,

    /// Tailnet IPv4 addresses of this machine, in client order; never empty.
    pub ipv4: Vec<Ipv4Addr>,
}

/// Reads the tailnet status from the client found on the executable search path or at a known
/// location.
pub(super) async fn system_tailnet_status() -> Option<TailnetStatus> {
    let candidates = binary_candidates(std::env::var_os("PATH").as_deref());
    let Some(binary) = candidates.iter().find(|path| is_executable(path)) else {
        tracing::debug!("no tailnet status: Tailscale client not found");
        return None;
    };
    read_status(binary, STATUS_DEADLINE).await
}

/// Asks the client at `binary` for its status, waiting at most `deadline` for its answer.
async fn read_status(binary: &Path, deadline: Duration) -> Option<TailnetStatus> {
    // The client's own diagnostics never reach the operator's terminal.
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[expect(
        clippy::disallowed_methods,
        reason = "bounds a real tailnet client process"
    )]
    let output = match tokio::time::timeout(deadline, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            tracing::debug!(binary = %binary.display(), %error, "no tailnet status: client did not run");
            return None;
        }
        Err(_) => {
            tracing::debug!(
                ?deadline,
                "no tailnet status: client did not answer in time"
            );
            return None;
        }
    };
    if !output.status.success() {
        tracing::debug!(status = %output.status, "no tailnet status: client failed");
        return None;
    }
    parse_status(&output.stdout)
}

/// Reads the status JSON leniently, since its format is not a stable interface: only a running
/// backend with at least one IPv4 address is a tailnet, and absent fields read as unset.
fn parse_status(json: &[u8]) -> Option<TailnetStatus> {
    let status: Value = serde_json::from_slice(json)
        .inspect_err(|error| tracing::debug!(%error, "no tailnet status: output is not JSON"))
        .ok()?;

    let state = status["BackendState"].as_str().unwrap_or("unknown");
    if state != "Running" {
        tracing::debug!(state, "no tailnet status: backend is not running");
        return None;
    }

    let this_node = &status["Self"];
    let ipv4: Vec<Ipv4Addr> = this_node["TailscaleIPs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|address| address.as_str()?.parse().ok())
        .collect();
    if ipv4.is_empty() {
        tracing::debug!("no tailnet status: no tailnet IPv4 address");
        return None;
    }

    let magic_dns = status["CurrentTailnet"]["MagicDNSEnabled"]
        .as_bool()
        .unwrap_or(false);
    let dns_name = this_node["DNSName"]
        .as_str()
        .filter(|name| magic_dns && !name.is_empty())
        .and_then(|name| {
            SiteNodeHost::normalize(name)
                .inspect_err(
                    |error| tracing::debug!(name, %error, "tailnet name is not a host name"),
                )
                .ok()
        });

    Some(TailnetStatus { dns_name, ipv4 })
}

/// Lists where the client is searched, in order: each absolute directory of the search path,
/// then the known install locations.
fn binary_candidates(search_path: Option<&OsStr>) -> Vec<PathBuf> {
    search_path
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join("tailscale"))
        .chain(KNOWN_BINARY_PATHS.iter().map(PathBuf::from))
        .collect()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use dotcore_domain::SiteNodeHost;

    use super::{KNOWN_BINARY_PATHS, TailnetStatus, binary_candidates, parse_status, read_status};
    use crate::test_support::{canonical_tempdir, write_executable_script};

    const TAILNET_IPV4: Ipv4Addr = Ipv4Addr::new(100, 101, 102, 103);

    #[test]
    fn a_running_node_with_magic_dns_reports_its_canonical_name_and_ipv4_only() {
        let status = parse(
            r#"{
                "BackendState": "Running",
                "Self": {
                    "DNSName": "gpu1.example.ts.net.",
                    "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]
                },
                "CurrentTailnet": {"Name": "example", "MagicDNSEnabled": true}
            }"#,
        );

        assert_eq!(
            status,
            Some(TailnetStatus {
                dns_name: Some(SiteNodeHost::parse("gpu1.example.ts.net").expect("name")),
                ipv4: vec![TAILNET_IPV4],
            })
        );
    }

    #[test]
    fn disabled_magic_dns_keeps_the_ipv4_without_a_name() {
        let status = parse(
            r#"{
                "BackendState": "Running",
                "Self": {"DNSName": "gpu1.example.ts.net.", "TailscaleIPs": ["100.101.102.103"]},
                "CurrentTailnet": {"MagicDNSEnabled": false}
            }"#,
        );

        assert_eq!(
            status,
            Some(TailnetStatus {
                dns_name: None,
                ipv4: vec![TAILNET_IPV4],
            })
        );
    }

    #[test]
    fn a_stopped_logged_out_or_unreadable_client_is_no_tailnet() {
        for output in [
            r#"{"BackendState": "NeedsLogin", "Self": {"DNSName": "", "TailscaleIPs": []}}"#,
            r#"{"BackendState": "Running", "Self": {"TailscaleIPs": ["fd7a:115c:a1e0::1"]}}"#,
            r#"{"Self": {}}"#,
            "Tailscale is stopped.",
        ] {
            assert_eq!(parse(output), None, "{output}");
        }
    }

    #[test]
    fn the_client_is_searched_on_absolute_path_entries_then_at_known_locations() {
        let search_path =
            std::env::join_paths(["/nix/profile/bin", "relative/bin", "/usr/sbin"]).expect("PATH");

        let mut expected = vec![
            PathBuf::from("/nix/profile/bin/tailscale"),
            PathBuf::from("/usr/sbin/tailscale"),
        ];
        expected.extend(KNOWN_BINARY_PATHS.iter().map(PathBuf::from));
        assert_eq!(binary_candidates(Some(&search_path)), expected);
    }

    #[tokio::test]
    async fn a_client_slower_than_the_deadline_is_no_tailnet() {
        let directory = canonical_tempdir().expect("temporary directory");
        let script = directory.path().join("tailscale");
        write_executable_script(&script, "#!/bin/sh\nexec sleep 60\n", 0o755);

        let started = Instant::now();
        let status = read_status(&script, Duration::from_millis(20)).await;

        assert_eq!(status, None);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the slow client was abandoned at the deadline"
        );
    }

    fn parse(json: &str) -> Option<TailnetStatus> {
        parse_status(json.as_bytes())
    }
}
