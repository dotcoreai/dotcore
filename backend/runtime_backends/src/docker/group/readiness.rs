//! Bounded READY dependency launch gates for Docker workload groups.

use std::net::{IpAddr, SocketAddr};

use dotcore_runtime::{
    ReadyLaunchGate, ReadyLaunchGateFailure, ReadyLaunchGateOutcome, RuntimeBackendError,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time,
};

use super::super::{
    adapter::DockerAdapter, client::DockerClient, request::DockerGroupContainerRequest,
    runtime_error, runtime_error_with_source,
};

const MAX_HTTP_RESPONSE_HEADER_BYTES: usize = 16 * 1024;

impl<C: DockerClient> DockerAdapter<C> {
    #[expect(
        clippy::disallowed_methods,
        reason = "probes a real container at its declared gate interval and timeout"
    )]
    pub(super) async fn wait_ready(
        &self,
        container_id: &str,
        request: &DockerGroupContainerRequest,
        gate: &ReadyLaunchGate,
    ) -> Result<ReadyLaunchGateOutcome, RuntimeBackendError> {
        let port = request
            .ports
            .iter()
            .find(|port| port.name == gate.port_name())
            .ok_or_else(|| runtime_error("Docker READY launch gate has no declared unit port"))?
            .container_port;
        let deadline = time::Instant::now() + gate.timeout();
        loop {
            let Some(remaining) = deadline.checked_duration_since(time::Instant::now()) else {
                return Ok(ReadyLaunchGateOutcome::TimedOut);
            };
            let inspection =
                match time::timeout(remaining, self.client.inspect_group_container(container_id))
                    .await
                {
                    Ok(Ok(inspection)) => inspection,
                    Ok(Err(error)) => {
                        return Err(runtime_error_with_source(
                            "Docker READY launch-gate inspection failed",
                            error,
                        ));
                    }
                    Err(_) => return Ok(ReadyLaunchGateOutcome::TimedOut),
                };
            let Some(address) = inspection.private_address else {
                return Ok(ReadyLaunchGateOutcome::Failed(
                    ReadyLaunchGateFailure::ConnectionFailed,
                ));
            };
            let Some(remaining) = deadline.checked_duration_since(time::Instant::now()) else {
                return Ok(ReadyLaunchGateOutcome::TimedOut);
            };
            match probe_http(address, port, gate.path(), remaining).await {
                Ok(true) => return Ok(ReadyLaunchGateOutcome::Passed),
                Ok(false) => {
                    return Ok(ReadyLaunchGateOutcome::Failed(
                        ReadyLaunchGateFailure::InvalidResponse,
                    ));
                }
                Err(_) if time::Instant::now() >= deadline => {
                    return Ok(ReadyLaunchGateOutcome::TimedOut);
                }
                Err(_) => time::sleep(gate.interval().min(remaining)).await,
            }
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "bounds one real HTTP probe of a container"
)]
async fn probe_http(
    address: std::net::Ipv4Addr,
    port: u16,
    path: &str,
    timeout: std::time::Duration,
) -> std::io::Result<bool> {
    time::timeout(timeout, async {
        let mut stream = TcpStream::connect(SocketAddr::new(IpAddr::V4(address), port)).await?;
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: workload\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        read_response_headers(&mut stream).await
    })
    .await
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?
}

async fn read_response_headers(stream: &mut TcpStream) -> std::io::Result<bool> {
    let mut headers = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "HTTP response ended before complete headers",
            ));
        }
        let tail_start = headers.len().saturating_sub(3);
        let mut boundary = headers[tail_start..].to_vec();
        boundary.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = boundary
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| tail_start + index)
        {
            if header_end + 4 > MAX_HTTP_RESPONSE_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HTTP response headers exceed size limit",
                ));
            }
            let header_bytes_in_read = header_end + 4 - headers.len();
            headers.extend_from_slice(&buffer[..header_bytes_in_read]);
            return Ok(classify_response_headers(&headers[..header_end]));
        }
        if headers.len() + read > MAX_HTTP_RESPONSE_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HTTP response headers exceed size limit",
            ));
        }
        headers.extend_from_slice(&buffer[..read]);
    }
}

fn classify_response_headers(headers: &[u8]) -> bool {
    let Ok(headers) = std::str::from_utf8(headers) else {
        return false;
    };
    let mut lines = headers.split("\r\n");
    let Some(status_line) = lines.next() else {
        return false;
    };
    let mut status = status_line.split_whitespace();
    let (Some(version), Some(code)) = (status.next(), status.next()) else {
        return false;
    };
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || code.len() != 3
        || !code.bytes().all(|byte| byte.is_ascii_digit())
        || !lines.all(is_valid_header)
    {
        return false;
    }
    code.starts_with('2')
}

fn is_valid_header(header: &str) -> bool {
    let Some((name, _)) = header.split_once(':') else {
        return false;
    };
    !name.is_empty()
        && !name
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

#[cfg(test)]
mod tests {
    use std::{io, net::Ipv4Addr, time::Duration};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time,
    };

    use super::{MAX_HTTP_RESPONSE_HEADER_BYTES, probe_http};

    #[tokio::test]
    async fn probe_collects_a_fragmented_status_line_and_headers_before_classifying() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("readiness listener");
        let port = listener.local_addr().expect("listener address").port();
        let probe = tokio::spawn(probe_http(
            Ipv4Addr::LOCALHOST,
            port,
            "/ready",
            Duration::from_secs(1),
        ));
        let (mut stream, _) = listener.accept().await.expect("readiness connection");
        read_request_headers(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 ")
            .await
            .expect("partial status line");
        time::sleep(Duration::from_millis(50)).await;
        assert!(!probe.is_finished(), "probe must wait for complete headers");
        stream
            .write_all(b"204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("remaining headers");
        assert!(probe.await.expect("probe task").expect("complete response"));
    }

    #[tokio::test]
    async fn probe_times_out_for_incomplete_headers_without_classifying_them() {
        let (port, server) = incomplete_response_server(b"HTTP/1.1 200 OK\r\nContent").await;

        let result = probe_http(
            Ipv4Addr::LOCALHOST,
            port,
            "/ready",
            Duration::from_millis(100),
        )
        .await;
        server.abort();

        assert_eq!(
            result
                .expect_err("incomplete response must not classify")
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn probe_rejects_oversized_incomplete_headers_within_the_bound() {
        let response = vec![b'x'; MAX_HTTP_RESPONSE_HEADER_BYTES + 1];
        let (port, server) = incomplete_response_server(&response).await;

        let result = probe_http(Ipv4Addr::LOCALHOST, port, "/ready", Duration::from_secs(1)).await;
        server.abort();

        assert_eq!(
            result.expect_err("oversized headers must fail").kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn probe_treats_malformed_complete_responses_as_not_ready() {
        let (port, server) =
            incomplete_response_server(b"HTTP/1.1 200 OK\r\nMalformed\r\n\r\n").await;

        let result = probe_http(Ipv4Addr::LOCALHOST, port, "/ready", Duration::from_secs(1)).await;
        server.abort();

        assert!(!result.expect("malformed complete response is actionable"));
    }

    async fn incomplete_response_server(response: &[u8]) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("readiness listener");
        let port = listener.local_addr().expect("listener address").port();
        let response = response.to_vec();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("readiness connection");
            read_request_headers(&mut stream).await;
            let _ = stream.write_all(&response).await;
            std::future::pending::<()>().await;
        });
        (port, server)
    }

    async fn read_request_headers(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 256];
        loop {
            let read = stream.read(&mut buffer).await.expect("readiness request");
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return;
            }
        }
    }
}
