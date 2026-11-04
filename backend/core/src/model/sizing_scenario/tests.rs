use std::time::{Duration, SystemTime};

use dotcore_catalog::{Catalog, CatalogLocalModelDeclarations, CatalogModelId, OciArtifactPolicy};
use dotcore_domain::{ExecutionPlatform, MemoryArchitecture};
use dotcore_proto::dotcore::{
    catalog::v1::Model,
    deployment::v1::{ExactPlacement, MemoryFit},
    model::v1::{
        HardwareClassResourceEstimate, ListModelProfileCompatibilityResponse,
        LocalModelDeploymentConfiguration, ModelCompatibilityReason, ModelCompatibilityReasonCode,
        ModelCompatibilityStatus, ModelProfileCompatibility, ModelProfileSizing,
        ScalarResourceEstimate, SizingScenarioUnsupportedReason, StartingTotalContextSource,
    },
    site::v1::ExecutionTarget,
};
use dotcore_site::node::target_inventory::{
    NodeAcceleratorFacts, NodeAcceleratorObservation, RuntimePlatformObservation,
    TargetCapabilityInventory, TargetCapabilityReconciler,
};

use super::{InvalidSizingScenario, SizingScenario, with_scenario_values};
use crate::{
    model::compatibility::evaluate_profile_compatibility, test_support::unsigned_parameter_values,
};

const MIB: u64 = 1024 * 1024;

#[test]
fn scenarios_need_the_minimum_context_and_a_context_per_request_within_the_total() {
    let capped = SizingScenario::new(1024, Some(1024)).expect("minimum scenario");
    assert_eq!(
        (
            capped.total_context_tokens(),
            capped.context_tokens_per_request()
        ),
        (1024, Some(1024))
    );
    assert_eq!(
        SizingScenario::new(u32::MAX, None).map(|scenario| scenario.context_tokens_per_request()),
        Ok(None)
    );
    assert_eq!(
        SizingScenario::new(1023, None),
        Err(InvalidSizingScenario::TotalContextBelowMinimum {
            total_context_tokens: 1023
        })
    );
    assert_eq!(
        SizingScenario::new(4096, Some(1023)),
        Err(InvalidSizingScenario::ContextPerRequestBelowMinimum {
            context_tokens_per_request: 1023
        })
    );
    assert_eq!(
        SizingScenario::new(4096, Some(4097)),
        Err(InvalidSizingScenario::ContextPerRequestAboveTotal {
            total_context_tokens: 4096,
            context_tokens_per_request: 4097
        })
    );
}

#[test]
fn each_engine_sets_the_scenario_on_its_profile_values() {
    let catalog = scenario_catalog();

    // llama.cpp: the scenario sets the total context over the engine's default 4 parallel
    // requests; the omitted context per request fills to the total context.
    let llama_estimates = |derived: &[(&str, u64)]| {
        ["dedicated", "unified"]
            .map(|class| {
                class_estimate(
                    class,
                    1_265_536_000,
                    Some(1024 * MIB),
                    derived.iter().copied(),
                )
            })
            .to_vec()
    };
    assert_eq!(
        profiles(&catalog, LLAMA_MODEL, Some(scenario(65_536, Some(16_384)))),
        [supported(
            LLAMA_MODEL,
            "chat",
            [
                ("prompt-cache-memory-mib", 1024),
                ("total-context", 65_536),
                ("parallel-requests", 4),
                ("context-per-request", 16_384),
            ],
            llama_estimates(&[]),
        )]
    );
    assert_eq!(
        profiles(&catalog, LLAMA_MODEL, Some(scenario(65_536, None))),
        [supported(
            LLAMA_MODEL,
            "chat",
            [
                ("prompt-cache-memory-mib", 1024),
                ("total-context", 65_536),
                ("parallel-requests", 4),
                ("context-per-request", 65_536),
            ],
            llama_estimates(&[("context-per-request", 65_536)]),
        )]
    );

    // vLLM: the KV cache memory derives per class from the total context; the omitted
    // context per request fills to the total context.
    let vllm_estimates = |filled: Option<u64>| {
        let derived = |kv_bytes| {
            filled
                .map(|value| ("context-per-request", value))
                .into_iter()
                .chain([("kv-cache-memory-bytes", kv_bytes)])
                .collect::<Vec<_>>()
        };
        vec![
            class_estimate("dedicated", 13_089_934_592, None, derived(9_089_934_592)),
            class_estimate("unified", 21_679_869_184, None, derived(17_679_869_184)),
        ]
    };
    let vllm_values = |context_per_request| {
        [
            ("maximum-batched-tokens", 1024),
            ("maximum-cudagraph-capture-size", 16),
            ("kv-offloading-size-gib", 0),
            ("total-context", 131_072),
            ("parallel-requests", 4),
            ("context-per-request", context_per_request),
        ]
    };
    assert_eq!(
        profiles(&catalog, VLLM_MODEL, Some(scenario(131_072, Some(32_768)))),
        [supported(
            VLLM_MODEL,
            "text",
            vllm_values(32_768),
            vllm_estimates(None)
        )]
    );
    assert_eq!(
        profiles(&catalog, VLLM_MODEL, Some(scenario(131_072, None))),
        [supported(
            VLLM_MODEL,
            "text",
            vllm_values(131_072),
            vllm_estimates(Some(131_072))
        )]
    );
    // A configuration maximum below the total context bounds the filled value.
    assert_eq!(
        profiles(&catalog, VLLM_CAPPED_MODEL, Some(scenario(131_072, None))),
        [supported(
            VLLM_CAPPED_MODEL,
            "text",
            vllm_values(65_536),
            vllm_estimates(Some(65_536))
        )]
    );

    // ninfer: host context and device state slots keep the profile defaults, and the
    // profile's 4 parallel requests hold the total context at the context per request. With
    // MTP on, the draft adds its base and per-token terms: 500 + 2 × 131072 bytes.
    for (context_per_request, filled) in [(Some(65_536), None), (None, Some(131_072))] {
        let estimate = |accelerator_memory_bytes| {
            vec![class_estimate(
                "dedicated",
                accelerator_memory_bytes,
                Some(20_480 * MIB),
                filled.map(|value| ("context-per-request", value)),
            )]
        };
        let values = [
            ("parallel-requests", 4),
            ("host-context-mib", 20_480),
            ("device-state-slots", 4),
            ("total-context", 131_072),
            (
                "context-per-request",
                context_per_request.or(filled).expect("context per request"),
            ),
        ];
        assert_eq!(
            profiles(
                &catalog,
                NINFER_MODEL,
                Some(scenario(
                    131_072,
                    context_per_request.map(|value| value as u32)
                ))
            ),
            [
                supported(NINFER_MODEL, "mtp", values, estimate(1_574_764)),
                supported(NINFER_MODEL, "no-mtp", values, estimate(1_312_120)),
            ],
            "context per request {context_per_request:?}"
        );
    }
}

