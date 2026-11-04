//! Reports what artifact preparation does to the caller that asked for it, one step at a time.
//!
//! Each preparation step a deployment runs on its hosting node, such as downloading model weights,
//! building a Nix package or making an image available to Docker, reports when it starts, each
//! line its tool prints, when it waits for another preparation of the same artifact, and how it
//! ends. A step that finds its artifact already prepared reports only that, naming the artifact,
//! so callers can say what each step decided without repeating it on every preparation.

use std::{
    error::Error,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use dotcore_domain::clock::Clock;
use tokio::time::Instant;

/// Receives the output of preparation calls, for example to keep one log per deployment and step.
/// A preparation call made without an observer reports nothing.
#[derive(Clone, Default)]
pub struct PreparationOutput {
    sink: Option<Arc<Sink>>,
}

impl PreparationOutput {
    /// Reports every event to `observer`, timed by `clock`.
    pub fn new(clock: Arc<dyn Clock>, observer: PreparationObserver) -> Self {
        Self {
            sink: Some(Arc::new(Sink { clock, observer })),
        }
    }

    /// Reports nothing.
    pub fn discard() -> Self {
        Self::default()
    }

    /// Returns the output of one step of this preparation, which prepares the artifact
    /// `artifact` describes.
    pub(crate) fn step(&self, step: PreparationStep, artifact: impl Into<String>) -> StepOutput {
        StepOutput {
            sink: self.sink.clone(),
            step,
            artifact: artifact.into(),
            started: Arc::new(Mutex::new(None)),
        }
    }
}

/// Receives each event of a preparation, in the order its step reported them.
pub type PreparationObserver = Arc<dyn Fn(PreparationEvent) + Send + Sync>;

/// One preparation step of a deployment on its hosting node. Each step prepares one artifact, so
/// callers can keep the output of each step apart.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PreparationStep {
    /// Downloads and verifies the model and auxiliary artifacts of a model deployment.
    ModelDownload,

    /// Builds the engine's Nix package.
    EngineBuild,

    /// Makes the engine's image available to Docker, by running its loader or pulling it.
    EngineImage,

    /// Builds one application service's Nix package.
    ServiceBuild { service: String },

    /// Makes one application service's image available to Docker, by running its loader or
    /// pulling it.
    ServiceImage { service: String },
}

/// One event a preparation step reported, with the wall time it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparationEvent {
    pub step: PreparationStep,
    pub time: SystemTime,
    pub kind: PreparationEventKind,
}

/// What a preparation step reported. A step either reports `AlreadyPrepared` alone, or
/// `Started` first, once per preparation call, then `Finished` or `Failed` to end it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparationEventKind {
    /// The step found work to do for its artifact, such as a Nix store path, an image reference or
    /// the model weights it downloads.
    Started { artifact: String },

    /// The step found its artifact already prepared and did nothing.
    AlreadyPrepared { artifact: String },

    /// The step waits while another preparation of the same artifact, from this or another
    /// process, holds it.
    Waiting,

    /// One line the step's tool printed, on the stream it printed it to.
    Line { stream: OutputStream, text: String },

    /// The step prepared its artifact, `duration` after it started.
    Finished { duration: Duration },

    /// The step failed with the error it reported.
    Failed { error: String },
}

/// Stream a tool printed a line to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,

    Stderr,
}

/// Output of one step of a preparation call. Clones share whether the step started, so
/// concurrent work of one step reports it started once.
#[derive(Clone)]
pub struct StepOutput {
    sink: Option<Arc<Sink>>,
    step: PreparationStep,
    artifact: String,
    started: Arc<Mutex<Option<Instant>>>,
}

impl StepOutput {
    /// Returns step output that reports nothing, for preparation work no caller observes.
    pub(crate) fn discard() -> Self {
        PreparationOutput::discard().step(PreparationStep::ModelDownload, String::new())
    }

    /// Reports one line the step's tool printed, after reporting the step started if it had not.
    pub fn line(&self, stream: OutputStream, text: impl Into<String>) {
        let text = text.into();
        self.report(Some(PreparationEventKind::Line { stream, text }));
    }

    /// Whether the step's events reach an observer, so work that only feeds them can be skipped.
    pub(crate) fn is_observed(&self) -> bool {
        self.sink.is_some()
    }

