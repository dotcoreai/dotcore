//! A real site manager started with `dotc server start` on a site folder a test prepared.
//!
//! Tests that need a running manager, for real deployments or real joins, start one on loopback
//! ports and reach it at the management address its startup protocol prints. The process is
//! stopped when the value is dropped, and its diagnostics stay readable for failing assertions.

use std::{
    io::{BufRead, BufReader},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use super::command::isolated_dotc;

/// How long the server may take to print its startup protocol.
const STARTUP_DEADLINE: Duration = Duration::from_secs(30);

/// How long the server may take to exit after it is asked to stop.
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// Delay between two checks of the server process.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// A running site manager, stopped when dropped.
pub struct ManagerServer {
    child: Child,
    address: SocketAddr,
    stderr: PathBuf,
}

impl ManagerServer {
    /// Starts the manager of the site under `root`, laid out as `site/`, `master-catalog/` and
    /// `cache/`, with its management and inference APIs on free loopback ports and its private
    /// listener, the one joining nodes dial, at `private_address`.
    pub fn start(root: &Path, private_address: SocketAddr) -> Self {
        for directory in ["models", "engines", "applications"] {
            std::fs::create_dir_all(root.join("master-catalog").join(directory))
                .expect("catalog directory");
        }
        std::fs::create_dir_all(root.join("cache")).expect("cache directory");
        let stderr = root.join("server.stderr");
        let mut child = dotc_with_certificates()
            .args(["server", "start"])
            .env("DOTCORE_SITE_DIR", root.join("site"))
            .env("DOTCORE_CATALOG_DIR", root.join("master-catalog"))
            .env("DOTCORE_CACHE_DIR", root.join("cache"))
            .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
            .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
            .env("DOTCORE_PRIVATE_ADDR", private_address.to_string())
            .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr).expect("server stderr file"))
            .spawn()
            .expect("start dotc server");
        let address = management_address(&mut child, &stderr);

        Self {
            child,
            address,
            stderr,
        }
    }

    /// The management address to pass as `--manager` or `DOTCORE_MANAGEMENT_ADDR`.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// `message` followed by the server's diagnostics, for a failing assertion.
    pub fn failure(&self, message: &str) -> String {
        format!("{message}\nserver stderr:\n{}", read_stderr(&self.stderr))
    }
}

impl Drop for ManagerServer {
    fn drop(&mut self) {
        // SAFETY: the child is owned by this value and not yet reaped, so its PID is still valid.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + EXIT_DEADLINE;
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Reads the startup protocol from the server's stdout and returns its management address.
fn management_address(child: &mut Child, stderr: &Path) -> SocketAddr {
    let stdout = child.stdout.take().expect("server stdout");
    let (lines, received) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + STARTUP_DEADLINE;
    let mut output = Vec::new();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("server status") {
            panic!(
                "server exited before startup with {status}; stdout: {output:?}; stderr:\n{}",
                read_stderr(stderr)
            );
        }
        let Ok(line) = received.recv_timeout(POLL_INTERVAL) else {
            continue;
        };
        if let Some(address) = line.strip_prefix("DOTCORE_MANAGEMENT_ADDRESS=http://") {
            return address.parse().expect("management address");
        }
        output.push(line);
    }
    panic!(
        "server did not print its management address within {STARTUP_DEADLINE:?}; stdout: {output:?}; stderr:\n{}",
        read_stderr(stderr)
    );
}

fn read_stderr(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Builds a `dotc` command with an empty environment, except the CA bundle the test host may need.
pub fn dotc_with_certificates() -> Command {
    let mut command = isolated_dotc();
    if let Some(certificates) = std::env::var_os("SSL_CERT_FILE") {
        command.env("SSL_CERT_FILE", certificates);
    }
    command
}