#[test]
fn unsupported_scenarios_report_the_first_reason_on_profiles_and_running_targets() {
    use SizingScenarioUnsupportedReason::{
        ContextAboveMaximum, DerivedParameterOutOfRange, ParameterOutOfRange,
    };
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let inventory = observed(&target, MemoryArchitecture::Dedicated, Some(1 << 40), now);
    let cases = [
        // Above the 524288-token model maximum, which wins over a total context above the
        // 16777216-token range.
        (LLAMA_MODEL, 16_777_217, Some(524_289), ContextAboveMaximum),
        (LLAMA_MODEL, 16_777_217, None, ParameterOutOfRange),
        // Above the configuration's 65536-token maximum, below the model's 262144.
        (
            VLLM_CAPPED_MODEL,
            131_072,
            Some(65_537),
            ContextAboveMaximum,
        ),
        (NINFER_MODEL, 600_000, Some(524_289), ContextAboveMaximum),
        // Within the model maximum, above ninfer's 262144-token context per request range,
        // supplied or filled.
        (NINFER_MODEL, 300_000, Some(300_000), ParameterOutOfRange),
        (NINFER_MODEL, 300_000, None, ParameterOutOfRange),
        // Above 8 × 4096 tokens, and above the profile's 4 parallel requests × 4096.
        (NINFER_MODEL, 40_960, Some(4096), ParameterOutOfRange),
        (NINFER_MODEL, 20_480, Some(4096), ParameterOutOfRange),
        // The derived KV cache memory is above its 64 GiB maximum on every class.
        (
            VLLM_MODEL,
            2_097_152,
            Some(262_144),
            DerivedParameterOutOfRange,
        ),
    ];

    for (model_id, total_context, context_per_request, expected) in cases {
        let without_scenario = evaluate(&catalog, model_id, None, &[&target], &inventory, now);
        let response = evaluate(
            &catalog,
            model_id,
            Some(scenario(total_context, context_per_request)),
            &[&target],
            &inventory,
            now,
        );

        let label = format!("{model_id} at {total_context} with {context_per_request:?}");
        let unsupported_classes = if expected == DerivedParameterOutOfRange {
            vec!["dedicated".to_owned(), "unified".to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(
            response
                .profiles
                .iter()
                .map(|profile| (
                    profile.engine_parameter_values.clone(),
                    profile.hardware_class_estimates.clone(),
                    profile.unsupported_reason(),
                    profile.unsupported_hardware_class_ids.clone(),
                ))
                .collect::<Vec<_>>(),
            response
                .profiles
                .iter()
                .map(|_| (None, Vec::new(), expected, unsupported_classes.clone()))
                .collect::<Vec<_>>(),
            "{label}"
        );
        assert!(!response.profiles.is_empty(), "{label}");
        // Status, reason and advisories stay those of the profile defaults.
        assert_eq!(
            response
                .entries
                .iter()
                .map(|entry| (
                    entry.status(),
                    entry.reason.clone(),
                    entry.advisories.clone(),
                    entry.hardware_class_id.clone(),
                    entry.memory_fit(),
                    entry.unsupported_reason(),
                ))
                .collect::<Vec<_>>(),
            without_scenario
                .entries
                .iter()
                .map(|entry| (
                    ModelCompatibilityStatus::Runs,
                    None,
                    entry.advisories.clone(),
                    "dedicated".to_owned(),
                    MemoryFit::Unspecified,
                    expected,
                ))
                .collect::<Vec<_>>(),
            "{label}"
        );
        assert_eq!(response.targets, without_scenario.targets, "{label}");
    }
}

#[test]
fn a_derived_value_out_of_range_on_one_class_marks_only_that_class() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let dedicated = target("dedicated", "node-a");
    let unified = target("unified", "node-b");
    let mut inventory = observed(
        &dedicated,
        MemoryArchitecture::Dedicated,
        Some(60_000_000_000),
        now,
    );
    inventory.extend(observed(
        &unified,
        MemoryArchitecture::Unified,
        Some(128_000_000_000),
        now,
    ));

    // A total context of 786432 tokens derives 52039607552 bytes of KV cache on the dedicated
    // class and 103579215104 bytes, above the 64 GiB maximum, on the unified one.
    let response = evaluate(
        &catalog,
        VLLM_MODEL,
        Some(scenario(786_432, Some(131_072))),
        &[&dedicated, &unified],
        &inventory,
        now,
    );

    assert_eq!(
        response.profiles,
        [ModelProfileSizing {
            unsupported_hardware_class_ids: vec!["unified".to_owned()],
            ..supported(
                VLLM_MODEL,
                "text",
                [
                    ("maximum-batched-tokens", 1024),
                    ("maximum-cudagraph-capture-size", 16),
                    ("kv-offloading-size-gib", 0),
                    ("total-context", 786_432),
                    ("parallel-requests", 4),
                    ("context-per-request", 131_072),
                ],
                vec![class_estimate(
                    "dedicated",
                    56_039_607_552,
                    None,
                    [("kv-cache-memory-bytes", 52_039_607_552)]
                )],
            )
        }]
    );
    assert_eq!(
        fits(&response),
        [
            (
                placement(&dedicated),
                "dedicated".to_owned(),
                MemoryFit::Fits,
                SizingScenarioUnsupportedReason::Unspecified
            ),
            (
                placement(&unified),
                "unified".to_owned(),
                MemoryFit::Unspecified,
                SizingScenarioUnsupportedReason::DerivedParameterOutOfRange
            ),
        ]
    );
}

#[test]
fn memory_fit_compares_the_class_estimate_with_the_target_total_memory() {
    use MemoryFit::{Exceeds, Fits, Unknown};
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    // The 16384-token scenario needs 1216384000 accelerator bytes and 1073741824 host bytes,
    // 2290125824 together; the defaults need 1232768000 accelerator bytes, 2306509824 with the
    // host.
    let cases = [
        (
            MemoryArchitecture::Dedicated,
            Some(1_232_768_000),
            Fits,
            Fits,
        ),
        (
            MemoryArchitecture::Dedicated,
            Some(1_216_384_000),
            Fits,
            Exceeds,
        ),
        (
            MemoryArchitecture::Dedicated,
            Some(1_216_383_999),
            Exceeds,
            Exceeds,
        ),
        // Unified memory counts the host estimate too.
        (
            MemoryArchitecture::Unified,
            Some(2_290_125_824),
            Fits,
            Exceeds,
        ),
        (
            MemoryArchitecture::Unified,
            Some(2_290_125_823),
            Exceeds,
            Exceeds,
        ),
        // A target that can't determine its memory total has no fit.
        (MemoryArchitecture::Dedicated, None, Unknown, Unknown),
    ];

    for (memory_architecture, pool_total, scenario_fit, defaults_fit) in cases {
        let class = match memory_architecture {
            MemoryArchitecture::Dedicated => "dedicated",
            MemoryArchitecture::Unified => "unified",
        };
        let target = target(class, "node-a");
        let inventory = observed(&target, memory_architecture, pool_total, now);
        let read = |scenario| {
            fits(&evaluate(
                &catalog,
                LLAMA_MODEL,
                scenario,
                &[&target],
                &inventory,
                now,
            ))
        };

        let fit = |fit| {
            vec![(
                placement(&target),
                class.to_owned(),
                fit,
                SizingScenarioUnsupportedReason::Unspecified,
            )]
        };
        assert_eq!(
            read(Some(scenario(16_384, None))),
            fit(scenario_fit),
            "{memory_architecture:?} pool {pool_total:?} with the scenario"
        );
        assert_eq!(
            read(None),
            fit(defaults_fit),
            "{memory_architecture:?} pool {pool_total:?} with the defaults"
        );
    }
}

#[test]
fn the_largest_fitting_total_context_matches_the_largest_total_whose_read_fits() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    use MemoryArchitecture::{Dedicated, Unified};
    // Each pool lands the solution inside every engine bound, with a host estimate counting
    // on unified memory only. llama.cpp: 1200000000 + 1000 × T accelerator bytes and 1 GiB of
    // host prompt cache; vLLM: 4500000000 + 65536 or 131072 × T bytes; ninfer: 1900 + 12 × T
    // bytes with MTP, 1400 + 10 × T without, and 20 GiB of host context. Each idle pool is the
    // smallest whose free memory less the 1% headroom is a round 2000000000, 3000000000,
    // 20000000000, 40000000000 or 300000 bytes, or 20 GiB + 300000 for ninfer on unified memory.
    let cases: [(&str, MemoryArchitecture, u64, &[u32]); 6] = [
        (LLAMA_MODEL, Dedicated, 2_020_202_020, &[800_000]),
        (LLAMA_MODEL, Unified, 3_030_303_030, &[726_258]),
        (VLLM_MODEL, Dedicated, 20_202_020_202, &[236_511]),
        (VLLM_MODEL, Unified, 40_404_040_404, &[270_843]),
        (NINFER_MODEL, Dedicated, 303_030, &[24_841, 29_860]),
        (
            NINFER_UNIFIED_MODEL,
            Unified,
            21_692_057_050,
            &[24_841, 29_860],
        ),
    ];

    for (model_id, memory_architecture, pool, expected) in cases {
        let target = target(class_of(memory_architecture), "node-a");
        let inventory = observed(&target, memory_architecture, Some(pool), now);
        for context_per_request in [Some(8192), None] {
            let label = format!("{model_id} on {pool} bytes with {context_per_request:?}");
            let solved = largest(&evaluate(
                &catalog,
                model_id,
                Some(scenario(16_384, context_per_request)),
                &[&target],
                &inventory,
                now,
            ));

            assert_eq!(
                solved,
                brute_force_largest(
                    &catalog,
                    model_id,
                    &target,
                    &inventory,
                    now,
                    context_per_request
                ),
                "{label}"
            );
            assert_eq!(
                solved,
                expected
                    .iter()
                    .map(|value| Some(*value))
                    .collect::<Vec<_>>(),
                "{label}"
            );
        }
        // Without a scenario, the context per request floats with the total context too.
        assert_eq!(
            largest(&evaluate(
                &catalog,
                model_id,
                None,
                &[&target],
                &inventory,
                now
            )),
            expected
                .iter()
                .map(|value| Some(*value))
                .collect::<Vec<_>>(),
            "{model_id} without a scenario"
        );
    }
}