    /// Reports the step started, unless it already did.
    pub(crate) fn start(&self) {
        self.report(None);
    }

    /// Reports the step waits for another preparation of its artifact.
    pub(crate) fn waiting(&self) {
        self.report(Some(PreparationEventKind::Waiting));
    }

    /// Reports how the step ended: finished when it started and succeeded, already prepared when
    /// it succeeded without starting, failed with the error and its causes when it failed for any
    /// reason but cancellation, which ends a started step as failed and an unstarted one silently.
    pub(crate) fn complete<T, E: Error>(
        &self,
        result: &Result<T, E>,
        cancelled: impl Fn(&E) -> bool,
    ) {
        match result {
            Ok(_) => self.succeed(),
            Err(error) if cancelled(error) => self.end(PreparationEventKind::Failed {
                error: error_chain(error),
            }),
            Err(error) => self.report(Some(PreparationEventKind::Failed {
                error: error_chain(error),
            })),
        }
    }

    /// Reports the step started if it had not, then `kind`.
    fn report(&self, kind: Option<PreparationEventKind>) {
        let Some(sink) = &self.sink else {
            return;
        };
        // Held while reporting, so events of one step reach the observer in order.
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if started.is_none() {
            *started = Some(sink.clock.now());
            self.emit(
                sink,
                PreparationEventKind::Started {
                    artifact: self.artifact.clone(),
                },
            );
        }
        if let Some(kind) = kind {
            self.emit(sink, kind);
        }
    }

    /// Reports a started step finished after the time it took, and an unstarted one already
    /// prepared.
    fn succeed(&self) {
        let Some(sink) = &self.sink else {
            return;
        };
        let started = self
            .started
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let kind = match *started {
            Some(start) => PreparationEventKind::Finished {
                duration: sink.clock.now().saturating_duration_since(start),
            },
            None => PreparationEventKind::AlreadyPrepared {
                artifact: self.artifact.clone(),
            },
        };
        self.emit(sink, kind);
    }

    /// Reports `kind` only if the step started.
    fn end(&self, kind: PreparationEventKind) {
        let Some(sink) = &self.sink else {
            return;
        };
        let started = self
            .started
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if started.is_some() {
            self.emit(sink, kind);
        }
    }

    fn emit(&self, sink: &Sink, kind: PreparationEventKind) {
        (sink.observer)(PreparationEvent {
            step: self.step.clone(),
            time: sink.clock.wall_now(),
            kind,
        });
    }
}

/// Renders an error with each of its causes, joined with `: `.
fn error_chain(error: &dyn Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        rendered.push_str(": ");
        rendered.push_str(&cause.to_string());
        source = cause.source();
    }
    rendered
}

struct Sink {
    clock: Arc<dyn Clock>,
    observer: PreparationObserver,
}

#[cfg(test)]
mod tests {
    use super::{test_support::RecordedOutput, *};

