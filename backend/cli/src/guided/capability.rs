//! The inference and applications runtime steps of guided `dotc site init`, reset and node join:
//! probe this host once, summarize what was detected, and let the operator check the
//! accelerators and runtimes the node offers.
//!
//! Every detected accelerator is listed on its own and checked, so a node can offer some of its
//! GPUs. In a development workspace, simulated inference is listed beside them as an alternative,
//! followed by a choice of the hardware to simulate. An absent capability, for example `nvidia` on a Mac, is a normal outcome: it is
//! summarized as "not detected" and never raised as a warning. A capability that is present but
//! failing reads "detected but unusable" with a one-line reason and cannot be checked. Checking
//! nothing is valid: the node then runs no local models, or no applications. The full probe
//! diagnostics go to the debug log.

use std::collections::BTreeSet;

use dotcore::composition::DEV_SOURCE_FLAKE_ENV;
use dotcore_node_hardware::{MachineDescription, hardware::UnavailableReason};
use dotcore_proto::dotcore::observation::v1::AcceleratorFacts;
use dotcore_site::initial::{
    inputs::nvidia_accelerator_device_ids,
    probe::{InitialSiteProbeError, InitialSiteProbeReport, InitialSiteProber},
    request::{InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest},
    target_name::model_target_display_name,
};

use crate::terminal::{Message, MultiSelectPrompt, SelectItem, Terminal};

/// Probe outcomes for the capabilities the operator still has to choose.
pub(crate) struct CapabilityRecommendations {
    nvidia: Option<Probe<Vec<DetectedDevice>>>,
    apple: Option<Probe<Vec<DetectedDevice>>>,
    docker: Option<Probe<()>>,
}

impl CapabilityRecommendations {
    /// Probes NVIDIA and Apple inference only when the inference choice is unresolved, and Docker
    /// applications only when the applications choice is. Every host is probed for every vendor;
    /// an absent capability is an outcome, not an error.
    pub(crate) async fn collect(
        prober: &InitialSiteProber,
        probe_inference: bool,
        probe_docker: bool,
    ) -> Self {
        let mut recommendations = Self {
            nvidia: None,
            apple: None,
            docker: None,
        };
        if probe_inference {
            recommendations.nvidia = Some(
                probe(
                    prober,
                    "nvidia",
                    &NVIDIA_SUBJECT,
                    InitialInferenceRuntime::Nvidia,
                    nvidia_devices,
                )
                .await,
            );
            recommendations.apple = Some(
                probe(
                    prober,
                    "apple",
                    &APPLE_SUBJECT,
                    InitialInferenceRuntime::Apple,
                    apple_devices,
                )
                .await,
            );
        }
        if probe_docker {
            let request = InitialSiteRequest::new([], [InitialApplicationRuntime::Docker]);
            recommendations.docker = Some(
                classified(prober.probe(&request).await, "docker", &DOCKER_SUBJECT).map(|_| ()),
            );
        }
        recommendations
    }
}

/// The inference capabilities the operator offers, with the NVIDIA GPUs they picked when they
/// left some out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InferencePick {
    /// Inference capabilities the node offers, possibly none.
    pub(crate) inference: BTreeSet<InitialInferenceRuntime>,

    /// CDI selectors of the picked NVIDIA GPUs when only some were picked; `None` keeps every
    /// GPU, the same as the command line without device selectors.
    pub(crate) nvidia_cdi_selectors: Option<Vec<String>>,

    /// Simulated hardware the operator picked, in order; `None` when simulated isn't offered or
    /// the request's default applies.
    pub(crate) simulated_hardware: Option<Vec<String>>,
}

/// Returns whether `dotc` runs in a development workspace, the only place guided collection
/// offers simulated hardware; the workspace environment names its development source there.
pub(crate) fn in_dev_env() -> bool {
    std::env::var_os(DEV_SOURCE_FLAKE_ENV).is_some_and(|value| !value.is_empty())
}