#[test]
fn the_largest_fitting_total_context_is_clamped_to_engine_and_derived_bounds() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let unbounded = observed(&target, MemoryArchitecture::Dedicated, Some(1 << 50), now);
    let cases = [
        // The engine's total context maximum.
        (LLAMA_MODEL, &unbounded, None, 16_777_216),
        (LLAMA_MODEL, &unbounded, Some(8192), 16_777_216),
        // vLLM's KV cache memory bound: 500000000 + 65536 × T ≤ 68719476736.
        (VLLM_MODEL, &unbounded, None, 1_040_946),
        // ninfer's 4 parallel requests hold at most 4 × 4096 tokens; a floating context per
        // request stays within its 262144-token range.
        (NINFER_MODEL, &unbounded, Some(4096), 16_384),
        (NINFER_MODEL, &unbounded, None, 262_144),
    ];
    for (model_id, inventory, context_per_request, expected) in cases {
        let label = format!("{model_id} with {context_per_request:?}");
        let solved = largest(&evaluate(
            &catalog,
            model_id,
            Some(scenario(16_384, context_per_request)),
            &[&target],
            inventory,
            now,
        ));

        assert_eq!(
            solved,
            brute_force_largest(
                &catalog,
                model_id,
                &target,
                inventory,
                now,
                context_per_request
            ),
            "{label}"
        );
        assert!(
            solved.iter().all(|value| *value == Some(expected)),
            "{label}: {solved:?}"
        );
    }

    // An estimate that doesn't grow with the total context fits up to the engine maximum.
    let flat = observed(
        &target,
        MemoryArchitecture::Dedicated,
        Some(2_000_000_000),
        now,
    );
    assert_eq!(
        largest(&evaluate(
            &catalog,
            LLAMA_FLAT_MODEL,
            None,
            &[&target],
            &flat,
            now
        )),
        [Some(16_777_216)]
    );
}

#[test]
fn the_largest_fitting_total_context_is_absent_when_unknown_or_nothing_fits() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let read = |pool, scenario| {
        let inventory = observed(&target, MemoryArchitecture::Dedicated, pool, now);
        let response = evaluate(&catalog, LLAMA_MODEL, scenario, &[&target], &inventory, now);
        (
            fits(&response)
                .into_iter()
                .map(|(_, _, fit, _)| fit)
                .collect::<Vec<_>>(),
            largest(&response),
        )
    };

    // An unknown memory total has no fit.
    assert_eq!(read(None, None), (vec![MemoryFit::Unknown], vec![None]));
    // 1201024000 bytes at the smallest 1024-token total are one byte above what the idle
    // 1213155554-byte pool leaves usable after its 1% headroom, and exactly what one more byte
    // leaves; the defaults' larger total context exceeds both.
    assert_eq!(
        read(Some(1_213_155_554), None),
        (vec![MemoryFit::Exceeds], vec![None])
    );
    assert_eq!(
        read(Some(1_213_155_555), None),
        (vec![MemoryFit::Exceeds], vec![Some(1024)])
    );
    // 300000 tokens fit with a floating context per request, below a supplied 400000, in the
    // pool leaving 1500000000 usable bytes.
    assert_eq!(
        read(Some(1_515_151_515), Some(scenario(400_000, Some(400_000)))),
        (vec![MemoryFit::Exceeds], vec![None])
    );
    assert_eq!(
        read(Some(1_515_151_515), Some(scenario(400_000, Some(300_000)))),
        (vec![MemoryFit::Exceeds], vec![Some(300_000)])
    );
}

#[test]
fn the_largest_fitting_total_context_uses_free_memory_less_a_headroom() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let total = 32_607 * MIB;
    let free = 32_102 * MIB;
    let headroom = total / 100;
    // vLLM: 4500000000 + 65536 × T bytes.
    let fit_for = |bytes: u64| (bytes - 4_500_000_000) / 65_536;
    let read = |free, scenario| {
        let inventory = observed_with_free(
            &target,
            MemoryArchitecture::Dedicated,
            Some(total),
            free,
            now,
        );
        let response = evaluate(&catalog, VLLM_MODEL, scenario, &[&target], &inventory, now);
        (
            fits(&response)
                .into_iter()
                .map(|(_, _, fit, _)| fit)
                .collect::<Vec<_>>(),
            largest(&response),
        )
    };

    let expected = u32::try_from(fit_for(free - headroom)).expect("tokens");
    assert_eq!(read(Some(free), None).1, [Some(expected)]);

    // Unknown free memory has no largest fit, never one from the total.
    assert_eq!(read(None, None).1, [None]);

    // Free memory below the estimate at the smallest total leaves nothing that fits.
    assert_eq!(read(Some(4_500_000_000), None).1, [None]);

    // The memory fit keeps comparing with the total: a scenario above the largest fit still fits.
    let on_total = u32::try_from(fit_for(total)).expect("tokens");
    assert!(on_total > expected);
    assert_eq!(
        read(Some(free), Some(scenario(on_total, None))).0,
        [MemoryFit::Fits]
    );
    assert_eq!(
        read(Some(free), Some(scenario(on_total + 1, None))).0,
        [MemoryFit::Exceeds]
    );
}