    #[test]
    fn a_step_reports_started_once_before_its_lines_and_its_duration_when_it_finishes() {
        let recorded = RecordedOutput::new();
        let step = recorded
            .output()
            .step(PreparationStep::EngineBuild, "/nix/store/abc-engine");

        step.line(OutputStream::Stderr, "first");
        recorded.clock.advance(Duration::from_secs(3));
        step.clone().line(OutputStream::Stdout, "second");
        step.complete(&Ok::<_, std::fmt::Error>(()), |_| false);

        assert_eq!(
            recorded.kinds(),
            [
                (
                    PreparationStep::EngineBuild,
                    started("/nix/store/abc-engine")
                ),
                (
                    PreparationStep::EngineBuild,
                    line(OutputStream::Stderr, "first")
                ),
                (
                    PreparationStep::EngineBuild,
                    line(OutputStream::Stdout, "second")
                ),
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Finished {
                        duration: Duration::from_secs(3)
                    }
                ),
            ]
        );
        assert_eq!(
            recorded.times(),
            [
                RecordedOutput::START,
                RecordedOutput::START,
                RecordedOutput::START + Duration::from_secs(3),
                RecordedOutput::START + Duration::from_secs(3),
            ]
        );
    }

    #[test]
    fn an_unstarted_step_reports_success_as_already_prepared_and_cancellation_silently() {
        let recorded = RecordedOutput::new();
        let output = recorded.output();

        output
            .step(PreparationStep::ModelDownload, "weights")
            .complete(&Ok::<_, std::fmt::Error>(()), |_| false);
        output
            .step(PreparationStep::EngineImage, "image")
            .complete(&Err::<(), _>(failure("cancelled", None)), |_| true);

        assert_eq!(
            recorded.kinds(),
            [(
                PreparationStep::ModelDownload,
                PreparationEventKind::AlreadyPrepared {
                    artifact: "weights".to_owned()
                }
            )]
        );
    }

    #[test]
    fn a_failure_starts_the_step_and_reports_the_error_with_its_causes() {
        let recorded = RecordedOutput::new();
        let output = recorded.output();

        output.step(PreparationStep::EngineBuild, "build").complete(
            &Err::<(), _>(failure(
                "nix build failed",
                Some(failure("builder exited with 1", None)),
            )),
            |_| false,
        );
        let started_step = output.step(PreparationStep::EngineImage, "image");
        started_step.waiting();
        started_step.complete(&Err::<(), _>(failure("cancelled", None)), |_| true);

        assert_eq!(
            recorded.kinds(),
            [
                (PreparationStep::EngineBuild, started("build")),
                (
                    PreparationStep::EngineBuild,
                    PreparationEventKind::Failed {
                        error: "nix build failed: builder exited with 1".to_owned()
                    }
                ),
                (PreparationStep::EngineImage, started("image")),
                (PreparationStep::EngineImage, PreparationEventKind::Waiting),
                (
                    PreparationStep::EngineImage,
                    PreparationEventKind::Failed {
                        error: "cancelled".to_owned()
                    }
                ),
            ]
        );
    }

    #[test]
    fn discarded_output_reports_nothing() {
        let step = StepOutput::discard();

        step.line(OutputStream::Stdout, "unseen");
        step.complete(&Err::<(), _>(failure("failed", None)), |_| false);

        assert!(!step.is_observed());
    }

    /// Test failure with an optional cause, rendered as its message.
    #[derive(Debug, thiserror::Error)]
    #[error("{message}")]
    struct Failure {
        message: &'static str,
        #[source]
        cause: Option<Box<Failure>>,
    }

    fn failure(message: &'static str, cause: Option<Failure>) -> Failure {
        Failure {
            message,
            cause: cause.map(Box::new),
        }
    }

    fn line(stream: OutputStream, text: &str) -> PreparationEventKind {
        PreparationEventKind::Line {
            stream,
            text: text.to_owned(),
        }
    }

    fn started(artifact: &str) -> PreparationEventKind {
        PreparationEventKind::Started {
            artifact: artifact.to_owned(),
        }
    }
}

/// Recorder of preparation output shared by the unit tests of every preparation module.
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        sync::{Arc, Mutex},
        time::SystemTime,
    };

    use super::{PreparationEvent, PreparationEventKind, PreparationOutput, PreparationStep};

    /// Observer recording every event of the preparations it is given to, on a virtual clock that
    /// starts at [`RecordedOutput::START`].
    pub(crate) struct RecordedOutput {
        pub(crate) clock: dotcore_domain::clock::VirtualClock,
        events: Arc<Mutex<Vec<PreparationEvent>>>,
    }

    impl RecordedOutput {
        pub(crate) const START: SystemTime = SystemTime::UNIX_EPOCH;

        pub(crate) fn new() -> Self {
            Self {
                clock: dotcore_domain::clock::VirtualClock::starting_at(Self::START),
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub(crate) fn output(&self) -> PreparationOutput {
            let events = self.events.clone();
            PreparationOutput::new(
                Arc::new(self.clock.clone()),
                Arc::new(move |event| events.lock().expect("recorded events").push(event)),
            )
        }

        /// Returns each recorded event's step and kind, in order.
        pub(crate) fn kinds(&self) -> Vec<(PreparationStep, PreparationEventKind)> {
            self.events
                .lock()
                .expect("recorded events")
                .iter()
                .map(|event| (event.step.clone(), event.kind.clone()))
                .collect()
        }

        /// Returns each recorded event's time, in order.
        pub(crate) fn times(&self) -> Vec<SystemTime> {
            self.events
                .lock()
                .expect("recorded events")
                .iter()
                .map(|event| event.time)
                .collect()
        }
    }
}
