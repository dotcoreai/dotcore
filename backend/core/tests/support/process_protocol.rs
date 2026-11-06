//! Carries test-only settings and commands from the integration-test harness to one backend
//! subprocess it starts, for example releasing a held simulated model transition.
//!
//! Settings travel once, as one serialized environment variable read at startup. Commands travel
//! afterwards as one JSON line each on the subprocess's standard input, and the subprocess
//! answers each on standard output with a prefixed acknowledgment naming the same command, so
//! acknowledgments never mix with its startup protocol or log output.

use std::{
    io::{BufRead, Write},
    path::PathBuf,
    process::ChildStdin,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use dotcore::{
    composition::BackendTestSupport,
    testing::{ManualClock, SimulatedInferenceTestSupport},
};
use dotcore_domain::clock::Clock;
use dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport;
use serde::{Deserialize, Serialize};

use super::{ProcessTestControls, node_liveness::ManagerClock};

/// Environment variable carrying the serialized [`ProcessSettings`] of one subprocess.
pub(super) const PROCESS_SETTINGS: &str = "DOTCORE_TEST_PROCESS_SETTINGS";

/// Prefix of every acknowledgment line a subprocess writes to standard output.
const ACKNOWLEDGMENT_PREFIX: &str = "DOTCORE_TEST_ACK ";

/// Time a subprocess has to acknowledge one command; longer than any command waits inside it.
const ACKNOWLEDGMENT_DEADLINE: Duration = Duration::from_secs(30);

/// Time a clock advance waits for live nodes to renew and expiring nodes to fall silent.
const ADVANCE_DEADLINE: Duration = Duration::from_secs(20);

const ACKNOWLEDGMENT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Command the harness sends to a running backend subprocess.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessCommand {
    /// Lets simulated model preparation continue after the test observed download activity.
    ReleasePreparationGate,

    /// Lets the simulated workload become ready after the test observed loading.
    ReleaseReadinessGate,

    /// Moves the manager's manual clock `by` ahead once every node but `expiring` has renewed
    /// and every `expiring` node has fallen silent.
    AdvanceClock { by: Duration, expiring: Vec<String> },
}

/// Static test settings of one backend subprocess, decided by the harness before it starts.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct ProcessSettings {
    capability_max_age_millis: Option<u64>,
    held_inference_stream_events: Option<usize>,
    local_development_store_path: Option<(String, String)>,
    application_endpoint_port: Option<u16>,
    model_transition_gates: bool,
    manual_clock: bool,
    native_launcher: Option<PathBuf>,
    manager_private_port: Option<u16>,
    liveness_silence_bound_millis: Option<u64>,
    view_republish_interval_millis: Option<u64>,
    node_action_lease_millis: Option<u64>,
}

impl ProcessSettings {
    /// Settings for a subprocess started under `controls`, with native targets using
    /// `native_launcher` and a manager's site-default private listener on
    /// `manager_private_port` when given.
    pub(super) fn new(
        controls: &ProcessTestControls,
        native_launcher: Option<PathBuf>,
        manager_private_port: Option<u16>,
    ) -> Self {
        let owned = |(first, second): (&str, &str)| (first.to_owned(), second.to_owned());
        Self {
            capability_max_age_millis: controls.capability_max_age.map(millis),
            held_inference_stream_events: controls.held_inference_stream_events,
            local_development_store_path: controls.local_development_store_path.map(owned),
            application_endpoint_port: controls.application_endpoint_port,
            model_transition_gates: controls.model_transition_gates,
            manual_clock: controls.manual_clock,
            native_launcher,
            manager_private_port,
            liveness_silence_bound_millis: controls.liveness_silence_bound.map(millis),
            view_republish_interval_millis: controls.view_republish_interval.map(millis),
            node_action_lease_millis: controls.node_action_lease.map(millis),
        }
    }

    /// Serializes these settings for [`PROCESS_SETTINGS`].
    pub(super) fn encode(&self) -> String {
        serde_json::to_string(self).expect("encode subprocess test settings")
    }

    /// Reads the settings the harness gave this subprocess.
    pub(super) fn from_environment() -> Self {
        let encoded = std::env::var(PROCESS_SETTINGS).expect("subprocess test settings");
        serde_json::from_str(&encoded).expect("decode subprocess test settings")
    }

    /// Reserved port a manager subprocess binds its site-default private listener on, in place
    /// of the fixed default port concurrent tests would share.
    pub(super) fn manager_private_port(&self) -> Option<u16> {
        self.manager_private_port
    }

    /// Manual clock a manager subprocess composes its backend with, when these settings ask for
    /// one; the subprocess keeps a clone to advance it on the harness's commands.
    pub(super) fn manual_clock(&self) -> Option<ManualClock> {
        self.manual_clock.then(ManualClock::new)
    }