#[test]
fn the_free_memory_fit_judges_the_same_estimate_against_the_largest_fit_line() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let total = 32_607 * MIB;
    let free = 32_102 * MIB;
    let read = |free, scenario| {
        let inventory = observed_with_free(
            &target,
            MemoryArchitecture::Dedicated,
            Some(total),
            free,
            now,
        );
        let response = evaluate(&catalog, VLLM_MODEL, scenario, &[&target], &inventory, now);
        response
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.memory_fit(),
                    entry.free_memory_fit(),
                    entry.largest_total_context_tokens,
                )
            })
            .collect::<Vec<_>>()
    };
    let largest = read(Some(free), None)[0].2.expect("largest fit");

    // The largest fit is the last total that fits free memory; one more fits only the total.
    assert_eq!(
        read(Some(free), Some(scenario(largest, None))),
        [(MemoryFit::Fits, MemoryFit::Fits, Some(largest))]
    );
    assert_eq!(
        read(Some(free), Some(scenario(largest + 1, None))),
        [(MemoryFit::Fits, MemoryFit::Exceeds, Some(largest))]
    );

    // Unknown free memory leaves the total fit standing.
    assert_eq!(
        read(None, Some(scenario(largest, None))),
        [(MemoryFit::Fits, MemoryFit::Unknown, None)]
    );
}

#[test]
fn a_stale_target_is_unknown_with_no_largest_fit() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let inventory = observed_with_free(
        &target,
        MemoryArchitecture::Dedicated,
        Some(32_607 * MIB),
        Some(32_102 * MIB),
        now - Duration::from_secs(3600),
    );

    let response = evaluate(&catalog, LLAMA_MODEL, None, &[&target], &inventory, now);

    assert_eq!(
        response
            .targets
            .iter()
            .map(|target| target.status())
            .collect::<Vec<_>>(),
        [ModelCompatibilityStatus::Unknown]
    );
    assert_eq!(largest(&response), Vec::<Option<u32>>::new());
}

#[test]
fn a_single_micro_batch_profile_sizes_and_fits_within_its_configuration_maximum() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();

    // The engine defaults the total context to the 16384-token configuration maximum.
    assert_eq!(
        profiles(&catalog, LLAMA_MICRO_BATCH_MODEL, None),
        [supported(
            LLAMA_MICRO_BATCH_MODEL,
            "chat",
            [
                ("prompt-cache-memory-mib", 1024),
                ("total-context", 16_384),
                ("parallel-requests", 4),
                ("context-per-request", 16_384),
            ],
            ["dedicated", "unified"]
                .map(|class| class_estimate(
                    class,
                    1_216_384_000,
                    Some(1024 * MIB),
                    [("context-per-request", 16_384)]
                ))
                .to_vec(),
        )]
    );
    assert_eq!(
        profiles(
            &catalog,
            LLAMA_MICRO_BATCH_MODEL,
            Some(scenario(16_385, None))
        )
        .iter()
        .map(ModelProfileSizing::unsupported_reason)
        .collect::<Vec<_>>(),
        [SizingScenarioUnsupportedReason::ParameterOutOfRange]
    );

    // Unbounded memory fits up to the maximum, and 1200000000 + 1000 × T bytes fit 8192 tokens
    // in the idle 1220395959-byte pool, which leaves 1208192000 bytes after its 1% headroom.
    let target = target("dedicated", "node-a");
    for (pool, expected) in [(1 << 50, 16_384), (1_220_395_959, 8192)] {
        let inventory = observed(&target, MemoryArchitecture::Dedicated, Some(pool), now);
        for scenario in [None, Some(scenario(4096, None))] {
            assert_eq!(
                largest(&evaluate(
                    &catalog,
                    LLAMA_MICRO_BATCH_MODEL,
                    scenario,
                    &[&target],
                    &inventory,
                    now,
                )),
                [Some(expected)],
                "{pool} bytes with {scenario:?}"
            );
        }
        assert_eq!(
            brute_force_largest(
                &catalog,
                LLAMA_MICRO_BATCH_MODEL,
                &target,
                &inventory,
                now,
                None
            ),
            [Some(expected)],
            "{pool} bytes"
        );
    }
}

#[test]
fn profiles_without_an_estimate_have_unknown_fit_and_profiles_that_cant_run_have_none() {
    let catalog = scenario_catalog();
    let now = SystemTime::now();
    let target = target("dedicated", "node-a");
    let two_devices = ExecutionTarget {
        accelerator_device_ids: vec!["0".to_owned(), "1".to_owned()],
        ..self::target("two-devices", "node-b")
    };
    let mut inventory = observed(&target, MemoryArchitecture::Dedicated, Some(1 << 40), now);
    inventory.extend(observed(
        &two_devices,
        MemoryArchitecture::Dedicated,
        Some(1 << 40),
        now,
    ));

    for scenario in [None, Some(scenario(4096, None))] {
        let response = evaluate(
            &catalog,
            UNSIZED_MODEL,
            scenario,
            &[&target, &two_devices],
            &inventory,
            now,
        );

        assert_eq!(
            response.entries,
            [
                ModelProfileCompatibility {
                    model_id: UNSIZED_MODEL.to_owned(),
                    profile_id: "chat".to_owned(),
                    target: Some(placement(&target)),
                    status: ModelCompatibilityStatus::Runs as i32,
                    reason: None,
                    advisories: Vec::new(),
                    hardware_class_id: "dedicated".to_owned(),
                    memory_fit: MemoryFit::Unknown as i32,
                    free_memory_fit: MemoryFit::Unknown as i32,
                    unsupported_reason: SizingScenarioUnsupportedReason::Unspecified as i32,
                    largest_total_context_tokens: None,
                    starting_total_context_tokens: Some(32_768),
                    starting_total_context_source: StartingTotalContextSource::ProfileDefault
                        as i32,
                },
                ModelProfileCompatibility {
                    model_id: UNSIZED_MODEL.to_owned(),
                    profile_id: "chat".to_owned(),
                    target: Some(placement(&two_devices)),
                    status: ModelCompatibilityStatus::CannotRun as i32,
                    reason: Some(ModelCompatibilityReason {
                        code: ModelCompatibilityReasonCode::AcceleratorDeviceCountMismatch as i32,
                        accelerator_device_count: 2,
                        required_accelerator_device_count: 1,
                        ..Default::default()
                    }),
                    advisories: Vec::new(),
                    hardware_class_id: String::new(),
                    memory_fit: MemoryFit::Unspecified as i32,
                    free_memory_fit: MemoryFit::Unspecified as i32,
                    unsupported_reason: SizingScenarioUnsupportedReason::Unspecified as i32,
                    largest_total_context_tokens: None,
                    starting_total_context_tokens: None,
                    starting_total_context_source: StartingTotalContextSource::Unspecified as i32,
                },
            ],
            "{scenario:?}"
        );
    }
}

#[test]
fn profiles_keep_their_class_estimates_when_no_target_reports() {
    let catalog = scenario_catalog();
    let observed_at = SystemTime::now();
    let target = target("dedicated", "node-a");
    let inventory = observed(
        &target,
        MemoryArchitecture::Dedicated,
        Some(1 << 40),
        observed_at,
    );
    // Observations expire after 60 seconds.
    let now = observed_at + Duration::from_secs(61);

    let response = evaluate(
        &catalog,
        LLAMA_MODEL,
        Some(scenario(16_384, None)),
        &[&target],
        &inventory,
        now,
    );

    assert_eq!(
        response
            .targets
            .iter()
            .map(|target| (target.target.clone(), target.status()))
            .collect::<Vec<_>>(),
        [(Some(placement(&target)), ModelCompatibilityStatus::Unknown)]
    );
    assert_eq!(response.entries, []);
    assert_eq!(
        response
            .profiles
            .iter()
            .map(|profile| profile.hardware_class_estimates.clone())
            .collect::<Vec<_>>(),
        [["dedicated", "unified"]
            .map(|class| class_estimate(
                class,
                1_216_384_000,
                Some(1024 * MIB),
                [("context-per-request", 16_384)]
            ))
            .to_vec()]
    );
}

