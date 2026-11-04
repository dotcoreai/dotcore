//! A fake inference API for `dotc` process tests, answering every request on a loopback port with
//! one scripted answer and recording each request line.

use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use serde_json::json;

/// A running fake, stopped on drop.
pub struct FakeInference {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeInference {
    /// Serves a successful model listing of `routes`.
    pub fn with_routes(routes: &[&str]) -> Self {
        let models = routes
            .iter()
            .map(|route| json!({ "id": route, "object": "model", "created": 0 }))
            .collect::<Vec<_>>();
        Self::answering(
            200,
            &json!({ "object": "list", "data": models }).to_string(),
        )
    }

    /// Answers every request with `status` and `body`.
    pub fn answering(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake inference API");
        let address = listener.local_addr().expect("fake inference address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let response = format!(
            "HTTP/1.1 {status} Scripted\r\nContent-Type: application/json\r\nContent-Length: \
             {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        let thread = thread::spawn({
            let requests = requests.clone();
            let stopped = stopped.clone();
            move || {
                for stream in listener.incoming() {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    let stream = stream.expect("accept fake inference connection");
                    answer(stream, &response, &requests);
                }
            }
        });

        Self {
            address,
            requests,
            stopped,
            thread: Some(thread),
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The origin a client is pointed at.
    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The OpenAI-compatible base URL a coding agent receives.
    pub fn base_url(&self) -> String {
        format!("{}/v1", self.origin())
    }

    /// The request lines received so far, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Drop for FakeInference {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Wakes the blocking accept so the thread sees the stop.
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads one request head, records its request line and writes `response`.
fn answer(stream: TcpStream, response: &str, requests: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("read request line");

    let mut header = String::new();
    while reader.read_line(&mut header).expect("read header") > 0 && header != "\r\n" {
        header.clear();
    }

    // Recorded before answering, so a client that got its answer sees its request recorded.
    requests
        .lock()
        .expect("requests")
        .push(request_line.trim_end().to_owned());
    reader
        .get_mut()
        .write_all(response.as_bytes())
        .expect("write fake inference response");
}
