//! Names the execution targets of a new SiteNode for people, from their runtime and the probed
//! devices they hold. For example, a node's model target is `2× NVIDIA GeForce RTX 5090`, or
//! `NVIDIA GeForce RTX 5090 · GPU 1` when it holds one of the node's GPUs, a simulated one is
//! `Simulated NVIDIA GeForce RTX 5090`, and its application target is `Docker applications`.
//!
//! Names are presentation only and always pass display name validation; the same inputs always
//! give the same name, so site creation and node joining agree on identical hardware. Targets
//! holding different GPUs of one node get different names, unless a name would be too long.

use dotcore_domain::selected_accelerator_facts;
use dotcore_proto::dotcore::observation::v1::{AcceleratorFacts, AcceleratorVendor};

use crate::{
    initial::request::{InitialApplicationRuntime, InitialInferenceRuntime},
    validation::{MAX_DISPLAY_NAME_CHARS, is_valid_display_name},
};

const NVIDIA_FALLBACK: &str = "NVIDIA GPU";
const APPLE_FALLBACK: &str = "Apple GPU";
const SIMULATED_FALLBACK: &str = "GPU";
const SIMULATED_PREFIX: &str = "Simulated";

/// Names a model execution target from its runtime, its accelerator selectors and the probed facts
/// of every accelerator on the node. NVIDIA and simulated targets name their selected cards and
/// list their indices when they hold only some of the node's GPUs; unknown models get a generic
/// name.
pub fn model_target_display_name(
    inference: InitialInferenceRuntime,
    accelerator_device_ids: &[String],
    accelerator_facts: &[AcceleratorFacts],
) -> String {
    let vendor_facts = |vendor: AcceleratorVendor| {
        accelerator_facts
            .iter()
            .filter(|fact| fact.vendor == vendor as i32)
            .cloned()
            .collect::<Vec<_>>()
    };
    match inference {
        InitialInferenceRuntime::Simulated => {
            // Without facts, the name skips the device suffix: a simulated GPU's ID is a made-up
            // UUID that tells people nothing.
            let known = selected_accelerator_facts(accelerator_device_ids, accelerator_facts)
                .is_some_and(|selected| !selected.is_empty());
            let cards = if known {
                gpu_display_name(
                    accelerator_device_ids,
                    accelerator_facts,
                    SIMULATED_FALLBACK,
                )
            } else {
                SIMULATED_FALLBACK.to_owned()
            };
            fitting(
                [format!("{SIMULATED_PREFIX} {cards}")],
                &format!("{SIMULATED_PREFIX} {SIMULATED_FALLBACK}"),
            )
        }
        InitialInferenceRuntime::Apple => vendor_facts(AcceleratorVendor::Apple)
            .first()
            .and_then(|fact| device_model(fact))
            .unwrap_or(APPLE_FALLBACK)
            .to_owned(),
        InitialInferenceRuntime::Nvidia => gpu_display_name(
            accelerator_device_ids,
            &vendor_facts(AcceleratorVendor::Nvidia),
            NVIDIA_FALLBACK,
        ),
    }
}

/// Names an application execution target from its runtime.
pub fn application_target_display_name(applications: InitialApplicationRuntime) -> String {
    match applications {
        InitialApplicationRuntime::Simulated => "Simulated applications",
        InitialApplicationRuntime::Docker => "Docker applications",
    }
    .to_owned()
}

/// Names the cards a target's selectors name, such as
/// `2× NVIDIA GeForce RTX 5090 + NVIDIA GeForce RTX 4090 · GPUs 0, 1, 3`. Too long a name falls
/// back to a card count of the generic name, such as `3× NVIDIA GPU (mixed)`, keeping the suffix
/// when it fits. A target whose GPUs have no known index, such as a MIG slice, lists its device
/// names instead, such as `NVIDIA GPU · gpu1`.
fn gpu_display_name(selectors: &[String], facts: &[AcceleratorFacts], fallback: &str) -> String {
    let Some(selected) = selected_accelerator_facts(selectors, facts).filter(|s| !s.is_empty())
    else {
        return fitting(
            [
                format!("{fallback}{}", device_suffix(selectors)),
                fallback.to_owned(),
            ],
            fallback,
        );
    };

    // Groups identical models, largest group first, then in probe order.
    let mut groups: Vec<(&str, usize)> = Vec::new();
    for fact in &selected {
        let model = device_model(fact).unwrap_or(fallback);
        match groups.iter_mut().find(|(name, _)| *name == model) {
            Some((_, count)) => *count += 1,
            None => groups.push((model, 1)),
        }
    }
    groups.sort_by_key(|(_, count)| std::cmp::Reverse(*count));

    let cards = groups
        .iter()
        .map(|(model, count)| counted(*count, model))
        .collect::<Vec<_>>()
        .join(" + ");
    let summary = match groups.as_slice() {
        [(_, count)] => counted(*count, fallback),
        _ => format!("{} (mixed)", counted(selected.len(), fallback)),
    };
    let suffix = if selected.len() < facts.len() {
        index_suffix(&selected).unwrap_or_else(|| device_suffix(selectors))
    } else {
        String::new()
    };

    fitting(
        [
            format!("{cards}{suffix}"),
            format!("{summary}{suffix}"),
            summary,
        ],
        fallback,
    )
}