#[test]
fn without_a_scenario_out_of_range_classes_are_left_out_without_a_reason() {
    let catalog = scenario_catalog();

    // The 32768-token default total context derives 2647483648 bytes of KV cache on the
    // dedicated class and 137938953472 bytes, above the 64 GiB maximum, on the unified one.
    assert_eq!(
        profiles(&catalog, VLLM_LONG_MODEL, None),
        [supported(
            VLLM_LONG_MODEL,
            "text",
            [
                ("maximum-batched-tokens", 1024),
                ("maximum-cudagraph-capture-size", 16),
                ("kv-offloading-size-gib", 0),
                ("total-context", 32_768),
                ("parallel-requests", 4),
                ("context-per-request", 32_768),
            ],
            vec![class_estimate(
                "dedicated",
                6_647_483_648,
                None,
                [
                    ("context-per-request", 32_768),
                    ("kv-cache-memory-bytes", 2_647_483_648)
                ]
            )],
        )]
    );
}

#[test]
fn classes_whose_sizing_fails_dont_keep_a_scenario_supported() {
    let catalog = scenario_catalog();

    // A total context of 2097152 tokens derives a KV cache above 64 GiB on the dedicated
    // class, and the unified class's sizing overflows, so no class can serve the scenario.
    assert_eq!(
        profiles(
            &catalog,
            VLLM_OVERFLOW_MODEL,
            Some(scenario(2_097_152, Some(262_144)))
        ),
        [ModelProfileSizing {
            model_id: VLLM_OVERFLOW_MODEL.to_owned(),
            profile_id: "text".to_owned(),
            engine_parameter_values: None,
            hardware_class_estimates: Vec::new(),
            unsupported_reason: SizingScenarioUnsupportedReason::DerivedParameterOutOfRange as i32,
            unsupported_hardware_class_ids: vec!["dedicated".to_owned()],
        }]
    );
}

#[test]
fn memory_fit_judges_target_freshness_at_the_evaluation_time() {
    let catalog = scenario_catalog();
    // Stale by the wall clock but fresh at the evaluation time.
    let observed_at = SystemTime::now() - Duration::from_secs(300);
    let now = observed_at + Duration::from_secs(1);
    let target = target("dedicated", "node-a");
    let inventory = observed(
        &target,
        MemoryArchitecture::Dedicated,
        Some(1_300_000_000),
        observed_at,
    );

    let response = evaluate(
        &catalog,
        LLAMA_MODEL,
        Some(scenario(16_384, None)),
        &[&target],
        &inventory,
        now,
    );

    assert_eq!(
        response
            .entries
            .iter()
            .map(|entry| (entry.status(), entry.reason.clone()))
            .collect::<Vec<_>>(),
        [(ModelCompatibilityStatus::Runs, None)]
    );
    assert_eq!(
        fits(&response),
        [(
            placement(&target),
            "dedicated".to_owned(),
            MemoryFit::Fits,
            SizingScenarioUnsupportedReason::Unspecified
        )]
    );
}

#[test]
fn scenarios_without_a_shared_context_definition_leave_the_profile_unsized() {
    let catalog = scenario_catalog();
    let declarations = |model_id: &str, profile_id: &str, binding_id: &str| {
        catalog
            .local_model_declarations(&CatalogModelId(model_id.to_owned()), profile_id, binding_id)
            .expect("declarations")
    };
    let defaults =
        |declarations: &CatalogLocalModelDeclarations| LocalModelDeploymentConfiguration {
            model_artifact_binding_id: declarations.binding.binding_id.clone(),
            engine_parameter_values: declarations.profile.engine_parameter_defaults.clone(),
            ..Default::default()
        };
    let without = |parameter_id: &str| {
        let mut declarations = declarations(LLAMA_MODEL, "chat", "weights");
        declarations.approved_engine_artifacts[0]
            .engine_release
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .retain(|definition| definition.parameter_id != parameter_id);
        declarations
    };

    // A total context or a supplied context per request without a definition can't be
    // checked.
    for (parameter_id, scenario) in [
        ("total-context", scenario(8192, None)),
        ("context-per-request", scenario(8192, Some(4096))),
    ] {
        let declarations = without(parameter_id);
        assert_eq!(
            with_scenario_values(&declarations, defaults(&declarations), &scenario),
            None,
            "{parameter_id}"
        );
    }

    // The unchanged declarations take it, leaving the context per request for resolution to
    // fill.
    let llama = declarations(LLAMA_MODEL, "chat", "weights");
    assert_eq!(
        with_scenario_values(&llama, defaults(&llama), &scenario(8192, None))
            .and_then(Result::ok)
            .and_then(|configuration| configuration.engine_parameter_values),
        Some(unsigned_parameter_values([
            ("prompt-cache-memory-mib", 1024),
            ("total-context", 8192),
        ]))
    );
}

const LLAMA_FLAT_MODEL: &str = "llama-flat-model";
const LLAMA_MODEL: &str = "llama-model";
const LLAMA_MICRO_BATCH_MODEL: &str = "llama-micro-batch-model";
const UNSIZED_MODEL: &str = "unsized-model";
const VLLM_CAPPED_MODEL: &str = "vllm-capped-model";
const VLLM_LONG_MODEL: &str = "vllm-long-model";
const VLLM_OVERFLOW_MODEL: &str = "vllm-overflow-model";
const VLLM_MODEL: &str = "vllm-model";
const NINFER_MODEL: &str = "ninfer-model";
const NINFER_UNIFIED_MODEL: &str = "ninfer-unified-model";

fn scenario(total_context_tokens: u32, context_tokens_per_request: Option<u32>) -> SizingScenario {
    SizingScenario::new(total_context_tokens, context_tokens_per_request).expect("scenario")
}

/// Profile sizings of one model of `catalog` without any target.
fn profiles(
    catalog: &Catalog,
    model_id: &str,
    scenario: Option<SizingScenario>,
) -> Vec<ModelProfileSizing> {
    evaluate(
        catalog,
        model_id,
        scenario,
        &[],
        &TargetCapabilityInventory::default(),
        SystemTime::now(),
    )
    .profiles
}

/// Evaluates one model of `catalog` on `targets`.
fn evaluate(
    catalog: &Catalog,
    model_id: &str,
    scenario: Option<SizingScenario>,
    targets: &[&ExecutionTarget],
    inventory: &TargetCapabilityInventory,
    now: SystemTime,
) -> ListModelProfileCompatibilityResponse {
    let models = catalog
        .list_model_releases()
        .into_iter()
        .filter(|model| model.model_id == model_id)
        .collect::<Vec<Model>>();
    assert_eq!(models.len(), 1, "{model_id}");
    evaluate_profile_compatibility(
        catalog,
        &models,
        &targets
            .iter()
            .map(|target| (*target).clone())
            .collect::<Vec<_>>(),
        scenario.as_ref(),
        Some(inventory),
        &Default::default(),
        now,
    )
}

/// Target, hardware class, memory fit and unsupported reason of every entry.
fn fits(
    response: &ListModelProfileCompatibilityResponse,
) -> Vec<(
    ExactPlacement,
    String,
    MemoryFit,
    SizingScenarioUnsupportedReason,
)> {
    response
        .entries
        .iter()
        .map(|entry| {
            (
                entry.target.clone().expect("entry target"),
                entry.hardware_class_id.clone(),
                entry.memory_fit(),
                entry.unsupported_reason(),
            )
        })
        .collect()
}