/// Returns the pre-resolved inference capabilities, or asks the operator to check the
/// accelerators the node offers. In a development workspace (`development`), simulated inference
/// is listed too, as an alternative to the real GPUs, and then the hardware to simulate is asked.
pub(crate) fn collect_inference(
    inference: Option<BTreeSet<InitialInferenceRuntime>>,
    development: bool,
    terminal: &mut dyn Terminal,
    recommendations: &CapabilityRecommendations,
) -> anyhow::Result<InferencePick> {
    if let Some(inference) = inference {
        return Ok(InferencePick {
            inference,
            nvidia_cdi_selectors: None,
            simulated_hardware: None,
        });
    }

    let nvidia = recommendations
        .nvidia
        .as_ref()
        .expect("unresolved inference must have an NVIDIA probe");
    let apple = recommendations
        .apple
        .as_ref()
        .expect("unresolved inference must have an Apple probe");
    terminal.message(&Message::Info(summary(
        "Accelerator detection",
        &[("nvidia", nvidia.state()), ("apple", apple.state())],
    )))?;

    // Each listed item maps back to what it offers: one device of a vendor.
    let mut offers = Vec::new();
    let mut items = Vec::new();
    for (vendor, inference, probe) in [
        ("nvidia", InitialInferenceRuntime::Nvidia, nvidia),
        ("apple", InitialInferenceRuntime::Apple, apple),
    ] {
        match probe {
            Probe::Ready(devices) => {
                for device in devices {
                    items.push(SelectItem::new(&device.name).with_description(vendor));
                    offers.push(Some((inference, Some(device.selector.clone()))));
                }
            }
            Probe::Unusable(reason) => {
                items.push(
                    SelectItem::new(vendor)
                        .with_unavailable_reason(format!("detected but unusable: {reason}")),
                );
                offers.push(None);
            }
            Probe::Absent(_) => {}
        }
    }

    // The simulated item offers no device; it stands in for every real GPU.
    if development {
        items.push(SelectItem::new("simulated").with_description(SIMULATED_INFERENCE_DESCRIPTION));
        offers.push(Some((InitialInferenceRuntime::Simulated, None)));
    }
    if items.is_empty() {
        terminal.message(&Message::Info(
            "No inference: this node runs no local models.".to_owned(),
        ))?;
        return Ok(inference_pick(nvidia, &[]));
    }

    let checked = (0..items.len())
        .filter(|index| matches!(&offers[*index], Some((runtime, _)) if *runtime != InitialInferenceRuntime::Simulated))
        .collect::<Vec<_>>();
    let prompt = MultiSelectPrompt::new("Inference", items).with_checked(checked);
    let picked = loop {
        let picked = terminal
            .multi_select(&prompt)?
            .into_iter()
            .filter_map(|index| offers[index].clone())
            .collect::<Vec<_>>();
        let simulated = picked
            .iter()
            .any(|(runtime, _)| *runtime == InitialInferenceRuntime::Simulated);
        if simulated && picked.len() > 1 {
            terminal.message(&Message::Warning(
                "Simulated hardware replaces the real GPUs: check one or the other.".to_owned(),
            ))?;
            continue;
        }
        break picked;
    };

    let simulated = picked
        .iter()
        .any(|(runtime, _)| *runtime == InitialInferenceRuntime::Simulated);
    if picked.is_empty() {
        terminal.message(&Message::Info(
            "No inference: this node runs no local models.".to_owned(),
        ))?;
    }
    let mut pick = inference_pick(nvidia, &picked);
    if simulated {
        pick.simulated_hardware = Some(collect_simulated_hardware(terminal)?);
    }
    Ok(pick)
}

