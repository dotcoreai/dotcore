//! Server command delegation and its process-lifecycle boundary.

use std::{
    future::Future,
    io::{self, Write},
    pin::Pin,
    sync::Arc,
};

use anyhow::{Context, Result};
use dotcore_backend::composition::PathOverrides;
use tokio::sync::oneshot::{self, error::TryRecvError};
use tracing::{error, info, warn};

type ShutdownSignal = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type BackendShutdown = Pin<Box<dyn Future<Output = ()> + Send>>;
type ShutdownOutcome = oneshot::Receiver<Result<(), ShutdownFailure>>;

const MANAGEMENT_ADDRESS_MARKER: &str = "DOTCORE_MANAGEMENT_ADDRESS=";
const INFERENCE_ADDRESS_MARKER: &str = "DOTCORE_INFERENCE_ADDRESS=";
const PRIVATE_ADDRESS_MARKER: &str = "DOTCORE_PRIVATE_ADDRESS=";

trait SignalRegistrar {
    fn ctrl_c(&self) -> ShutdownSignal;

    fn sigterm(&self) -> Result<ShutdownSignal>;
}

struct TokioSignalRegistrar;

/// Starts the backend with the command-owned shutdown future. Each given path in `paths`
/// replaces its variable.
pub async fn start(paths: &PathOverrides) -> Result<()> {
    info!(
        event_kind = "cli_process_start_requested",
        "CLI backend process start requested"
    );
    let result = start_with(
        &TokioSignalRegistrar,
        |shutdown, mut shutdown_outcome| async move {
            let configuration =
                dotcore_backend::composition::BackendConfiguration::from_environment_with_paths(
                    paths,
                )
                .map_err(anyhow::Error::from)?;
            let Some(backend) = prepare_or_shutdown(
                async {
                    let result =
                        dotcore_backend::composition::Backend::prepare(configuration, shutdown)
                            .await
                            .map_err(anyhow::Error::from);
                    if let Err(error) = &result {
                        error!(
                            event_kind = "cli_backend_preparation_failed",
                            error = %error,
                            "CLI backend preparation failed"
                        );
                    } else {
                        info!(
                            event_kind = "cli_backend_prepared",
                            "CLI backend preparation completed"
                        );
                    }
                    result
                },
                &mut shutdown_outcome,
            )
            .await?
            else {
                return Ok(());
            };
            if shutdown_received(&mut shutdown_outcome)? {
                return Ok(());
            }
            let addresses = backend.addresses();
            write_startup_protocol(addresses).map_err(|error| {
                error!(
                    event_kind = "cli_startup_protocol_failed",
                    error = %error,
                    "CLI startup protocol could not be written"
                );
                anyhow::Error::from(error)
            })?;
            info!(
                event_kind = "cli_startup_protocol_written",
                management_address = ?addresses.management,
                inference_address = ?addresses.inference,
                private_address = ?addresses.private,
                "CLI startup protocol written"
            );
            backend.serve().await.map_err(anyhow::Error::from)?;
            receive_shutdown_outcome(shutdown_outcome.await)
        },
    )
    .await;
    if result.is_ok() {
        info!(
            event_kind = "cli_shutdown_completed",
            "CLI backend process shutdown completed"
        );
    }
    result?;
    Ok(())
}

fn write_startup_protocol(addresses: dotcore_backend::server::ServerAddresses) -> io::Result<()> {
    let stdout = io::stdout();
    write_startup_protocol_to(&mut stdout.lock(), addresses)
}

/// Writes the heading, then one marker per public listener; a node serving only its private
/// channel reports that address instead.
fn write_startup_protocol_to(
    output: &mut impl Write,
    addresses: dotcore_backend::server::ServerAddresses,
) -> io::Result<()> {
    writeln!(output, "Dotcore server starting")?;
    if let Some(address) = addresses.management {
        writeln!(output, "{MANAGEMENT_ADDRESS_MARKER}http://{address}")?;
    }
    if let Some(address) = addresses.inference {
        writeln!(output, "{INFERENCE_ADDRESS_MARKER}http://{address}")?;
    }
    if let (None, None, Some(address)) =
        (addresses.management, addresses.inference, addresses.private)
    {
        writeln!(output, "{PRIVATE_ADDRESS_MARKER}https://{address}")?;
    }
    output.flush()
}