    /// Backend controls of a development backend adjusted by these settings, reading
    /// `manual_clock` when given and system time otherwise.
    ///
    /// Simulated models keep their representative acquisition activity with zero-duration steps
    /// and zero ready and stop delays, overriding the site scenario, so lifecycle tests wait on no
    /// simulated time; tests observe intermediate transitions through the transition gates.
    pub(super) fn test_support(&self, manual_clock: Option<ManualClock>) -> BackendTestSupport {
        let mut runtime = BackendTestSupport::development()
            .simulated_runtime
            .expect("development simulated runtime")
            .with_representative_startup(Duration::ZERO)
            .with_lifecycle_delays(Duration::ZERO, Duration::ZERO);
        if let Some(port) = self.application_endpoint_port {
            runtime = runtime.with_application_endpoint_port(port);
        }
        if self.model_transition_gates {
            runtime = runtime.with_model_transition_gates();
        }
        BackendTestSupport {
            simulated_runtime: Some(runtime),
            target_capability_max_age: self.capability_max_age_millis.map(Duration::from_millis),
            simulated_inference: self
                .held_inference_stream_events
                .map(SimulatedInferenceTestSupport::blocked_after_stream_events),
            local_development_store_paths: self
                .local_development_store_path
                .clone()
                .into_iter()
                .collect(),
            native_launcher: self.native_launcher.clone(),
            clock: manual_clock.map(|clock| Arc::new(clock) as Arc<dyn Clock>),
            metric_recorder: None,
            liveness_silence_bound: self
                .liveness_silence_bound_millis
                .map(Duration::from_millis),
            view_republish_interval: self
                .view_republish_interval_millis
                .map(Duration::from_millis),
            node_heartbeat_interval: None,
            manager_silence_bound: None,
            node_action_lease: self.node_action_lease_millis.map(Duration::from_millis),
        }
    }

    /// Answers the harness's commands on standard input for the rest of this subprocess's life,
    /// acting on the controls the backend runs with: `runtime` for simulated runtime gates and
    /// `manager_clock` for a manager's manual clock.
    ///
    /// A plain thread reads standard input so a blocked read never holds the backend's runtime
    /// open; it ends when the harness closes standard input or the process exits.
    pub(super) fn serve_commands(
        &self,
        runtime: Option<SimulatedRuntimeTestSupport>,
        manager_clock: Option<ManagerClock>,
    ) {
        let gates = self.model_transition_gates.then_some(runtime).flatten();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let request: CommandRequest =
                    serde_json::from_str(&line.expect("subprocess command line"))
                        .expect("decode subprocess command");
                let result = execute(request.command, gates.as_ref(), manager_clock.as_ref());
                let acknowledgment = serde_json::to_string(&CommandAcknowledgment {
                    id: request.id,
                    result,
                })
                .expect("encode subprocess command acknowledgment");
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "{ACKNOWLEDGMENT_PREFIX}{acknowledgment}")
                    .expect("subprocess command acknowledgment");
                stdout
                    .flush()
                    .expect("flush subprocess command acknowledgment");
            }
        });
    }
}

/// Harness end of one subprocess's command channel.
#[derive(Debug)]
pub(super) struct CommandChannel {
    input: ChildStdin,
    next_id: u64,
}

impl CommandChannel {
    pub(super) fn new(input: ChildStdin) -> Self {
        Self { input, next_id: 1 }
    }

    /// Sends `command` and waits for its acknowledgment among the subprocess's standard-output
    /// `lines`; fails with the subprocess's refusal, or with the lines seen when no
    /// acknowledgment arrives within the deadline.
    pub(super) async fn send(
        &mut self,
        command: ProcessCommand,
        lines: &mpsc::Receiver<String>,
    ) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::to_string(&CommandRequest {
            id,
            command: command.clone(),
        })
        .expect("encode subprocess command");
        writeln!(self.input, "{request}")
            .and_then(|()| self.input.flush())
            .map_err(|error| format!("send {command:?} to the subprocess: {error}"))?;

        let deadline = Instant::now() + ACKNOWLEDGMENT_DEADLINE;
        let mut seen = Vec::new();
        loop {
            while let Ok(line) = lines.try_recv() {
                if let Some(acknowledgment) = line.strip_prefix(ACKNOWLEDGMENT_PREFIX) {
                    let acknowledgment: CommandAcknowledgment =
                        serde_json::from_str(acknowledgment)
                            .expect("decode subprocess command acknowledgment");
                    if acknowledgment.id == id {
                        return acknowledgment.result.map_err(|reason| {
                            format!("the subprocess refused {command:?}: {reason}")
                        });
                    }
                }
                seen.push(line);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the subprocess did not acknowledge {command:?} within \
                     {ACKNOWLEDGMENT_DEADLINE:?}; stdout since sending: {seen:?}"
                ));
            }
            tokio::time::sleep(ACKNOWLEDGMENT_POLL_INTERVAL).await;
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CommandRequest {
    id: u64,
    command: ProcessCommand,
}

#[derive(Debug, Serialize, Deserialize)]
struct CommandAcknowledgment {
    id: u64,
    result: Result<(), String>,
}

/// Runs one harness command against the controls this subprocess runs with.
pub fn execute(
    command: ProcessCommand,
    gates: Option<&SimulatedRuntimeTestSupport>,
    manager_clock: Option<&ManagerClock>,
) -> Result<(), String> {
    let gates = || gates.ok_or_else(|| "model transition gates are not enabled".to_owned());
    match command {
        ProcessCommand::ReleasePreparationGate => gates()?.release_preparation_gate(),
        ProcessCommand::ReleaseReadinessGate => gates()?.release_readiness_gate(),
        ProcessCommand::AdvanceClock { by, expiring } => manager_clock
            .ok_or_else(|| "the manager does not run on a manual clock".to_owned())?
            .advance(by, &expiring, ADVANCE_DEADLINE)?,
    }
    Ok(())
}

/// Whole milliseconds of a harness duration, as the settings carry them.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("subprocess setting milliseconds")
}