/// Asks which hardware simulated models run on: named GPUs, put on one simulated host, or one
/// whole machine. An invalid mix is explained and asked again.
fn collect_simulated_hardware(terminal: &mut dyn Terminal) -> anyhow::Result<Vec<String>> {
    let choices = MachineDescription::simulated_choices();
    let items = choices.iter().map(|choice| {
        let description = if choice.whole_machine {
            format!("{}; pick alone", choice.summary)
        } else {
            choice.summary.clone()
        };
        SelectItem::new(choice.name).with_description(description)
    });
    let default = choices
        .iter()
        .position(|choice| choice.name == InitialSiteRequest::DEFAULT_SIMULATED_HARDWARE);
    let prompt = MultiSelectPrompt::new("Hardware to simulate", items).with_checked(default);

    loop {
        let picked = terminal
            .multi_select(&prompt)?
            .into_iter()
            .map(|index| choices[index].name.to_owned())
            .collect::<Vec<_>>();
        match MachineDescription::simulated(&picked, std::env::consts::ARCH) {
            Ok(_) => return Ok(picked),
            Err(error) => terminal.message(&Message::Warning(format!("{error}.")))?,
        }
    }
}

/// Returns the pre-resolved application capabilities, or asks the operator to check the
/// runtimes the node offers applications on.
pub(crate) fn collect_applications(
    applications: Option<BTreeSet<InitialApplicationRuntime>>,
    terminal: &mut dyn Terminal,
    recommendations: &CapabilityRecommendations,
) -> anyhow::Result<BTreeSet<InitialApplicationRuntime>> {
    if let Some(applications) = applications {
        return Ok(applications);
    }

    let docker = recommendations
        .docker
        .as_ref()
        .expect("unresolved applications must have a Docker probe");
    terminal.message(&Message::Info(summary(
        "Applications detection",
        &[("docker", docker.state())],
    )))?;
    let docker_item = match docker {
        Probe::Ready(()) => SelectItem::new("docker").with_description("detected"),
        Probe::Absent(reason) => {
            SelectItem::new("docker").with_unavailable_reason(format!("not detected: {reason}"))
        }
        Probe::Unusable(reason) => SelectItem::new("docker")
            .with_unavailable_reason(format!("detected but unusable: {reason}")),
    };
    let prompt = MultiSelectPrompt::new(
        "Applications",
        [
            docker_item,
            SelectItem::new("simulated").with_description(SIMULATED_DESCRIPTION),
        ],
    )
    .with_checked(matches!(docker, Probe::Ready(())).then_some(0));

    let applications = terminal
        .multi_select(&prompt)?
        .into_iter()
        .map(|index| match index {
            0 => InitialApplicationRuntime::Docker,
            _ => InitialApplicationRuntime::Simulated,
        })
        .collect::<BTreeSet<_>>();
    if applications.is_empty() {
        terminal.message(&Message::Info(
            "No applications: this node runs no applications.".to_owned(),
        ))?;
    }
    Ok(applications)
}

/// Description of the simulated item, which stands in for hardware during development.
const SIMULATED_DESCRIPTION: &str = "development only";

/// Description of the simulated inference item, whose hardware is asked next.
const SIMULATED_INFERENCE_DESCRIPTION: &str =
    "development only: pick the hardware to simulate next";

/// One detected accelerator the node can offer as a model target.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DetectedDevice {
    /// The name its model target would get, such as `NVIDIA GeForce RTX 5090 · GPU 1`.
    name: String,

    /// The NVIDIA CDI selector naming the GPU, or the Apple GPU's device id.
    selector: String,
}

/// The outcome of probing one capability.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Probe<T> {
    /// The capability is detected and usable, with what it offers.
    Ready(T),

    /// The capability is not on this host; carries the one-line reason.
    Absent(String),

    /// The capability is present but failing; carries the one-line reason.
    Unusable(String),
}

impl<T> Probe<T> {
    fn state(&self) -> &'static str {
        match self {
            Self::Ready(_) => "detected",
            Self::Absent(_) => "not detected",
            Self::Unusable(_) => "detected but unusable",
        }
    }

    fn map<U>(self, ready: impl FnOnce(T) -> U) -> Probe<U> {
        match self {
            Self::Ready(value) => Probe::Ready(ready(value)),
            Self::Absent(reason) => Probe::Absent(reason),
            Self::Unusable(reason) => Probe::Unusable(reason),
        }
    }
}

