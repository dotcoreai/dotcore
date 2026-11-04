//! Reads whole HTTP requests in test fixture servers, so a fixture never replies or closes while
//! part of the request is still unread.
//!
//! Closing a connection that still holds unread data makes the kernel send a reset instead of a
//! clean close, and the client then loses the response even though it was written in full.

use tokio::io::AsyncReadExt as _;

/// Consumes one whole HTTP request, headers and `Content-Length` body, and returns the body.
/// Returns an empty body when the client closes before finishing the request.
pub(crate) async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut scratch = [0_u8; 1024];
    loop {
        let read = stream.read(&mut scratch).await.expect("upstream request");
        if read == 0 {
            return Vec::new();
        }
        request.extend_from_slice(&scratch[..read]);
        let Some(headers_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = headers_end + 4;
        if request.len() - body_start >= content_length(&request[..body_start]) {
            return request.split_off(body_start);
        }
    }
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}