impl SignalRegistrar for TokioSignalRegistrar {
    fn ctrl_c(&self) -> ShutdownSignal {
        Box::pin(async {
            tokio::signal::ctrl_c()
                .await
                .context("failed to receive Ctrl-C/SIGINT")
        })
    }

    fn sigterm(&self) -> Result<ShutdownSignal> {
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("failed to register SIGTERM handler")?;
        Ok(sigterm_shutdown(signal))
    }
}

fn sigterm_shutdown(mut signal: tokio::signal::unix::Signal) -> ShutdownSignal {
    Box::pin(async move { receive_sigterm(signal.recv()).await })
}

async fn receive_sigterm(signal: impl Future<Output = Option<()>>) -> Result<()> {
    signal.await.context("failed to receive SIGTERM")
}

fn shutdown(registrar: &impl SignalRegistrar) -> Result<ShutdownSignal> {
    let ctrl_c = registrar.ctrl_c();
    let sigterm = registrar.sigterm()?;

    Ok(Box::pin(async move {
        tokio::select! {
            result = ctrl_c => result,
            result = sigterm => result,
        }
    }))
}

async fn start_with<R, S>(
    registrar: &R,
    start_backend: impl FnOnce(BackendShutdown, ShutdownOutcome) -> S,
) -> Result<()>
where
    R: SignalRegistrar,
    S: Future<Output = Result<()>>,
{
    let shutdown = shutdown(registrar)?;
    let (registered, registered_waiter) = oneshot::channel();
    let (shutdown_sender, shutdown_waiter) = oneshot::channel();
    let (shutdown_outcome_sender, shutdown_outcome) = oneshot::channel();
    let (completed_sender, completed) = oneshot::channel();
    let signal_task = tokio::spawn(async move {
        let mut shutdown = shutdown;
        let mut registered = Some(registered);
        let result = std::future::poll_fn(|context| -> std::task::Poll<Result<()>> {
            let result = shutdown.as_mut().poll(context);
            match result {
                std::task::Poll::Pending => {
                    if let Some(registered) = registered.take() {
                        let _ = registered.send(Ok(()));
                    }
                    std::task::Poll::Pending
                }
                std::task::Poll::Ready(Ok(())) => {
                    if let Some(registered) = registered.take() {
                        let _ = registered.send(Ok(()));
                    }
                    std::task::Poll::Ready(Ok(()))
                }
                std::task::Poll::Ready(Err(error)) => {
                    if let Some(registered) = registered.take() {
                        let _ = registered.send(Err(error));
                        return std::task::Poll::Ready(Ok(()));
                    }
                    std::task::Poll::Ready(Err(error))
                }
            }
        })
        .await;
        match result {
            Ok(()) => {
                info!(
                    event_kind = "cli_shutdown_requested",
                    "CLI shutdown signal received"
                );
                let _ = shutdown_outcome_sender.send(Ok(()));
                let _ = completed_sender.send(Ok(()));
            }
            Err(error) => {
                warn!(event_kind = "cli_shutdown_signal_failed", error = %error, "CLI shutdown signal failed");
                let failure = ShutdownFailure::new(error);
                let _ = shutdown_outcome_sender.send(Err(failure.clone()));
                let _ = completed_sender.send(Err(failure));
            }
        }
        let _ = shutdown_sender.send(());
    });

    match registered_waiter
        .await
        .context("signal handler stopped before registration")?
    {
        Ok(()) => {}
        Err(error) => {
            let _ = signal_task.await;
            return Err(error);
        }
    }
    let backend_result = start_backend(
        Box::pin(async move {
            let _ = shutdown_waiter.await;
        }),
        shutdown_outcome,
    )
    .await;

    if let Err(error) = backend_result {
        error!(event_kind = "cli_process_failed", error = %error, "CLI backend process failed");
        signal_task.abort();
        let _ = signal_task.await;
        return Err(error);
    }

    receive_shutdown_outcome(completed.await)?;
    signal_task.await.context("signal handler task failed")?;
    Ok(())
}