/// Returns the first name short enough for a display name, else `fallback`.
fn fitting(names: impl IntoIterator<Item = String>, fallback: &str) -> String {
    names
        .into_iter()
        .find(|name| name.chars().count() <= MAX_DISPLAY_NAME_CHARS)
        .unwrap_or_else(|| fallback.to_owned())
}

/// Returns the device's model name when it is usable in a display name on its own.
fn device_model(fact: &AcceleratorFacts) -> Option<&str> {
    let model = fact.model.trim();
    is_valid_display_name(model).then_some(model)
}

fn counted(count: usize, model: &str) -> String {
    if count == 1 {
        model.to_owned()
    } else {
        format!("{count}× {model}")
    }
}

/// Lists the CDI device names of a target's selectors, such as ` · gpu1` for
/// `nvidia.com/gpu=gpu1`; none without selectors.
fn device_suffix(selectors: &[String]) -> String {
    if selectors.is_empty() {
        return String::new();
    }
    let devices = selectors
        .iter()
        .map(|selector| {
            selector
                .split_once('=')
                .map_or(selector.as_str(), |(_, device)| device)
        })
        .collect::<Vec<_>>();
    format!(" · {}", devices.join(", "))
}

/// Lists the selected GPUs' enumeration indices, such as ` · GPU 1` or ` · GPUs 0, 1`; none when
/// any selected GPU reported no index.
fn index_suffix(selected: &[&AcceleratorFacts]) -> Option<String> {
    let indices = selected
        .iter()
        .map(|fact| fact.vendor_index.map(|index| index.to_string()))
        .collect::<Option<Vec<_>>>()?;
    let label = if indices.len() == 1 { "GPU" } else { "GPUs" };
    Some(format!(" · {label} {}", indices.join(", ")))
}

#[cfg(test)]
mod tests {
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::observation::v1::{AcceleratorFacts, AcceleratorVendor};

    use super::{application_target_display_name, model_target_display_name};
    use crate::initial::request::{InitialApplicationRuntime, InitialInferenceRuntime};

    const RTX_5090: &str = "NVIDIA GeForce RTX 5090";
    const RTX_4090: &str = "NVIDIA GeForce RTX 4090";

    #[test]
    fn runtime_targets_are_named_for_their_runtime() {
        assert_eq!(
            application_target_display_name(InitialApplicationRuntime::Docker),
            "Docker applications"
        );
        assert_eq!(
            application_target_display_name(InitialApplicationRuntime::Simulated),
            "Simulated applications"
        );
    }

    #[test]
    fn simulated_targets_are_named_for_their_simulated_gpu() {
        for (choices, expected) in [
            (vec!["rtx-5090"], vec!["Simulated NVIDIA GeForce RTX 5090"]),
            (
                vec!["rtx-4090", "rtx-4080"],
                vec![
                    "Simulated NVIDIA GeForce RTX 4090 · GPU 0",
                    "Simulated NVIDIA GeForce RTX 4080 · GPU 1",
                ],
            ),
            (vec!["gb10"], vec!["Simulated NVIDIA GB10"]),
            (vec!["mac-64g"], vec!["Simulated Apple M4 Max"]),
        ] {
            let choices = choices.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let machine = MachineDescription::simulated(&choices, "arm64").expect("machine");
            let facts = machine.accelerator_facts();
            let names = machine
                .gpu_ids()
                .map(|gpu_id| {
                    model_target_display_name(
                        InitialInferenceRuntime::Simulated,
                        &[gpu_id.to_owned()],
                        &facts,
                    )
                })
                .collect::<Vec<_>>();

            assert_eq!(names, expected, "{choices:?}");
        }

        assert_eq!(
            model_target_display_name(InitialInferenceRuntime::Simulated, &[], &[]),
            "Simulated GPU"
        );
    }

    #[test]
    fn apple_target_is_named_for_its_chip() {
        let chip = AcceleratorFacts {
            vendor: AcceleratorVendor::Apple as i32,
            model: " Apple M3 Max ".to_owned(),
            ..AcceleratorFacts::default()
        };
        let devices = ["apple-gpu-0".to_owned()];

        assert_eq!(
            model_target_display_name(InitialInferenceRuntime::Apple, &devices, &[chip]),
            "Apple M3 Max"
        );
        assert_eq!(
            model_target_display_name(InitialInferenceRuntime::Apple, &devices, &[]),
            "Apple GPU"
        );
    }