/// Largest fitting total context of every entry.
fn largest(response: &ListModelProfileCompatibilityResponse) -> Vec<Option<u32>> {
    response
        .entries
        .iter()
        .map(|entry| entry.largest_total_context_tokens)
        .collect()
}

/// Largest total context whose scenario read fits the one target's free memory, the line the
/// largest fit fills, per profile, found by bisecting over reads: the totals that fit form one
/// interval starting at the smallest accepted total.
fn brute_force_largest(
    catalog: &Catalog,
    model_id: &str,
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
    now: SystemTime,
    context_per_request: Option<u32>,
) -> Vec<Option<u32>> {
    let fit_per_profile = |total_context: u32| {
        evaluate(
            catalog,
            model_id,
            Some(scenario(total_context, context_per_request)),
            &[target],
            inventory,
            now,
        )
        .entries
        .iter()
        .map(|entry| entry.free_memory_fit() == MemoryFit::Fits)
        .collect::<Vec<_>>()
    };
    let lowest = context_per_request.unwrap_or(1024).max(1024);
    let highest = 16_777_216;
    let profiles = fit_per_profile(lowest).len();
    (0..profiles)
        .map(|profile| {
            let fits = |total_context| fit_per_profile(total_context)[profile];
            if !fits(lowest) {
                return None;
            }
            if fits(highest) {
                return Some(highest);
            }
            let (mut fitting, mut exceeding) = (lowest, highest);
            while exceeding - fitting > 1 {
                let middle = fitting + (exceeding - fitting) / 2;
                if fits(middle) {
                    fitting = middle;
                } else {
                    exceeding = middle;
                }
            }
            Some(fitting)
        })
        .collect()
}

fn class_of(memory_architecture: MemoryArchitecture) -> &'static str {
    match memory_architecture {
        MemoryArchitecture::Dedicated => "dedicated",
        MemoryArchitecture::Unified => "unified",
    }
}

fn supported<const N: usize>(
    model_id: &str,
    profile_id: &str,
    values: [(&str, u64); N],
    hardware_class_estimates: Vec<HardwareClassResourceEstimate>,
) -> ModelProfileSizing {
    ModelProfileSizing {
        model_id: model_id.to_owned(),
        profile_id: profile_id.to_owned(),
        engine_parameter_values: Some(unsigned_parameter_values(values)),
        hardware_class_estimates,
        unsupported_reason: SizingScenarioUnsupportedReason::Unspecified as i32,
        unsupported_hardware_class_ids: Vec::new(),
    }
}

fn class_estimate<'a>(
    hardware_class_id: &str,
    accelerator_memory_bytes: u64,
    host_memory_bytes: Option<u64>,
    derived: impl IntoIterator<Item = (&'a str, u64)>,
) -> HardwareClassResourceEstimate {
    let estimate = |resource: &str, value| ScalarResourceEstimate {
        resource: resource.to_owned(),
        value,
        unit: "bytes".to_owned(),
    };
    HardwareClassResourceEstimate {
        hardware_class_id: hardware_class_id.to_owned(),
        estimates: [Some(estimate(
            "acceleratorMemory",
            accelerator_memory_bytes,
        ))]
        .into_iter()
        .chain([host_memory_bytes.map(|value| estimate("hostMemory", value))])
        .flatten()
        .collect(),
        derived_engine_parameter_values: Some(unsigned_parameter_values(derived)),
        launch_rejection: None,
    }
}

/// Host memory beside a dedicated test device: idle, and larger than any host estimate here.
const IDLE_HOST_BYTES: u64 = 1 << 40;

fn target(target_id: &str, node_id: &str) -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: target_id.to_owned(),
        display_name: target_id.to_owned(),
        site_node_id: node_id.to_owned(),
        platform: Some(ExecutionPlatform::linux_amd64().to_oci_platform()),
        accelerator_device_ids: vec!["0".to_owned()],
        runtime_binding: Some(crate::test_support::docker_model_runtime_binding()),
    }
}

/// Inventory for one linux/amd64 NVIDIA target observed at `now`, with nothing in use.
fn observed(
    target: &ExecutionTarget,
    memory_architecture: MemoryArchitecture,
    memory_pool_total_bytes: Option<u64>,
    now: SystemTime,
) -> TargetCapabilityInventory {
    observed_with_free(
        target,
        memory_architecture,
        memory_pool_total_bytes,
        memory_pool_total_bytes,
        now,
    )
}

/// Inventory for one linux/amd64 NVIDIA target observed at `now`, with an explicit free memory.
fn observed_with_free(
    target: &ExecutionTarget,
    memory_architecture: MemoryArchitecture,
    memory_pool_total_bytes: Option<u64>,
    memory_pool_free_bytes: Option<u64>,
    now: SystemTime,
) -> TargetCapabilityInventory {
    TargetCapabilityReconciler::reconcile(
        [target],
        [RuntimePlatformObservation::available_at(
            &target.execution_target_id,
            ExecutionPlatform::linux_amd64(),
            now,
        )],
        [NodeAcceleratorObservation::available_at(
            &target.site_node_id,
            memory_facts(
                memory_architecture,
                memory_pool_total_bytes,
                memory_pool_free_bytes,
            ),
            now,
        )
        .with_memory_architecture(Some(memory_architecture))],
    )
}

/// NVIDIA facts of a node whose one device, beside an idle host, or host on unified memory, has
/// `total` and `free` memory; an unknown total leaves the memory unreported.
fn memory_facts(
    memory_architecture: MemoryArchitecture,
    total: Option<u64>,
    free: Option<u64>,
) -> NodeAcceleratorFacts {
    let facts = NodeAcceleratorFacts::nvidia("580.173.02");
    match (memory_architecture, total) {
        (_, None) => facts,
        (MemoryArchitecture::Dedicated, Some(total)) => facts
            .with_device_memory("GPU-0", Some(total), free.map(|free| total - free))
            .with_host_memory(IDLE_HOST_BYTES, IDLE_HOST_BYTES),
        (MemoryArchitecture::Unified, Some(total)) => {
            facts.with_host_memory(total, free.expect("unified free memory"))
        }
    }
}

fn placement(target: &ExecutionTarget) -> ExactPlacement {
    ExactPlacement {
        site_node_id: target.site_node_id.clone(),
        execution_target_id: target.execution_target_id.clone(),
    }
}

