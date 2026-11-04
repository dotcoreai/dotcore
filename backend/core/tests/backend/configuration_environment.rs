//! Subprocess-only backend configuration environment diagnostic coverage.

use std::time::{Duration, Instant};

use crate::support;

const SNAPSHOT_HELPER: &str = "DOTCORE_CONFIGURATION_SNAPSHOT_HELPER";

#[test]
fn configuration_snapshot_survives_environment_removal_through_prepare_and_serve() {
    let fixture = support::temporary_fast_fixture();
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .arg("--ignored")
        .arg("--exact")
        .arg("configuration_environment::configuration_snapshot_prepare_serve_helper")
        .env_clear()
        .env(SNAPSHOT_HELPER, "1")
        .env("DOTCORE_SITE_DIR", fixture.as_os_str())
        // Master catalog wiring: the subprocess configuration points at the real master catalog.
        .env(
            "DOTCORE_CATALOG_DIR",
            support::workspace_root().join("catalog"),
        )
        .env("DOTCORE_CACHE_DIR", fixture.join("cache"))
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173");
    if let Some(certificates) = std::env::var_os("SSL_CERT_FILE") {
        command.env("SSL_CERT_FILE", certificates);
    }
    let output = command_output_with_deadline(command, Duration::from_secs(10));

    assert!(
        output.status.success(),
        "snapshot helper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
#[ignore = "subprocess helper"]
async fn configuration_snapshot_prepare_serve_helper() {
    assert_eq!(std::env::var(SNAPSHOT_HELPER).as_deref(), Ok("1"));
    let configuration = dotcore_backend::composition::BackendConfiguration::from_environment()
        .expect("configuration snapshot");
    for variable in [
        "DOTCORE_SITE_DIR",
        "DOTCORE_CATALOG_DIR",
        "DOTCORE_CACHE_DIR",
        "DOTCORE_MANAGEMENT_ADDR",
        "DOTCORE_INFERENCE_ADDR",
        "DOTCORE_FRONTEND_ORIGIN",
    ] {
        // This helper is the only test in an isolated subprocess, so mutating its environment is safe.
        unsafe { std::env::remove_var(variable) };
    }
    let (shutdown, signal) = tokio::sync::oneshot::channel();
    let backend = dotcore_backend::composition::Backend::prepare(configuration, async move {
        let _ = signal.await;
    })
    .await
    .expect("prepare from captured snapshot");
    let address = backend.addresses().management.expect("management listener");
    let mut task = tokio::spawn(backend.serve());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut task => {
                    let error = result.expect("Backend task").expect_err("Backend must remain active");
                    panic!("Backend failed before transport dispatch: {error}");
                }
                result = tokio::net::TcpStream::connect(address) => match result {
                    Ok(_) => break,
                    Err(_) => tokio::task::yield_now().await,
                },
            }
        }
    })
    .await
    .expect("management connection deadline");
    shutdown.send(()).expect("Backend shutdown");
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("Backend join deadline")
        .expect("Backend task")
        .expect("Backend result");
}

fn command_output_with_deadline(
    mut command: std::process::Command,
    timeout: Duration,
) -> std::process::Output {
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("start subprocess");
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("inspect subprocess").is_some() {
            return child.wait_with_output().expect("subprocess output");
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out subprocess");
            let output = child
                .wait_with_output()
                .expect("timed-out subprocess output");
            panic!(
                "subprocess exceeded {timeout:?}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