    #[test]
    fn nvidia_target_holding_every_gpu_names_its_cards() {
        for (models, selectors, expected) in [
            (vec![RTX_5090], vec!["nvidia.com/gpu=gpu0"], RTX_5090),
            (
                vec![RTX_5090, RTX_5090],
                vec!["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"],
                "2× NVIDIA GeForce RTX 5090",
            ),
            (
                vec![RTX_4090, RTX_5090, RTX_5090],
                vec!["nvidia.com/gpu=all"],
                "2× NVIDIA GeForce RTX 5090 + NVIDIA GeForce RTX 4090",
            ),
        ] {
            assert_eq!(nvidia_name(&models, &selectors), expected, "{selectors:?}");
        }
    }

    #[test]
    fn nvidia_target_holding_some_gpus_lists_their_indices() {
        assert_eq!(
            nvidia_name(&[RTX_5090, RTX_5090], &["nvidia.com/gpu=gpu1"]),
            "NVIDIA GeForce RTX 5090 · GPU 1"
        );
        assert_eq!(
            nvidia_name(
                &[RTX_5090, RTX_4090, RTX_5090],
                &["nvidia.com/gpu=2", "nvidia.com/gpu=gpu1"]
            ),
            "NVIDIA GeForce RTX 4090 + NVIDIA GeForce RTX 5090 · GPUs 1, 2"
        );
    }

    #[test]
    fn nvidia_target_without_usable_models_gets_a_generic_name() {
        assert_eq!(nvidia_name(&[""], &["nvidia.com/gpu=gpu0"]), "NVIDIA GPU");
        assert_eq!(
            nvidia_name(&["", ""], &["nvidia.com/gpu=all"]),
            "2× NVIDIA GPU"
        );
        assert_eq!(
            nvidia_name(&["bad\u{7}name"], &["nvidia.com/gpu=gpu0"]),
            "NVIDIA GPU"
        );
    }

    #[test]
    fn nvidia_target_without_known_indices_lists_its_device_names() {
        assert_eq!(
            nvidia_name(&[RTX_5090], &["nvidia.com/gpu=gpu4"]),
            "NVIDIA GPU · gpu4"
        );
        assert_eq!(
            nvidia_name(&[], &["nvidia.com/gpu=gpu0"]),
            "NVIDIA GPU · gpu0"
        );
        assert_eq!(
            nvidia_name(&[RTX_5090, RTX_5090], &["nvidia.com/gpu=0:1"]),
            "NVIDIA GPU · 0:1"
        );

        let mut facts = nvidia_facts(&[RTX_5090, RTX_5090]);
        facts[1].vendor_index = None;
        assert_eq!(
            model_target_display_name(
                InitialInferenceRuntime::Nvidia,
                &["nvidia.com/gpu=GPU-1".to_owned()],
                &facts
            ),
            "NVIDIA GeForce RTX 5090 · GPU-1"
        );
    }

    #[test]
    fn long_nvidia_name_falls_back_to_a_card_count() {
        let models = (0..12)
            .map(|index| format!("NVIDIA Model {index}"))
            .collect::<Vec<_>>();
        let models = models.iter().map(String::as_str).collect::<Vec<_>>();

        assert_eq!(
            nvidia_name(&models, &["nvidia.com/gpu=all"]),
            "12× NVIDIA GPU (mixed)"
        );
        assert_eq!(
            nvidia_name(
                &models,
                &[
                    "nvidia.com/gpu=0",
                    "nvidia.com/gpu=1",
                    "nvidia.com/gpu=2",
                    "nvidia.com/gpu=3",
                    "nvidia.com/gpu=4",
                    "nvidia.com/gpu=5",
                    "nvidia.com/gpu=6",
                    "nvidia.com/gpu=7",
                    "nvidia.com/gpu=8",
                    "nvidia.com/gpu=9",
                    "nvidia.com/gpu=10"
                ]
            ),
            "11× NVIDIA GPU (mixed) · GPUs 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10"
        );
    }

    fn nvidia_name(models: &[&str], selectors: &[&str]) -> String {
        let selectors = selectors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        model_target_display_name(
            InitialInferenceRuntime::Nvidia,
            &selectors,
            &nvidia_facts(models),
        )
    }

    fn nvidia_facts(models: &[&str]) -> Vec<AcceleratorFacts> {
        models
            .iter()
            .enumerate()
            .map(|(index, model)| AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: (*model).to_owned(),
                device_id: format!("GPU-{index}"),
                vendor_index: Some(u32::try_from(index).expect("index")),
                ..AcceleratorFacts::default()
            })
            .collect()
    }
}