/// Loads a catalog with one sized model per engine, an unsized llama.cpp model, and NVIDIA
/// dedicated and unified hardware classes.
fn scenario_catalog() -> Catalog {
    let root = tempfile::tempdir().expect("catalog root");
    for (path, content) in [
        ("hardware-classes/dedicated.yaml", DEDICATED_CLASS),
        ("hardware-classes/unified.yaml", UNIFIED_CLASS),
        ("engines/llama.yaml", LLAMA_ENGINE),
        ("engines/vllm.yaml", VLLM_ENGINE),
        ("engines/ninfer.yaml", NINFER_ENGINE),
        ("models/llama.yaml", LLAMA_MODEL_DECLARATION),
        ("models/unsized.yaml", UNSIZED_MODEL_DECLARATION),
        ("models/vllm.yaml", VLLM_MODEL_DECLARATION),
        ("models/ninfer.yaml", NINFER_MODEL_DECLARATION),
    ] {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().expect("directory")).expect("directory");
        std::fs::write(path, content).expect("declaration");
    }
    // A vLLM variant whose configuration lowers the maximum context, one whose default total
    // context derives a KV cache above 64 GiB on the unified class only, and one whose
    // unified sizing overflows.
    let variant_of = |declaration: &str, base_id: &str, model_id: &str, from: &str, to: &str| {
        assert!(declaration.contains(from), "{model_id} variant");
        declaration.replace(base_id, model_id).replace(from, to)
    };
    let variant = |model_id: &str, from: &str, to: &str| {
        variant_of(VLLM_MODEL_DECLARATION, VLLM_MODEL, model_id, from, to)
    };
    // A llama.cpp variant whose estimate doesn't grow with the total context, one that processes
    // prompts in a single micro-batch below a 16384-token configuration maximum, and a ninfer
    // variant that also supports the unified class.
    for (path, content) in [
        (
            "models/llama-micro-batch.yaml",
            variant_of(
                LLAMA_MODEL_DECLARATION,
                LLAMA_MODEL,
                LLAMA_MICRO_BATCH_MODEL,
                "  - configurationId: \"llama\"\n",
                "  - configurationId: \"llama\"\n    maximumContextTokens: 16384\n",
            )
            .replace(
                "fixedSettings: { gpuLayers: 0 }",
                "fixedSettings: { gpuLayers: 0, singleMicroBatch: true }",
            ),
        ),
        (
            "models/llama-flat.yaml",
            variant_of(
                LLAMA_MODEL_DECLARATION,
                LLAMA_MODEL,
                LLAMA_FLAT_MODEL,
                "bytesPerContextToken: 1000",
                "bytesPerContextToken: 0",
            ),
        ),
        (
            "models/ninfer-unified.yaml",
            variant_of(
                NINFER_MODEL_DECLARATION,
                NINFER_MODEL,
                NINFER_UNIFIED_MODEL,
                "      - hardwareClassId: \"dedicated\"\n        bindingSizing: [{ bindingId: \"weights\" }]\n",
                "      - hardwareClassId: \"dedicated\"\n        bindingSizing: [{ bindingId: \"weights\" }]\n      - hardwareClassId: \"unified\"\n        bindingSizing: [{ bindingId: \"weights\" }]\n",
            ),
        ),
    ] {
        std::fs::write(root.path().join(path), content).expect("variant declaration");
    }
    for (path, content) in [
        (
            "models/vllm-capped.yaml",
            variant(
                VLLM_CAPPED_MODEL,
                "  - configurationId: \"vllm\"\n",
                "  - configurationId: \"vllm\"\n    maximumContextTokens: 65536\n",
            ),
        ),
        (
            "models/vllm-long.yaml",
            variant(
                VLLM_LONG_MODEL,
                "{ kvBytesPerToken: 131072 }",
                "{ kvBytesPerToken: 4194304 }",
            ),
        ),
        (
            "models/vllm-overflow.yaml",
            variant(
                VLLM_OVERFLOW_MODEL,
                "{ kvBytesPerToken: 131072 }",
                "{ kvBytesPerToken: 1152921504606846976 }",
            ),
        ),
    ] {
        std::fs::write(root.path().join(path), content).expect("variant declaration");
    }
    Catalog::load_with_policy(
        root.path(),
        OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("scenario catalog")
}

const DEDICATED_CLASS: &str = r#"
hardwareClassId: "dedicated"
displayName: "Dedicated"
selector:
  accelerators: ["nvidia"]
  memoryArchitecture: "ACCELERATOR_MEMORY_ARCHITECTURE_DEDICATED"
"#;

const UNIFIED_CLASS: &str = r#"
hardwareClassId: "unified"
displayName: "Unified"
selector:
  accelerators: ["nvidia"]
  memoryArchitecture: "ACCELERATOR_MEMORY_ARCHITECTURE_UNIFIED"
"#;

const LLAMA_ENGINE: &str = r#"
engineReleaseId: "llama"
engineKind: "ENGINE_KIND_LLAMA_CPP"
version: "1"
parameterSchema:
  schemaId: "llama-v2"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, unsignedInteger: { minimum: "1", maximum: "64", unit: "requests", defaultValue: "4" } }
    - { parameterId: "prompt-cache-memory-mib", displayName: "Prompt cache memory", unsignedInteger: { minimum: "0", maximum: "65536", unit: "MiB" } }
artifacts:
  - artifactId: "linux"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "llama" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

/// llama.cpp model whose maximum context is above a 64th of the engine's total context range,
/// so a scenario can map a total context outside it.
const LLAMA_MODEL_DECLARATION: &str = r#"
modelId: "llama-model"
displayName: "llama.cpp model"
version: "1"
maximumContextTokens: 524288
artifacts:
  - artifactId: "weights"
    weightQuantization: "Q8_0"
    source:
      repository: "dotcore/llama"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "weights-Q8_0.gguf"
        provenanceDigest: "0000000000000000000000000000000000000000000000000000000000000000"
configurations:
  - configurationId: "llama"
    engineArtifacts: [{ engineReleaseId: "llama", engineArtifactId: "linux" }]
    artifactBindings: [{ bindingId: "weights", modelArtifactId: "weights" }]
    sizing:
      parameters: { baseBytes: 1000000000, bytesPerContextToken: 1000, slotStateBytes: 50000000 }
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: >-
            ${baseBytes} + ${bytesPerContextToken} * ${total-context}
            + ${parallel-requests} * ${slotStateBytes}
        hostMemory:
          unit: bytes
          equation: "${prompt-cache-memory-mib} * MiB"
    hardwareSupport:
      - hardwareClassId: "dedicated"
        bindingSizing: [{ bindingId: "weights" }]
      - hardwareClassId: "unified"
        bindingSizing: [{ bindingId: "weights" }]
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: { fixedSettings: { gpuLayers: 0 } }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
profiles:
  - profileId: "chat"
    displayName: "Chat"
    configurationId: "llama"
    defaultModelArtifactBindingId: "weights"
    engineParameterDefaults:
      values:
        - { parameterId: "prompt-cache-memory-mib", unsignedInteger: "1024" }
"#;

const UNSIZED_MODEL_DECLARATION: &str = r#"
modelId: "unsized-model"
displayName: "Unsized model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "weights"
    weightQuantization: "Q8_0"
    source:
      repository: "dotcore/unsized"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "weights-Q8_0.gguf"
        provenanceDigest: "0000000000000000000000000000000000000000000000000000000000000000"
configurations:
  - configurationId: "llama"
    engineArtifacts: [{ engineReleaseId: "llama", engineArtifactId: "linux" }]
    artifactBindings: [{ bindingId: "weights", modelArtifactId: "weights" }]
    hardwareSupport:
      - hardwareClassId: "dedicated"
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: { fixedSettings: { gpuLayers: 0 } }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
profiles:
  - profileId: "chat"
    displayName: "Chat"
    configurationId: "llama"
    defaultModelArtifactBindingId: "weights"
    engineParameterDefaults:
      values:
        - { parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }
"#;

const VLLM_ENGINE: &str = r#"
engineReleaseId: "vllm"
engineKind: "ENGINE_KIND_VLLM"
version: "1"
parameterSchema:
  schemaId: "vllm-v2"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, unsignedInteger: { minimum: "1024", maximum: "1048576", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, unsignedInteger: { minimum: "1", maximum: "1024", unit: "requests", defaultValue: "4" } }
    - { parameterId: "maximum-batched-tokens", displayName: "Maximum batched tokens", unsignedInteger: { minimum: "1", maximum: "1048576", unit: "tokens" } }
    - { parameterId: "maximum-cudagraph-capture-size", displayName: "Maximum CUDA graph capture size", unsignedInteger: { minimum: "1", maximum: "1024", unit: "sequences" } }
    - { parameterId: "kv-cache-memory-bytes", displayName: "KV cache memory", unsignedInteger: { minimum: "1", maximum: "68719476736", unit: "bytes" } }
    - { parameterId: "kv-offloading-size-gib", displayName: "KV offloading size", unsignedInteger: { minimum: "0", maximum: "1024", unit: "GiB" } }