/// The wording used to describe one probed capability.
struct Subject {
    /// The name used in reasons, for example "NVIDIA".
    name: &'static str,

    /// The driver whose library may be missing or failing, for example "NVIDIA driver".
    driver: &'static str,

    /// What is missing when no device is found, for example "NVIDIA GPU".
    device: &'static str,
}

const NVIDIA_SUBJECT: Subject = Subject {
    name: "NVIDIA",
    driver: "NVIDIA driver",
    device: "NVIDIA GPU",
};

const APPLE_SUBJECT: Subject = Subject {
    name: "Apple",
    driver: "Apple GPU driver",
    device: "Apple GPU",
};

const DOCKER_SUBJECT: Subject = Subject {
    name: "Docker",
    driver: "Docker driver",
    device: "Docker daemon",
};

/// Combines the picked items into capabilities, keeping NVIDIA selectors only when some
/// detected GPUs were left out.
fn inference_pick(
    nvidia: &Probe<Vec<DetectedDevice>>,
    picked: &[(InitialInferenceRuntime, Option<String>)],
) -> InferencePick {
    let selectors = picked
        .iter()
        .filter(|(inference, _)| *inference == InitialInferenceRuntime::Nvidia)
        .filter_map(|(_, selector)| selector.clone())
        .collect::<Vec<_>>();
    let every_gpu = matches!(nvidia, Probe::Ready(devices) if devices.len() == selectors.len());
    InferencePick {
        inference: picked.iter().map(|(inference, _)| *inference).collect(),
        nvidia_cdi_selectors: (!selectors.is_empty() && !every_gpu).then_some(selectors),
        simulated_hardware: None,
    }
}

/// Renders every probed capability, for example
/// "Accelerator detection: nvidia not detected, apple detected".
fn summary(prefix: &str, states: &[(&str, &str)]) -> String {
    let states = states
        .iter()
        .map(|(capability, state)| format!("{capability} {state}"))
        .collect::<Vec<_>>();
    format!("{prefix}: {}", states.join(", "))
}

/// Probes one accelerator vendor and lists its devices under the names their model targets
/// would get; `devices` returns the probed devices' selectors and facts, or why they are
/// unusable.
async fn probe(
    prober: &InitialSiteProber,
    capability: &'static str,
    subject: &Subject,
    inference: InitialInferenceRuntime,
    devices: DeviceSelection,
) -> Probe<Vec<DetectedDevice>> {
    let request = InitialSiteRequest::new([inference], []);
    let report = match classified(prober.probe(&request).await, capability, subject) {
        Probe::Ready(report) => report,
        Probe::Absent(reason) => return Probe::Absent(reason),
        Probe::Unusable(reason) => return Probe::Unusable(reason),
    };
    let (selectors, facts) = match devices(&request, &report) {
        Ok(devices) => devices,
        Err(reason) => return Probe::Unusable(reason),
    };
    Probe::Ready(
        selectors
            .into_iter()
            .map(|selector| DetectedDevice {
                name: model_target_display_name(inference, std::slice::from_ref(&selector), &facts),
                selector,
            })
            .collect(),
    )
}

/// Returns a successful probe's device selectors and facts, or why they are unusable.
type DeviceSelection = fn(
    &InitialSiteRequest,
    &InitialSiteProbeReport,
) -> Result<(Vec<String>, Vec<AcceleratorFacts>), String>;

/// Returns one full-GPU selector per probed NVIDIA GPU, as the site would declare them.
fn nvidia_devices(
    request: &InitialSiteRequest,
    report: &InitialSiteProbeReport,
) -> Result<(Vec<String>, Vec<AcceleratorFacts>), String> {
    let nvidia = report.nvidia().ok_or("NVIDIA probe failed")?;
    let selectors = nvidia_accelerator_device_ids(request, report).map_err(|error| {
        tracing::debug!(
            diagnostic = %format!("{:#}", anyhow::Error::from(error)),
            "NVIDIA devices could not be selected"
        );
        "NVIDIA GPUs cannot each be named by one device selector"
    })?;
    Ok((selectors, nvidia.facts().to_vec()))
}

