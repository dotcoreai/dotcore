//! The memory block of a deployment estimate: how full each memory pool of the target would be,
//! and what the server says fits.
//!
//! Each pool the target draws from becomes one bar: what already occupies it, this deployment's
//! demand and the fit line, with the server's verdict and, past the line, by how much. The largest
//! fits and the values sizing derived follow. Every number comes from the estimate; nothing is
//! computed here beyond adding up a pool's segments.

use dotcore_catalog::typed_parameters::canonical_text;
use dotcore_proto::dotcore::{
    capacity::v1::{MemoryPoolKind, MemoryPoolPreview, MemorySegmentSource},
    deployment::v1::MemoryFit,
    model::v1::EstimateLocalModelResourcesResponse,
};

use super::{
    template::{all_at_once, fit_hints},
    view::{CONTEXT_PER_REQUEST, TOTAL_CONTEXT},
};
use crate::{
    hardware::gib,
    terminal::{BarChart, BarFill, BarRow, BarSegment, BarTone, Message},
};

/// The estimate's overall verdict on one line.
pub(super) fn verdict(estimate: &EstimateLocalModelResourcesResponse) -> Message {
    match estimate
        .placement_preview
        .as_ref()
        .map(|preview| preview.free_memory_fit())
    {
        Some(MemoryFit::Fits) => Message::Success("Memory: fits now".to_owned()),
        Some(MemoryFit::Exceeds) => Message::Warning("Memory: exceeds the free memory".to_owned()),
        _ => Message::Info("Memory: fit unknown".to_owned()),
    }
}

/// The pools' bars, then the largest fits and derived values; `parallel_requests` names the
/// requests the per-request fit counts.
pub(super) fn memory_block(
    estimate: &EstimateLocalModelResourcesResponse,
    parallel_requests: Option<u64>,
) -> Vec<Message> {
    let mut messages = Vec::new();
    let pools = estimate
        .placement_preview
        .as_ref()
        .map_or(&[][..], |preview| &preview.pools);
    if !pools.is_empty() {
        messages.push(Message::Bars(BarChart {
            rows: pools.iter().map(pool_row).collect(),
            legend: vec![
                (BarFill::Used, "in use".to_owned()),
                (BarFill::Added, "this deployment".to_owned()),
            ],
            marker_legend: Some("fit line".to_owned()),
        }));
    }
    if let Some(facts) = fit_facts(estimate, parallel_requests) {
        messages.push(facts);
    }
    messages
}

/// The footer of a guided deploy: one bar per memory pool, then the verdict with what the server
/// says fits, or a launch rejection in its place; `parallel_requests` names the requests the
/// per-request fit counts.
pub(super) fn footer_lines(
    estimate: &EstimateLocalModelResourcesResponse,
    parallel_requests: Option<u64>,
) -> Vec<Message> {
    let mut lines = Vec::new();
    let pools = estimate
        .placement_preview
        .as_ref()
        .map_or(&[][..], |preview| &preview.pools);
    if !pools.is_empty() {
        lines.push(Message::Bars(BarChart {
            rows: pools.iter().map(pool_row).collect(),
            legend: Vec::new(),
            marker_legend: None,
        }));
    }
    if let Some(reason) = launch_rejection(estimate) {
        lines.push(Message::Error(format!(
            "A launch would reject these values: {reason}"
        )));
        return lines;
    }

    let mut status = verdict(estimate);
    let hints = fit_hints(estimate, parallel_requests);
    let text = match &mut status {
        Message::Success(text) | Message::Warning(text) | Message::Info(text) => text,
        _ => return lines,
    };
    for key in [CONTEXT_PER_REQUEST, TOTAL_CONTEXT] {
        if let Some(hint) = hints.get(key) {
            text.push_str(" · ");
            text.push_str(&hint.text);
        }
    }
    lines.push(status);
    lines
}

/// The memory the deployment was estimated to take on `machine`, on one line: each pool's use
/// against its total, with the verdict's tone.
pub(super) fn memory_line(
    estimate: &EstimateLocalModelResourcesResponse,
    machine: &str,
) -> Message {
    let pools = estimate
        .placement_preview
        .as_ref()
        .map_or(&[][..], |preview| &preview.pools);
    let notes = pools
        .iter()
        .map(|pool| {
            let row = pool_row(pool);
            format!("{} {}", row.label, row.note)
        })
        .collect::<Vec<_>>();
    let text = if notes.is_empty() {
        format!("Memory on {machine}: fit unknown")
    } else {
        format!("Memory on {machine}: {}", notes.join(" · "))
    };
    match verdict(estimate) {
        Message::Success(_) => Message::Success(text),
        Message::Warning(_) => Message::Warning(text),
        _ => Message::Info(text),
    }
}