artifacts:
  - artifactId: "linux"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "vllm" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

const VLLM_MODEL_DECLARATION: &str = r#"
modelId: "vllm-model"
displayName: "vLLM model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "snapshot"
    source:
      repository: "dotcore/vllm"
      revision: "0123456789abcdef0123456789abcdef01234567"
      snapshot:
        files:
          - { path: "config.json", provenanceDigest: "1111111111111111111111111111111111111111111111111111111111111111" }
          - { path: "model.safetensors", provenanceDigest: "2222222222222222222222222222222222222222222222222222222222222222" }
auxiliaryArtifacts:
  - auxiliaryArtifactId: "chat-template"
    source:
      repository: "dotcore/vllm"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file: { path: "chat_template.jinja", provenanceDigest: "3333333333333333333333333333333333333333333333333333333333333333" }
configurations:
  - configurationId: "vllm"
    engineArtifacts: [{ engineReleaseId: "vllm", engineArtifactId: "linux" }]
    artifactBindings: [{ bindingId: "snapshot", modelArtifactId: "snapshot", auxiliaryArtifactIds: ["chat-template"] }]
    sizing:
      classParameters: ["kvBytesPerToken"]
      parameters: { baseBytes: 4000000000, kvBaseBytes: 500000000 }
      derivedParameters:
        kv-cache-memory-bytes: >-
          ${kvBaseBytes} + ${kvBytesPerToken} * ${total-context}
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + ${kv-cache-memory-bytes}"
    hardwareSupport:
      - hardwareClassId: "dedicated"
        parameters: { kvBytesPerToken: 65536 }
        bindingSizing: [{ bindingId: "snapshot" }]
      - hardwareClassId: "unified"
        parameters: { kvBytesPerToken: 131072 }
        bindingSizing: [{ bindingId: "snapshot" }]
    engineConfiguration:
      engineKind: "ENGINE_KIND_VLLM"
      vllm:
        fixedSettings:
          chatTemplateAuxiliaryArtifactId: "chat-template"
          kvCacheDataType: "VLLM_KV_CACHE_DATA_TYPE_FP8"
          kvCacheMemoryBytesParameterId: "kv-cache-memory-bytes"
          maximumBatchedTokensParameterId: "maximum-batched-tokens"
          maximumCudaGraphCaptureSizeParameterId: "maximum-cudagraph-capture-size"
          kvOffloadingSizeGibParameterId: "kv-offloading-size-gib"
          kvOffloadingBackend: "VLLM_KV_OFFLOADING_BACKEND_NATIVE"
          containerPort: 15000
          readinessPolicy: { mode: "VLLM_READINESS_MODE_MODELS" }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
profiles:
  - profileId: "text"
    displayName: "Text"
    configurationId: "vllm"
    defaultModelArtifactBindingId: "snapshot"
    engineParameterDefaults:
      values:
        - { parameterId: "maximum-batched-tokens", unsignedInteger: "1024" }
        - { parameterId: "maximum-cudagraph-capture-size", unsignedInteger: "16" }
        - { parameterId: "kv-offloading-size-gib", unsignedInteger: "0" }
"#;

const NINFER_ENGINE: &str = r#"
engineReleaseId: "ninfer"
engineKind: "ENGINE_KIND_NINFER"
version: "1"
parameterSchema:
  schemaId: "ninfer-v2"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, unsignedInteger: { minimum: "1024", maximum: "2097152", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, unsignedInteger: { minimum: "1024", maximum: "262144", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, unsignedInteger: { minimum: "1", maximum: "8", unit: "requests", defaultValue: "4" } }
    - { parameterId: "host-context-mib", displayName: "Host context memory", unsignedInteger: { minimum: "0", maximum: "1048576", unit: "MiB" } }
    - { parameterId: "device-state-slots", displayName: "Device state slots", unsignedInteger: { minimum: "0", maximum: "64", unit: "slots" } }
artifacts:
  - artifactId: "linux"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "ninfer" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

/// ninfer model whose maximum context is above the engine's context per request range, so a
/// scenario can map a context per request outside it.
const NINFER_MODEL_DECLARATION: &str = r#"
modelId: "ninfer-model"
displayName: "ninfer model"
version: "1"
maximumContextTokens: 524288
options:
  - { optionId: "mtp", displayName: "MTP", speculativeDecoding: { method: "SPECULATIVE_DECODING_METHOD_MTP" } }
artifacts:
  - artifactId: "weights"
    weightQuantization: "NVFP4"
    source:
      repository: "dotcore/ninfer"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "weights_nvfp4.ninfer"
        provenanceDigest: "74d2c57145e6ff11d1d2faa79594477f9bc903a611af1fb20218189fbbb77d82"
configurations:
  - configurationId: "ninfer"
    engineArtifacts: [{ engineReleaseId: "ninfer", engineArtifactId: "linux" }]
    artifactBindings: [{ bindingId: "weights", modelArtifactId: "weights" }]
    sizing:
      parameters: { baseBytes: 1000, bytesPerKvToken: 10, bytesPerConcurrency: 100, mtpBaseBytes: 500, mtpBytesPerKvToken: 2 }
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: >-
            ${baseBytes} + ${bytesPerKvToken} * ${total-context}
            + ${bytesPerConcurrency} * ${parallel-requests}
            + if(${options.mtp.token-count} > 0, 1, 0) * (${mtpBaseBytes} + ${mtpBytesPerKvToken} * ${total-context})
        hostMemory:
          unit: bytes
          equation: "${host-context-mib} * MiB"
    hardwareSupport:
      - hardwareClassId: "dedicated"
        bindingSizing: [{ bindingId: "weights" }]
    engineConfiguration:
      engineKind: "ENGINE_KIND_NINFER"
      ninfer:
        fixedSettings:
          kvCacheDataType: "NINFER_KV_CACHE_DATA_TYPE_FP8"
          pendingTimeoutMilliseconds: 120000
          preserveThinking: true
          deviceStateSlotsParameterId: "device-state-slots"
          hostContextMibParameterId: "host-context-mib"
        optionRealizations:
          - optionId: "mtp"
            mtp: { lmHeadDraft: true }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
    options:
      - optionId: "mtp"
        parameters:
          - { parameterId: "token-count", displayName: "Speculative tokens", unsignedInteger: { minimum: "0", maximum: "5", unit: "tokens" } }
        defaultValues:
          optionId: "mtp"
          values:
            - { parameterId: "token-count", unsignedInteger: "3" }
profiles:
  - profileId: "mtp"
    displayName: "MTP"
    configurationId: "ninfer"
    defaultModelArtifactBindingId: "weights"
    engineParameterDefaults:
      values:
        - { parameterId: "parallel-requests", unsignedInteger: "4" }
        - { parameterId: "host-context-mib", unsignedInteger: "20480" }
        - { parameterId: "device-state-slots", unsignedInteger: "4" }
  - profileId: "no-mtp"
    displayName: "No MTP"
    configurationId: "ninfer"
    defaultModelArtifactBindingId: "weights"
    engineParameterDefaults:
      values:
        - { parameterId: "parallel-requests", unsignedInteger: "4" }
        - { parameterId: "host-context-mib", unsignedInteger: "20480" }
        - { parameterId: "device-state-slots", unsignedInteger: "4" }
    optionDefaults:
      - optionId: "mtp"
        values:
          - { parameterId: "token-count", unsignedInteger: "0" }
"#;