/// Returns the observed Apple GPU.
fn apple_devices(
    _request: &InitialSiteRequest,
    report: &InitialSiteProbeReport,
) -> Result<(Vec<String>, Vec<AcceleratorFacts>), String> {
    let apple = report.apple().ok_or("Apple probe failed")?;
    Ok((
        vec![apple.device_id().to_owned()],
        vec![apple.facts().clone()],
    ))
}

/// Classifies a probe result, logging the full error chain of a failure to the debug log only.
fn classified(
    result: Result<InitialSiteProbeReport, InitialSiteProbeError>,
    capability: &'static str,
    subject: &Subject,
) -> Probe<InitialSiteProbeReport> {
    let error = match result {
        Ok(report) => return Probe::Ready(report),
        Err(error) => error,
    };

    let outcome = classify(&error, subject);
    tracing::debug!(
        capability,
        outcome = ?outcome,
        diagnostic = %format!("{:#}", anyhow::Error::from(error)),
        "capability probe did not succeed"
    );
    outcome
}

/// Classifies a probe failure by its stable reason, never by the error text.
fn classify<T>(error: &InitialSiteProbeError, subject: &Subject) -> Probe<T> {
    let reason = match error {
        InitialSiteProbeError::NvidiaUnavailable { reason, .. }
        | InitialSiteProbeError::AppleUnavailable { reason, .. } => *reason,
        InitialSiteProbeError::DockerUnavailable(_) => {
            return Probe::Absent("no reachable Docker daemon found".to_owned());
        }
        _ => return Probe::Unusable(format!("{} probe failed", subject.name)),
    };
    match reason {
        UnavailableReason::Unsupported => Probe::Absent(format!(
            "{} is not supported on this platform",
            subject.name
        )),
        UnavailableReason::MissingLibrary => {
            Probe::Absent(format!("no {} library found", subject.driver))
        }
        UnavailableReason::NoDevice => Probe::Absent(format!("no {} found", subject.device)),
        UnavailableReason::PermissionDenied => Probe::Unusable(format!(
            "{} present but permission was denied",
            subject.driver
        )),
        UnavailableReason::CollectionFailed => {
            Probe::Unusable(format!("{} present but collection failed", subject.driver))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use dotcore_node_hardware::hardware::UnavailableReason;
    use dotcore_site::initial::{
        probe::InitialSiteProber,
        request::{InitialApplicationRuntime, InitialInferenceRuntime},
    };

    use super::{
        CapabilityRecommendations, InferencePick, collect_applications, collect_inference,
    };
    use crate::{
        guided::test_support::{
            apple_prober, available_prober, detailed_unavailable_nvidia_prober, two_gpu_prober,
            unavailable_docker_prober, unavailable_nvidia_prober, unusable_nvidia_prober,
        },
        terminal::{
            Message, MultiSelectPrompt, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal,
            SelectItem,
        },
    };

    const GPU: &str = "NVIDIA RTX PRO 6000 Blackwell";

    /// The hardware prompt that follows a checked simulated item, with RTX 5090 checked.
    fn hardware_prompt() -> ScriptedPrompt {
        let gpu = |name: &str, summary: &str| SelectItem::new(name).with_description(summary);
        ScriptedPrompt::MultiSelect(
            MultiSelectPrompt::new(
                "Hardware to simulate",
                [
                    gpu("rtx-4080", "NVIDIA GeForce RTX 4080, 16 GiB"),
                    gpu("rtx-4090", "NVIDIA GeForce RTX 4090, 24 GiB"),
                    gpu("rtx-5090", "NVIDIA GeForce RTX 5090, 32 GiB"),
                    gpu(
                        "rtx-pro-6000",
                        "NVIDIA RTX PRO 6000 Blackwell Workstation Edition, 96 GiB",
                    ),
                    gpu(
                        "gb10",
                        "whole machine: NVIDIA GB10, 122 GiB host memory, linux/arm64; pick alone",
                    ),
                    gpu(
                        "mac-64g",
                        "whole machine: Apple M4 Max, 64 GiB host memory, darwin/arm64; pick alone",
                    ),
                ],
            )
            .with_checked([2]),
        )
    }

    fn simulated_item() -> SelectItem {
        SelectItem::new("simulated")
            .with_description("development only: pick the hardware to simulate next")
    }

    #[test]
    fn every_detected_gpu_is_listed_and_checked() {
        let recommendations = recommendations(&two_gpu_prober());
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::MultiSelectDefault]);

        let pick =
            collect_inference(None, false, &mut terminal, &recommendations).expect("inference");

        assert_eq!(
            pick,
            InferencePick {
                inference: BTreeSet::from([InitialInferenceRuntime::Nvidia]),
                nvidia_cdi_selectors: None,
                simulated_hardware: None,
            }
        );
        assert_eq!(
            terminal.messages(),
            [Message::Info(
                "Accelerator detection: nvidia detected, apple not detected".to_owned()
            )]
        );
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::MultiSelect(
                MultiSelectPrompt::new(
                    "Inference",
                    [
                        SelectItem::new(format!("{GPU} · GPU 0")).with_description("nvidia"),
                        SelectItem::new(format!("{GPU} · GPU 1")).with_description("nvidia"),
                    ],
                )
                .with_checked([0, 1])
            )]
        );
    }

    #[test]
    fn leaving_a_gpu_out_selects_the_others() {
        let recommendations = recommendations(&two_gpu_prober());
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::MultiSelect(vec![1])]);

        let pick =
            collect_inference(None, false, &mut terminal, &recommendations).expect("inference");

        assert_eq!(
            pick,
            InferencePick {
                inference: BTreeSet::from([InitialInferenceRuntime::Nvidia]),
                nvidia_cdi_selectors: Some(vec!["nvidia.com/gpu=gpu1".to_owned()]),
                simulated_hardware: None,
            }
        );
        assert_eq!(terminal.prompts().len(), 1);
    }

    #[test]
    fn a_development_workspace_lists_simulated_unchecked_then_asks_the_hardware() {
        let recommendations = recommendations(&two_gpu_prober());
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::MultiSelect(vec![2]),
            ScriptedAnswer::MultiSelect(vec![1, 0]),
        ]);

        let pick =
            collect_inference(None, true, &mut terminal, &recommendations).expect("inference");

        assert_eq!(
            pick,
            InferencePick {
                inference: BTreeSet::from([InitialInferenceRuntime::Simulated]),
                nvidia_cdi_selectors: None,
                simulated_hardware: Some(vec!["rtx-4080".to_owned(), "rtx-4090".to_owned()]),
            }
        );
        assert_eq!(
            terminal.prompts(),
            [
                ScriptedPrompt::MultiSelect(
                    MultiSelectPrompt::new(
                        "Inference",
                        [
                            SelectItem::new(format!("{GPU} · GPU 0")).with_description("nvidia"),
                            SelectItem::new(format!("{GPU} · GPU 1")).with_description("nvidia"),
                            simulated_item(),
                        ],
                    )
                    .with_checked([0, 1])
                ),
                hardware_prompt(),
            ]
        );
    }

    #[test]
    fn simulated_beside_a_real_gpu_is_asked_again() {
        let recommendations = recommendations(&two_gpu_prober());
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::MultiSelect(vec![0, 2]),
            ScriptedAnswer::MultiSelect(vec![2]),
            ScriptedAnswer::MultiSelectDefault,
        ]);

        let pick =
            collect_inference(None, true, &mut terminal, &recommendations).expect("inference");

        assert_eq!(pick.simulated_hardware, Some(vec!["rtx-5090".to_owned()]));
        assert_eq!(
            terminal.lines(),
            [
                "Accelerator detection: nvidia detected, apple not detected",
                "! Simulated hardware replaces the real GPUs: check one or the other.",
            ]
        );
        assert_eq!(terminal.prompts().len(), 3);
    }

    #[test]
    fn a_whole_machine_beside_other_hardware_is_asked_again() {
        let recommendations = recommendations(&unavailable_nvidia_prober());
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::MultiSelect(vec![0]),
            ScriptedAnswer::MultiSelect(vec![2, 4]),
            ScriptedAnswer::MultiSelect(vec![4]),
        ]);

        let pick =
            collect_inference(None, true, &mut terminal, &recommendations).expect("inference");

        assert_eq!(pick.simulated_hardware, Some(vec!["gb10".to_owned()]));
        assert_eq!(
            terminal.lines(),
            [
                "Accelerator detection: nvidia not detected, apple not detected",
                "! \"gb10\" is a whole machine and can't be combined with other simulated \
                 hardware.",
            ]
        );
        assert_eq!(
            terminal.prompts(),
            [
                ScriptedPrompt::MultiSelect(MultiSelectPrompt::new(
                    "Inference",
                    [simulated_item()]
                )),
                hardware_prompt(),
                hardware_prompt(),
            ]
        );
    }

    #[test]
    fn an_observed_apple_gpu_is_listed_by_chip_and_checked() {
        let recommendations = recommendations(&apple_prober());
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::MultiSelectDefault]);

        let pick =
            collect_inference(None, false, &mut terminal, &recommendations).expect("inference");

        assert_eq!(
            pick.inference,
            BTreeSet::from([InitialInferenceRuntime::Apple])
        );
        assert_eq!(
            terminal.messages(),
            [Message::Info(
                "Accelerator detection: nvidia not detected, apple detected".to_owned()
            )]
        );
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::MultiSelect(
                MultiSelectPrompt::new(
                    "Inference",
                    [SelectItem::new("Apple M3 Max").with_description("apple"),],
                )
                .with_checked([0])
            )]
        );
    }

    #[test]
    fn outside_a_development_workspace_no_accelerator_runs_no_local_models() {
        let recommendations = recommendations(&unavailable_nvidia_prober());
        let mut terminal = ScriptedTerminal::interactive([]);

        let pick =
            collect_inference(None, false, &mut terminal, &recommendations).expect("inference");

        assert_eq!(pick.inference, BTreeSet::new());
        assert_eq!(pick.simulated_hardware, None);
        assert!(terminal.prompts().is_empty());
        assert_eq!(
            terminal.lines(),
            [
                "Accelerator detection: nvidia not detected, apple not detected",
                "No inference: this node runs no local models.",
            ]
        );
    }

    #[test]
    fn leaving_simulated_unchecked_runs_no_local_models() {
        let recommendations = recommendations(&unavailable_nvidia_prober());
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::MultiSelectDefault]);

        let pick =
            collect_inference(None, true, &mut terminal, &recommendations).expect("inference");

        assert_eq!(pick.inference, BTreeSet::new());
        assert_eq!(pick.simulated_hardware, None);
        assert_eq!(
            terminal.lines(),
            [
                "Accelerator detection: nvidia not detected, apple not detected",
                "No inference: this node runs no local models.",
            ]
        );
    }

    #[test]
    fn a_missing_nvidia_library_is_summarized_without_paths_or_warning() {
        let recommendations = recommendations(&detailed_unavailable_nvidia_prober());
        let mut terminal = ScriptedTerminal::interactive([]);

        collect_inference(None, false, &mut terminal, &recommendations).expect("inference");

        assert_eq!(
            terminal.lines(),
            [
                "Accelerator detection: nvidia not detected, apple not detected",
                "No inference: this node runs no local models.",
            ]
        );
    }

    #[test]
    fn a_present_but_failing_nvidia_is_listed_as_unusable() {
        for (reason, line) in [
            (
                UnavailableReason::PermissionDenied,
                "NVIDIA driver present but permission was denied",
            ),
            (
                UnavailableReason::CollectionFailed,
                "NVIDIA driver present but collection failed",
            ),
        ] {
            let recommendations = recommendations(&unusable_nvidia_prober(reason));
            let mut terminal = ScriptedTerminal::interactive([
                ScriptedAnswer::MultiSelect(vec![1]),
                ScriptedAnswer::MultiSelectDefault,
            ]);

            let pick =
                collect_inference(None, true, &mut terminal, &recommendations).expect("inference");

            assert_eq!(
                pick.inference,
                BTreeSet::from([InitialInferenceRuntime::Simulated])
            );
            assert_eq!(
                terminal.prompts(),
                [
                    ScriptedPrompt::MultiSelect(MultiSelectPrompt::new(
                        "Inference",
                        [
                            SelectItem::new("nvidia")
                                .with_unavailable_reason(format!("detected but unusable: {line}")),
                            simulated_item(),
                        ],
                    )),
                    hardware_prompt(),
                ]
            );
            assert_eq!(
                terminal.messages(),
                [Message::Info(
                    "Accelerator detection: nvidia detected but unusable, apple not detected"
                        .to_owned()
                )]
            );
        }
    }

    #[test]
    fn detected_docker_is_checked_and_simulated_applications_can_join_it() {
        let recommendations = recommendations(&available_prober());
        let expected = ScriptedPrompt::MultiSelect(
            MultiSelectPrompt::new(
                "Applications",
                [
                    SelectItem::new("docker").with_description("detected"),
                    SelectItem::new("simulated").with_description("development only"),
                ],
            )
            .with_checked([0]),
        );
        for (answer, applications) in [
            (
                ScriptedAnswer::MultiSelectDefault,
                BTreeSet::from([InitialApplicationRuntime::Docker]),
            ),
            (
                ScriptedAnswer::MultiSelect(vec![0, 1]),
                BTreeSet::from([
                    InitialApplicationRuntime::Simulated,
                    InitialApplicationRuntime::Docker,
                ]),
            ),
        ] {
            let mut terminal = ScriptedTerminal::interactive([answer]);

            assert_eq!(
                collect_applications(None, &mut terminal, &recommendations).expect("applications"),
                applications
            );
            assert_eq!(terminal.prompts(), std::slice::from_ref(&expected));
            assert_eq!(
                terminal.lines(),
                ["Applications detection: docker detected"]
            );
        }
    }

    #[test]
    fn an_unreachable_docker_daemon_cannot_be_checked() {
        let recommendations = recommendations(&unavailable_docker_prober());
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::MultiSelectDefault]);

        let applications =
            collect_applications(None, &mut terminal, &recommendations).expect("applications");

        assert_eq!(applications, BTreeSet::new());
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::MultiSelect(MultiSelectPrompt::new(
                "Applications",
                [
                    SelectItem::new("docker")
                        .with_unavailable_reason("not detected: no reachable Docker daemon found"),
                    SelectItem::new("simulated").with_description("development only"),
                ],
            ))]
        );
        assert_eq!(
            terminal.lines(),
            [
                "Applications detection: docker not detected",
                "No applications: this node runs no applications.",
            ]
        );
    }

    #[test]
    fn resolved_choices_are_returned_without_prompting() {
        let recommendations = recommendations(&available_prober());
        let mut terminal = ScriptedTerminal::interactive([]);

        let pick = collect_inference(Some(BTreeSet::new()), true, &mut terminal, &recommendations)
            .expect("inference");
        let applications = collect_applications(
            Some(BTreeSet::from([InitialApplicationRuntime::Simulated])),
            &mut terminal,
            &recommendations,
        )
        .expect("applications");

        assert_eq!(pick.inference, BTreeSet::new());
        assert_eq!(
            applications,
            BTreeSet::from([InitialApplicationRuntime::Simulated])
        );
        assert!(terminal.prompts().is_empty());
    }

    fn recommendations(prober: &InitialSiteProber) -> CapabilityRecommendations {
        crate::build_runtime()
            .expect("runtime")
            .block_on(CapabilityRecommendations::collect(prober, true, true))
    }
}