async fn prepare_or_shutdown<T>(
    preparation: impl Future<Output = Result<T>>,
    shutdown_outcome: &mut ShutdownOutcome,
) -> Result<Option<T>> {
    tokio::select! {
        biased;
        outcome = shutdown_outcome => {
            receive_shutdown_outcome(outcome)?;
            Ok(None)
        }
        prepared = preparation => Ok(Some(prepared?)),
    }
}

fn receive_shutdown_outcome(
    outcome: std::result::Result<Result<(), ShutdownFailure>, oneshot::error::RecvError>,
) -> Result<()> {
    Ok(outcome.context("signal handler stopped before shutdown")??)
}

/// A cloneable handoff retains one signal failure for each shutdown observer.
#[derive(Clone, Debug, thiserror::Error)]
#[error("server signal handler failed")]
struct ShutdownFailure {
    #[source]
    source: Arc<anyhow::Error>,
}

impl ShutdownFailure {
    fn new(source: anyhow::Error) -> Self {
        Self {
            source: Arc::new(source),
        }
    }

    #[cfg(test)]
    fn diagnostic(&self) -> &anyhow::Error {
        &self.source
    }
}

fn shutdown_received(shutdown_outcome: &mut ShutdownOutcome) -> Result<bool> {
    match shutdown_outcome.try_recv() {
        Ok(outcome) => {
            receive_shutdown_outcome(Ok(outcome))?;
            Ok(true)
        }
        Err(TryRecvError::Empty) => Ok(false),
        Err(TryRecvError::Closed) => Err(anyhow::anyhow!("signal handler stopped before shutdown")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context as TaskContext, Poll},
    };

    use anyhow::anyhow;

    use super::{
        BackendShutdown, ShutdownOutcome, ShutdownSignal, SignalRegistrar, prepare_or_shutdown,
        shutdown, start_with,
    };

    #[tokio::test]
    async fn shutdown_selects_ctrl_c_when_sigterm_is_pending() {
        let registrar = TestSignalRegistrar::new(Signal::Ready, Signal::Pending);

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            shutdown(&registrar).unwrap(),
        )
        .await
        .expect("Ctrl-C shutdown completion")
        .expect("Ctrl-C shutdown result");

        assert_eq!(registrar.registered(), ["ctrl_c", "sigterm"]);
    }

    #[tokio::test]
    async fn shutdown_selects_sigterm_when_ctrl_c_is_pending() {
        let registrar = TestSignalRegistrar::new(Signal::Pending, Signal::Ready);

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            shutdown(&registrar).unwrap(),
        )
        .await
        .expect("SIGTERM shutdown completion")
        .expect("SIGTERM shutdown result");

        assert_eq!(registrar.registered(), ["ctrl_c", "sigterm"]);
    }

    #[test]
    fn shutdown_surfaces_sigterm_registration_errors() {
        let registrar = TestSignalRegistrar::new(Signal::Pending, Signal::Failure);

        let error = match shutdown(&registrar) {
            Ok(_) => panic!("SIGTERM registration failure"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), "injected SIGTERM registration failure");
        assert_eq!(registrar.registered(), ["ctrl_c", "sigterm"]);
    }

    #[tokio::test]
    async fn signal_registration_failure_is_a_command_error() {
        let registrar = TestSignalRegistrar::new(Signal::Pending, Signal::Failure);

        let error = start_with(&registrar, successful_server)
            .await
            .expect_err("signal registration failure");

        assert_eq!(error.to_string(), "injected SIGTERM registration failure");
        assert_command_error(error);
        assert_eq!(registrar.registered(), ["ctrl_c", "sigterm"]);
    }

    #[tokio::test]
    async fn signal_registration_is_polled_before_backend_start() {
        let registrar = TestSignalRegistrar::new(Signal::Ready, Signal::Pending);

        start_with(&registrar, |shutdown, _| async {
            assert!(registrar.ctrl_c_was_polled());
            shutdown.await;
            Ok(())
        })
        .await
        .expect("early SIGINT shutdown");
    }

    #[tokio::test]
    async fn immediate_sigterm_failure_is_a_registration_error() {
        let registrar = TestSignalRegistrar::new(Signal::Pending, Signal::Closed);

        let error = start_with(&registrar, successful_server)
            .await
            .expect_err("SIGTERM receipt failure");

        assert_eq!(format!("{error:#}"), "failed to receive SIGTERM");
        assert_command_error(error);
        assert_eq!(registrar.registered(), ["ctrl_c", "sigterm"]);
    }

    #[tokio::test]
    async fn immediate_signal_registration_failure_prevents_backend_start() {
        let registrar = TestSignalRegistrar::new(Signal::Failure, Signal::Pending);
        let started = Arc::new(AtomicBool::new(false));
        let started_backend = Arc::clone(&started);

        let error = start_with(&registrar, move |_, _| {
            started_backend.store(true, Ordering::Relaxed);
            async { Ok(()) }
        })
        .await
        .expect_err("immediate signal registration failure");

        assert_eq!(error.to_string(), "injected signal failure");
        assert_command_error(error);
        assert!(!started.load(Ordering::Relaxed));
        assert_eq!(registrar.registered(), ["ctrl_c", "sigterm"]);
    }

    #[tokio::test]
    async fn delayed_signal_receipt_error_after_registration_is_a_command_error() {
        let registrar = TestSignalRegistrar::new(Signal::Delayed, Signal::Pending);
        let running_registrar = registrar.clone();
        let started = Arc::new(AtomicBool::new(false));
        let started_backend = Arc::clone(&started);

        let running = tokio::spawn(async move {
            start_with(&running_registrar, move |shutdown, _| async move {
                started_backend.store(true, Ordering::Relaxed);
                shutdown.await;
                Ok(())
            })
            .await
        });
        wait_until(&started).await;
        registrar.trigger(DelayedSignal::Failure);

        let error = running
            .await
            .expect("server task")
            .expect_err("delayed signal receipt failure");

        assert_eq!(
            format!("{error:#}"),
            "server signal handler failed: injected delayed signal failure"
        );
        assert_command_error(error);
    }

    #[tokio::test]
    async fn shutdown_handoffs_retain_the_original_contextual_error() {
        let registrar = TestSignalRegistrar::new(Signal::Delayed, Signal::Pending);
        let running_registrar = registrar.clone();
        let started = Arc::new(AtomicBool::new(false));
        let started_backend = Arc::clone(&started);
        let running = tokio::spawn(async move {
            start_with(&running_registrar, move |shutdown, _| async move {
                started_backend.store(true, Ordering::Relaxed);
                shutdown.await;
                Ok(())
            })
            .await
        });
        wait_until(&started).await;
        registrar.trigger(DelayedSignal::Failure);

        let error = running
            .await
            .expect("server task")
            .expect_err("delayed signal receipt failure");
        let handoff = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<super::ShutdownFailure>())
            .expect("shutdown failure handoff");

        assert_eq!(
            format!("{:#}", handoff.diagnostic()),
            "injected delayed signal failure"
        );
        assert_eq!(
            handoff.diagnostic().backtrace().status(),
            capture_backtrace_status()
        );
    }

    #[test]
    fn shutdown_outcome_retains_contextual_and_concrete_sources() {
        let source = anyhow::Error::new(std::io::Error::other("injected signal I/O failure"))
            .context("injected signal operation");
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender
            .send(Err(super::ShutdownFailure::new(source)))
            .expect("shutdown outcome receiver");

        let error = super::receive_shutdown_outcome(receiver.blocking_recv())
            .expect_err("shutdown outcome failure");

        assert_eq!(
            format!("{error:#}"),
            "server signal handler failed: injected signal operation: injected signal I/O failure"
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "server signal handler failed")
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.to_string() == "injected signal operation")
        );
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|cause| cause.to_string() == "injected signal I/O failure")
        }));
        assert_eq!(error.backtrace().status(), capture_backtrace_status());
    }

    #[tokio::test]
    async fn signal_during_preparation_cancels_resources_without_serving() {
        let registrar = TestSignalRegistrar::new(Signal::Delayed, Signal::Pending);
        let running_registrar = registrar.clone();
        let served = Arc::new(AtomicBool::new(false));
        let served_backend = Arc::clone(&served);
        let preparation_started = Arc::new(AtomicBool::new(false));
        let preparation_started_backend = Arc::clone(&preparation_started);
        let preparation_dropped = Arc::new(AtomicBool::new(false));
        let preparation_dropped_backend = Arc::clone(&preparation_dropped);
        let listener = TcpListener::bind("127.0.0.1:0").expect("partial listener");
        let listener_address = listener.local_addr().expect("partial listener address");

        let running = tokio::spawn(async move {
            start_with(
                &running_registrar,
                move |_, mut shutdown_outcome| async move {
                    let preparation = PendingPreparation {
                        _listener: listener,
                        started: preparation_started_backend,
                        dropped: preparation_dropped_backend,
                    };
                    let Some(()) = prepare_or_shutdown(preparation, &mut shutdown_outcome).await?
                    else {
                        return Ok(());
                    };
                    served_backend.store(true, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
        });
        wait_until(&preparation_started).await;
        registrar.trigger(DelayedSignal::Ready);

        running
            .await
            .expect("server task")
            .expect("signal during preparation");

        assert!(!served.load(Ordering::Relaxed));
        assert!(preparation_dropped.load(Ordering::Relaxed));
        TcpListener::bind(listener_address).expect("released partial listener");
    }

    #[derive(Clone, Copy)]
    enum Signal {
        Ready,

        Pending,

        Failure,

        Closed,

        Delayed,
    }

    #[derive(Clone, Copy)]
    enum DelayedSignal {
        Ready,

        Failure,
    }

    #[derive(Clone)]
    struct TestSignalRegistrar {
        registered: Arc<Mutex<Vec<&'static str>>>,
        ctrl_c_polled: Arc<AtomicBool>,
        delayed_signal: tokio::sync::watch::Sender<Option<DelayedSignal>>,
        ctrl_c: Signal,
        sigterm: Signal,
    }

    impl TestSignalRegistrar {
        fn new(ctrl_c: Signal, sigterm: Signal) -> Self {
            let (delayed_signal, _) = tokio::sync::watch::channel(None);
            Self {
                registered: Arc::default(),
                ctrl_c_polled: Arc::default(),
                delayed_signal,
                ctrl_c,
                sigterm,
            }
        }

        fn registered(&self) -> Vec<&'static str> {
            self.registered.lock().expect("registered signals").clone()
        }

        fn ctrl_c_was_polled(&self) -> bool {
            self.ctrl_c_polled.load(Ordering::Relaxed)
        }

        fn trigger(&self, signal: DelayedSignal) {
            self.delayed_signal
                .send(Some(signal))
                .expect("delayed signal receiver");
        }
    }

    impl SignalRegistrar for TestSignalRegistrar {
        fn ctrl_c(&self) -> ShutdownSignal {
            self.registered
                .lock()
                .expect("registered signals")
                .push("ctrl_c");
            let ctrl_c_polled = Arc::clone(&self.ctrl_c_polled);
            let signal = self.ctrl_c;
            let delayed_signal = self.delayed_signal.subscribe();
            Box::pin(async move {
                ctrl_c_polled.store(true, Ordering::Relaxed);
                signal_future(signal, delayed_signal).await
            })
        }

        fn sigterm(&self) -> anyhow::Result<ShutdownSignal> {
            self.registered
                .lock()
                .expect("registered signals")
                .push("sigterm");
            match self.sigterm {
                Signal::Failure => Err(anyhow!("injected SIGTERM registration failure")),
                Signal::Closed => Ok(Box::pin(async {
                    super::receive_sigterm(std::future::ready(None)).await
                })),
                signal => Ok(signal_future(signal, self.delayed_signal.subscribe())),
            }
        }
    }

    fn signal_future(
        signal: Signal,
        mut delayed_signal: tokio::sync::watch::Receiver<Option<DelayedSignal>>,
    ) -> ShutdownSignal {
        match signal {
            Signal::Ready => Box::pin(std::future::ready(Ok(()))),
            Signal::Pending => Box::pin(std::future::pending()),
            Signal::Failure => {
                Box::pin(std::future::ready(Err(anyhow!("injected signal failure"))))
            }
            Signal::Closed => Box::pin(std::future::pending()),
            Signal::Delayed => Box::pin(async move {
                delayed_signal
                    .changed()
                    .await
                    .expect("delayed signal sender");
                match delayed_signal
                    .borrow_and_update()
                    .expect("delayed signal value")
                {
                    DelayedSignal::Ready => Ok(()),
                    DelayedSignal::Failure => Err(anyhow!("injected delayed signal failure")),
                }
            }),
        }
    }

    async fn successful_server(
        shutdown: BackendShutdown,
        _shutdown_outcome: ShutdownOutcome,
    ) -> anyhow::Result<()> {
        shutdown.await;
        Ok(())
    }

    fn assert_command_error(error: anyhow::Error) {
        assert_eq!(
            super::super::report_failure(error, false),
            std::process::ExitCode::from(1)
        );
    }

    /// Status of a backtrace captured now: `Captured` only when the environment enables
    /// backtraces (`RUST_LIB_BACKTRACE` or `RUST_BACKTRACE`), so retention checks hold either way.
    fn capture_backtrace_status() -> std::backtrace::BacktraceStatus {
        std::backtrace::Backtrace::capture().status()
    }

    async fn wait_until(value: &AtomicBool) {
        tokio::time::timeout(std::time::Duration::from_millis(50), async {
            while !value.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("asynchronous operation started");
    }

    struct PendingPreparation {
        _listener: TcpListener,
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Future for PendingPreparation {
        type Output = anyhow::Result<()>;

        fn poll(self: std::pin::Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Self::Output> {
            self.started.store(true, Ordering::Relaxed);
            Poll::Pending
        }
    }

    impl Drop for PendingPreparation {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn startup_protocol_writes_the_exact_ordered_lines_and_flushes_once() {
        let mut output = FlushRecordingWriter::default();
        super::write_startup_protocol_to(&mut output, public_addresses()).expect("startup output");

        assert_eq!(
            output.bytes,
            b"Dotcore server starting\nDOTCORE_MANAGEMENT_ADDRESS=http://127.0.0.1:50051\nDOTCORE_INFERENCE_ADDRESS=http://127.0.0.1:50052\n"
        );
        assert_eq!(output.flushes, 1);
    }

    #[test]
    fn startup_protocol_preserves_write_and_flush_failures() {
        for failure in [Failure::Write, Failure::Flush] {
            let error = super::write_startup_protocol_to(
                &mut FailingWriter { failure },
                public_addresses(),
            )
            .expect_err("startup output failure");
            assert_eq!(error.to_string(), format!("injected {failure:?} failure"));
        }
    }

    #[test]
    fn startup_protocol_reports_only_the_private_address_without_public_listeners() {
        let mut output = FlushRecordingWriter::default();
        super::write_startup_protocol_to(
            &mut output,
            dotcore_backend::server::ServerAddresses {
                management: None,
                inference: None,
                private: Some("127.0.0.2:50053".parse().expect("private address")),
            },
        )
        .expect("startup output");

        assert_eq!(
            output.bytes,
            b"Dotcore server starting\nDOTCORE_PRIVATE_ADDRESS=https://127.0.0.2:50053\n"
        );
        assert_eq!(output.flushes, 1);
    }

    fn public_addresses() -> dotcore_backend::server::ServerAddresses {
        dotcore_backend::server::ServerAddresses {
            management: Some("127.0.0.1:50051".parse().expect("management address")),
            inference: Some("127.0.0.1:50052".parse().expect("inference address")),
            private: None,
        }
    }

    #[derive(Default)]
    struct FlushRecordingWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl std::io::Write for FlushRecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Failure {
        Write,

        Flush,
    }

    struct FailingWriter {
        failure: Failure,
    }

    impl std::io::Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            match self.failure {
                Failure::Write => Err(std::io::Error::other("injected Write failure")),
                Failure::Flush => Ok(bytes.len()),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("injected Flush failure"))
        }
    }
}
