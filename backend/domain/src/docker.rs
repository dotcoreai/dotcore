//! The local Docker daemon endpoint the backend talks to.
//!
//! Every backend Docker client and every Docker tool the backend runs targets this one endpoint,
//! so they always reach the same daemon. It honors a Unix socket chosen in the environment and
//! otherwise uses the system Docker socket.

use std::time::Duration;

/// Environment variable Docker clients read their daemon endpoint from.
pub const DOCKER_HOST_ENV: &str = "DOCKER_HOST";

/// Request timeout of the backend's Docker clients, in seconds.
pub const DOCKER_CLIENT_TIMEOUT_SECONDS: u64 = 120;

/// Longest one bounded Docker Engine operation may take.
pub const DOCKER_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// System Docker socket used when the environment selects no Unix socket.
const DEFAULT_ENDPOINT: &str = "unix:///var/run/docker.sock";

const UNIX_SCHEME: &str = "unix://";

/// Returns the Docker daemon endpoint of this process: the environment's endpoint when it is a
/// Unix socket, otherwise the system Docker socket. Other schemes are ignored rather than
/// followed, so a tool run with this endpoint never reaches a daemon the backend does not use.
pub fn local_endpoint() -> String {
    endpoint_from(std::env::var(DOCKER_HOST_ENV).ok().as_deref())
}

/// Returns the environment's Docker endpoint when it is set but ignored for not being a Unix
/// socket, so the backend can warn that it uses the system Docker socket instead.
pub fn ignored_endpoint() -> Option<String> {
    ignored_from(std::env::var(DOCKER_HOST_ENV).ok().as_deref())
}

fn endpoint_from(value: Option<&str>) -> String {
    value
        .filter(|value| value.starts_with(UNIX_SCHEME))
        .unwrap_or(DEFAULT_ENDPOINT)
        .to_owned()
}

fn ignored_from(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty() && !value.starts_with(UNIX_SCHEME))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_sockets_from_the_environment_are_kept_and_other_endpoints_fall_back() {
        assert_eq!(
            endpoint_from(Some("unix:///run/user/1000/docker.sock")),
            "unix:///run/user/1000/docker.sock"
        );
        assert_eq!(endpoint_from(None), "unix:///var/run/docker.sock");
        assert_eq!(
            endpoint_from(Some("tcp://10.0.0.5:2375")),
            "unix:///var/run/docker.sock"
        );
        assert_eq!(endpoint_from(Some("")), "unix:///var/run/docker.sock");
    }

    #[test]
    fn only_set_endpoints_other_than_unix_sockets_are_reported_as_ignored() {
        assert_eq!(
            ignored_from(Some("tcp://10.0.0.5:2375")).as_deref(),
            Some("tcp://10.0.0.5:2375")
        );
        assert_eq!(
            ignored_from(Some("unix:///run/user/1000/docker.sock")),
            None
        );
        assert_eq!(ignored_from(Some("")), None);
        assert_eq!(ignored_from(None), None);
    }
}