/// The reason a launch would reject the estimated values, if one would.
fn launch_rejection(estimate: &EstimateLocalModelResourcesResponse) -> Option<&str> {
    estimate
        .hardware_class_estimates
        .iter()
        .find_map(|entry| entry.launch_rejection.as_deref())
}

/// One pool's bar: other use, then this deployment's demand, against its total and fit line.
fn pool_row(preview: &MemoryPoolPreview) -> BarRow {
    let pool = preview.pool.clone().unwrap_or_default();
    let label = match pool.kind() {
        MemoryPoolKind::Device => {
            let mut label = pool
                .device_index
                .map_or_else(|| "GPU".to_owned(), |index| format!("GPU {index}"));
            if !pool.device_name.is_empty() {
                label.push(' ');
                label.push_str(&pool.device_name);
            }
            label
        }
        MemoryPoolKind::Host => "Host memory".to_owned(),
        MemoryPoolKind::Unspecified => "Memory".to_owned(),
    };

    let (demand, used) = pool
        .segments
        .iter()
        .fold((0, 0), |(demand, used), segment| {
            if segment.source() == MemorySegmentSource::Previewed {
                (demand + segment.bytes, used)
            } else {
                (demand, used + segment.bytes)
            }
        });
    let drawn = match pool.total_bytes {
        Some(total) => format!("{} of {}", gib(used + demand), gib(total)),
        None => format!("{} in use, total unknown", gib(used + demand)),
    };
    let (verdict, tone) = match preview.free_memory_fit() {
        MemoryFit::Fits => ("fits".to_owned(), BarTone::Good),
        MemoryFit::Exceeds if preview.beyond_total => (
            format!("over the total by {}", gib(preview.over_by_bytes)),
            BarTone::Bad,
        ),
        MemoryFit::Exceeds => (
            format!("over the fit line by {}", gib(preview.over_by_bytes)),
            BarTone::Bad,
        ),
        MemoryFit::Unknown | MemoryFit::Unspecified => ("fit unknown".to_owned(), BarTone::Neutral),
    };

    BarRow {
        label,
        segments: vec![
            BarSegment {
                fill: BarFill::Used,
                amount: used,
            },
            BarSegment {
                fill: BarFill::Added,
                amount: demand,
            },
        ],
        capacity: pool.total_bytes,
        marker: preview.fit_line_bytes,
        note: format!("{drawn} · {verdict}"),
        tone,
    }
}

/// The largest values that fit and the values sizing derived, as aligned facts; none when the
/// estimate gives neither.
pub(super) fn fit_facts(
    estimate: &EstimateLocalModelResourcesResponse,
    parallel_requests: Option<u64>,
) -> Option<Message> {
    let mut facts = Vec::new();
    if let Some(tokens) = estimate.largest_context_tokens_per_request {
        facts.push((
            format!(
                "Context per request fitting {}",
                all_at_once(parallel_requests)
            ),
            tokens.to_string(),
        ));
    }
    if let Some(tokens) = estimate.largest_total_context_tokens {
        facts.push(("Largest total context".to_owned(), tokens.to_string()));
    }
    for fit in &estimate.memory_parameter_fits {
        facts.push((
            format!("Largest {}", fit.parameter_id),
            fit.largest_fit.to_string(),
        ));
    }
    for value in estimate
        .hardware_class_estimates
        .iter()
        .flat_map(|entry| entry.derived_engine_parameter_values.iter())
        .flat_map(|values| values.values.iter())
    {
        if let Some(text) = value.value.as_ref().map(canonical_text) {
            facts.push((format!("Derived {}", value.parameter_id), text));
        }
    }
    (!facts.is_empty()).then_some(Message::KeyValues(facts))
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        capacity::v1::{MemoryPool, MemorySegment, PlacementPreview},
        catalog::v1::{ParameterValue, ParameterValues, parameter_value},
        model::v1::HardwareClassResourceEstimate,
    };

    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn a_fitting_estimate_draws_its_pools_with_the_largest_fits() {
        let estimate = estimate(
            MemoryFit::Fits,
            vec![
                pool(MemoryPoolKind::Device, 6, 6, MemoryFit::Fits, 0, false),
                pool(MemoryPoolKind::Host, 8, 0, MemoryFit::Fits, 0, false),
            ],
        );

        let block = memory_block(&estimate, Some(4));

        assert_eq!(verdict(&estimate).plain_lines(), ["✔ Memory: fits now"]);
        assert_eq!(
            lines(&block),
            [
                "  GPU 0 RTX    ██████▓▓▓▓▓▓░░░░░│░░░░░░░  12.0 GiB of 24.0 GiB · fits",
                "  Host memory  ████████░░░░░░░░│░░░░░░░░  8.0 GiB of 24.0 GiB · fits",
                "  █ in use  ▓ this deployment  │ fit line",
                "  Context per request fitting all 4 at once  49152",
                "  Largest total context                      196608",
                "  Derived total-context                      131072",
            ]
        );
    }

    #[test]
    fn an_exceeding_estimate_says_by_how_much_each_pool_is_over() {
        let estimate = estimate(
            MemoryFit::Exceeds,
            vec![
                pool(MemoryPoolKind::Device, 6, 16, MemoryFit::Exceeds, 2, false),
                pool(MemoryPoolKind::Host, 20, 8, MemoryFit::Exceeds, 4, true),
            ],
        );

        let block = memory_block(&estimate, None);

        assert_eq!(
            verdict(&estimate).ascii_lines(),
            ["! Memory: exceeds the free memory"]
        );
        assert_eq!(
            ascii_lines(&block)[..3],
            [
                "  GPU 0 RTX    ######+++++++++++|+++++..  22.0 GiB of 24.0 GiB · over the fit line by 2.0 GiB",
                "  Host memory  ##############|###+++++++  28.0 GiB of 24.0 GiB · over the total by 4.0 GiB",
                "  # in use  + this deployment  | fit line",
            ]
        );
        assert_eq!(
            ascii_lines(&block)[3],
            "  Context per request fitting all requests at once  49152"
        );
    }

    #[test]
    fn without_a_demand_the_fit_is_unknown() {
        let estimate = EstimateLocalModelResourcesResponse {
            placement_preview: Some(PlacementPreview {
                pools: vec![pool(
                    MemoryPoolKind::Device,
                    6,
                    0,
                    MemoryFit::Unknown,
                    0,
                    false,
                )],
                free_memory_fit: MemoryFit::Unknown.into(),
                ..PlacementPreview::default()
            }),
            ..EstimateLocalModelResourcesResponse::default()
        };

        let block = memory_block(&estimate, None);

        assert_eq!(verdict(&estimate).plain_lines(), ["Memory: fit unknown"]);
        assert_eq!(
            lines(&block),
            [
                "  GPU 0 RTX  ██████░░░░░░░░░░░│░░░░░░░  6.0 GiB of 24.0 GiB · fit unknown",
                "  █ in use  ▓ this deployment  │ fit line",
            ]
        );
        assert_eq!(
            ascii_lines(&block),
            [
                "  GPU 0 RTX  ######...........|.......  6.0 GiB of 24.0 GiB · fit unknown",
                "  # in use  + this deployment  | fit line",
            ]
        );
        assert_eq!(
            memory_block(&EstimateLocalModelResourcesResponse::default(), None),
            []
        );
    }

    #[test]
    fn the_footer_draws_a_bar_per_pool_then_the_verdict_with_the_hints() {
        let fits = estimate(
            MemoryFit::Fits,
            vec![
                pool(MemoryPoolKind::Device, 6, 6, MemoryFit::Fits, 0, false),
                pool(MemoryPoolKind::Device, 2, 6, MemoryFit::Fits, 0, false),
            ],
        );
        let exceeds = estimate(
            MemoryFit::Exceeds,
            vec![pool(
                MemoryPoolKind::Device,
                6,
                16,
                MemoryFit::Exceeds,
                2,
                false,
            )],
        );

        assert_eq!(
            lines(&footer_lines(&fits, Some(4))),
            [
                "  GPU 0 RTX  ██████▓▓▓▓▓▓░░░░░│░░░░░░░  12.0 GiB of 24.0 GiB · fits",
                "  GPU 0 RTX  ██▓▓▓▓▓▓░░░░░░░░░│░░░░░░░  8.0 GiB of 24.0 GiB · fits",
                "✔ Memory: fits now · Fits all 4 at once: 49152 · Largest fit: 196608",
            ]
        );
        assert_eq!(
            lines(&footer_lines(&exceeds, None)),
            [
                "  GPU 0 RTX  ██████▓▓▓▓▓▓▓▓▓▓▓│▓▓▓▓▓░░  22.0 GiB of 24.0 GiB · over the fit line by 2.0 GiB",
                "! Memory: exceeds the free memory · Fits all requests at once: 49152 · Largest fit: 196608",
            ]
        );
        assert_eq!(
            lines(&footer_lines(
                &EstimateLocalModelResourcesResponse::default(),
                None
            )),
            ["Memory: fit unknown"]
        );
    }

    #[test]
    fn a_launch_rejection_takes_the_place_of_the_verdict() {
        let mut rejected = estimate(
            MemoryFit::Fits,
            vec![pool(MemoryPoolKind::Host, 8, 0, MemoryFit::Fits, 0, false)],
        );
        rejected.hardware_class_estimates[0].launch_rejection =
            Some("context per request exceeds the total context".to_owned());

        assert_eq!(
            lines(&footer_lines(&rejected, None)),
            [
                "  Host memory  ████████░░░░░░░░│░░░░░░░░  8.0 GiB of 24.0 GiB · fits",
                "✘ A launch would reject these values: context per request exceeds the total context",
            ]
        );
    }

    #[test]
    fn the_memory_line_states_each_pool_on_the_machine_with_the_verdict() {
        let fits = estimate(
            MemoryFit::Fits,
            vec![
                pool(MemoryPoolKind::Device, 6, 6, MemoryFit::Fits, 0, false),
                pool(MemoryPoolKind::Host, 8, 0, MemoryFit::Fits, 0, false),
            ],
        );

        assert_eq!(
            memory_line(&fits, "gpu1 CUDA").plain_lines(),
            [
                "✔ Memory on gpu1 CUDA: GPU 0 RTX 12.0 GiB of 24.0 GiB · fits · Host memory 8.0 GiB of 24.0 GiB · fits"
            ]
        );
        assert_eq!(
            memory_line(&EstimateLocalModelResourcesResponse::default(), "gpu1 CUDA").plain_lines(),
            ["Memory on gpu1 CUDA: fit unknown"]
        );
    }

    fn lines(block: &[Message]) -> Vec<String> {
        block.iter().flat_map(Message::plain_lines).collect()
    }

    fn ascii_lines(block: &[Message]) -> Vec<String> {
        block.iter().flat_map(Message::ascii_lines).collect()
    }

    fn estimate(
        fit: MemoryFit,
        pools: Vec<MemoryPoolPreview>,
    ) -> EstimateLocalModelResourcesResponse {
        EstimateLocalModelResourcesResponse {
            placement_preview: Some(PlacementPreview {
                pools,
                free_memory_fit: fit.into(),
                ..PlacementPreview::default()
            }),
            hardware_class_estimates: vec![HardwareClassResourceEstimate {
                derived_engine_parameter_values: Some(ParameterValues {
                    values: vec![ParameterValue {
                        parameter_id: "total-context".to_owned(),
                        value: Some(parameter_value::Value::UnsignedInteger(131_072)),
                    }],
                }),
                ..HardwareClassResourceEstimate::default()
            }],
            largest_total_context_tokens: Some(196_608),
            largest_context_tokens_per_request: Some(49152),
            ..EstimateLocalModelResourcesResponse::default()
        }
    }

    /// A 24 GiB pool with a 17 GiB fit line holding `used` GiB of other use and `demand` GiB of
    /// the previewed deployment.
    fn pool(
        kind: MemoryPoolKind,
        used: u64,
        demand: u64,
        fit: MemoryFit,
        over_by: u64,
        beyond_total: bool,
    ) -> MemoryPoolPreview {
        let mut segments = vec![MemorySegment {
            source: MemorySegmentSource::Other.into(),
            bytes: used * GIB,
            ..MemorySegment::default()
        }];
        if demand > 0 {
            segments.push(MemorySegment {
                source: MemorySegmentSource::Previewed.into(),
                bytes: demand * GIB,
                ..MemorySegment::default()
            });
        }
        let device = kind == MemoryPoolKind::Device;
        MemoryPoolPreview {
            pool: Some(MemoryPool {
                kind: kind.into(),
                device_name: if device {
                    "RTX".to_owned()
                } else {
                    String::new()
                },
                device_index: device.then_some(0),
                total_bytes: Some(24 * GIB),
                segments,
                ..MemoryPool::default()
            }),
            fit_line_bytes: Some(if device { 17 * GIB } else { 16 * GIB }),
            free_memory_fit: fit.into(),
            over_by_bytes: over_by * GIB,
            beyond_total,
        }
    }
}
