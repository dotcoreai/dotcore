use dotcore_catalog::test_support::{
    SIZING_ENGINE_CACHE_DEFAULT, create_empty_catalog_source, temporary_catalog_root,
    write_sizing_engine, write_sizing_model_with,
};
use dotcore_proto::{
    dotcore::{
        catalog::v1::{
            EngineParameterValue, ModelOptionParameterValue, ModelOptionValues,
            model_option_parameter_value,
        },
        telemetry::v1::{
            WorkloadRealizationKey, WorkloadResourceDevice, WorkloadResourceInputs,
            WorkloadResourceNode, WorkloadResourceObservation,
        },
    },
    google::protobuf::Timestamp,
};

use super::*;

const BASE: u64 = 1_000_000;
const PER_TOKEN: u64 = 36_864;

const NVIDIA_AMD64: &str = "nvidia-amd64";
const AMD64_BASE: u64 = 1_300_000;
const AMD64_PER_TOKEN: u64 = 40_960;

const VISION_BASE: u64 = 1_700_000;
const VISION_GROWTH: u64 = 400;
const VISION_PROJECTOR: &str = "vision-projector";
const LINEAR_EQUATION: &str = "${base} + ${perToken} * ${total-context}";
const PER_TOKEN_Q8: u64 = 19_584;
const KV_CACHE_EQUATION: &str = r#"${base} + if(${options.kv-cache.precision} == \"q8_0\", 0, 1) * ${perToken} * ${total-context} + if(${options.kv-cache.precision} == \"q8_0\", 1, 0) * ${perTokenQ8} * ${total-context}"#;
const Q8_OFFSET: u64 = 67_000_000;
const VISION_EQUATION: &str = "if(${options.vision.enabled}, 1, 0) * (${baseVision} + ${perToken} * ${total-context}) + if(!${options.vision.enabled}, 1, 0) * (${base} + ${perToken} * ${total-context})";
#[test]
fn recovers_known_constants_from_synthetic_records() {
    let catalog = catalog("${base} + ${perToken} * ${total-context}", None);
    let records = [8_192, 16_384, 32_768]
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            record(
                &format!("r{index}"),
                &format!("d{index}"),
                WorkloadResourceObservationKind::Startup,
                context,
                BASE + PER_TOKEN * context,
            )
        })
        .collect::<Vec<_>>();

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        report.joint.parameters,
        BTreeMap::from([
            ("base".to_owned(), BASE),
            ("perToken".to_owned(), PER_TOKEN)
        ])
    );
    assert_eq!(report.point_count, 3);
    assert_eq!(report.proposal.margin, Some(0));
    assert!(report.joint.points.iter().all(|point| point.residual == 0));
    assert_eq!(
        report.joint.residuals_by_accelerator["NVIDIA GeForce RTX 5090"].count,
        3
    );
}

#[test]
fn recovers_one_slope_per_kv_cache_precision_from_mixed_records() {
    let catalog = catalog_with_kv_cache_option();
    let records = [
        ("f16", 8_192),
        ("f16", 16_384),
        ("q8_0", 8_192),
        ("q8_0", 16_384),
        ("q8_0", 32_768),
    ]
    .into_iter()
    .map(|(precision, context)| {
        kv_cache_record(&format!("{precision}-{context}"), Some(precision), context)
    })
    .collect::<Vec<_>>();

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        report.joint.parameters,
        BTreeMap::from([
            ("base".to_owned(), BASE),
            ("perToken".to_owned(), PER_TOKEN),
            ("perTokenQ8".to_owned(), PER_TOKEN_Q8),
        ])
    );
    assert_eq!(report.point_count, 5);
    assert_eq!(report.proposal.margin, Some(0));
    assert!(report.joint.points.iter().all(|point| point.residual == 0));
    assert_eq!(report.defaulted_options, []);
}

#[test]
fn a_record_without_the_option_value_counts_at_the_configuration_default() {
    let catalog = catalog_with_kv_cache_option();
    let records = [
        ("legacy-8192", None, 8_192),
        ("legacy-16384", None, 16_384),
        ("q8_0-8192", Some("q8_0"), 8_192),
        ("q8_0-16384", Some("q8_0"), 16_384),
    ]
    .into_iter()
    .map(|(record_id, precision, context)| kv_cache_record(record_id, precision, context))
    .collect::<Vec<_>>();

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        report.joint.parameters,
        BTreeMap::from([
            ("base".to_owned(), BASE),
            ("perToken".to_owned(), PER_TOKEN),
            ("perTokenQ8".to_owned(), PER_TOKEN_Q8),
        ])
    );
    assert_eq!(report.point_count, 4);
    assert_eq!(report.skipped, []);
    assert_eq!(
        report.defaulted_options,
        ["legacy-8192", "legacy-16384"].map(|record_id| DefaultedOptions {
            record_id: record_id.to_owned(),
            option_ids: vec!["kv-cache".to_owned()],
        })
    );
}

#[test]
fn fits_each_kv_cache_precision_group_with_its_own_intercept() {
    let catalog = catalog_with_kv_cache_option();
    let mut records = mixed_precision_records();
    records.push(kv_record(
        "f16-8192-peak",
        "f16-8192",
        WorkloadResourceObservationKind::Peak,
        "f16",
        8_192,
        BASE + PER_TOKEN * 8_192 + 500,
    ));

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        report.groups,
        vec![
            GroupReport {
                choices: precision_choice("f16"),
                default: true,
                point_count: 3,
                context_range: Some(ContextRange {
                    minimum: 8_192,
                    maximum: 32_768,
                }),
                slopes: BTreeMap::from([("perToken".to_owned(), PER_TOKEN)]),
                intercept_parameters: BTreeMap::from([("base".to_owned(), 1)]),
                intercept: Some(BASE as i64),
                intercept_delta: None,
                residuals: Some(ResidualSummary {
                    count: 4,
                    minimum: 0,
                    maximum: 500,
                    mean: 125.0,
                }),
                local_slopes: vec![
                    LocalSlope {
                        from_context: 8_192,
                        to_context: 16_384,
                        per_token: PER_TOKEN as f64,
                    },
                    LocalSlope {
                        from_context: 16_384,
                        to_context: 32_768,
                        per_token: PER_TOKEN as f64,
                    },
                ],
                error: None,
            },
            GroupReport {
                choices: precision_choice("q8_0"),
                default: false,
                point_count: 3,
                context_range: Some(ContextRange {
                    minimum: 8_192,
                    maximum: 32_768,
                }),
                slopes: BTreeMap::from([("perTokenQ8".to_owned(), PER_TOKEN_Q8)]),
                intercept_parameters: BTreeMap::from([("base".to_owned(), 1)]),
                intercept: Some((BASE + Q8_OFFSET) as i64),
                intercept_delta: Some(Q8_OFFSET as i64),
                residuals: Some(ResidualSummary {
                    count: 3,
                    minimum: 0,
                    maximum: 0,
                    mean: 0.0,
                }),
                local_slopes: vec![
                    LocalSlope {
                        from_context: 8_192,
                        to_context: 16_384,
                        per_token: PER_TOKEN_Q8 as f64,
                    },
                    LocalSlope {
                        from_context: 16_384,
                        to_context: 32_768,
                        per_token: PER_TOKEN_Q8 as f64,
                    },
                ],
                error: None,
            },
        ]
    );
    assert_eq!(
        report.proposal.constants,
        BTreeMap::from([
            (
                "base".to_owned(),
                ProposedConstant {
                    value: BASE,
                    source: "intercept of the default group kv-cache.precision=f16".to_owned(),
                }
            ),
            (
                "perToken".to_owned(),
                ProposedConstant {
                    value: PER_TOKEN,
                    source: "slope of the default group kv-cache.precision=f16".to_owned(),
                }
            ),
            (
                "perTokenQ8".to_owned(),
                ProposedConstant {
                    value: PER_TOKEN_Q8,
                    source: "slope of the group kv-cache.precision=q8_0".to_owned(),
                }
            ),
        ])
    );
    assert_eq!(
        (
            report.proposal.margin,
            report.proposal.margin_parameter.as_deref()
        ),
        (Some(Q8_OFFSET), Some("marginBytes"))
    );
    assert_eq!(
        report.proposal.block,
        r#"# Fitted on hardware class "nvidia" (NVIDIA GeForce RTX 5090) with fixture-engine/linux-amd64, from 7 records.
# kv-cache.precision=f16: 3 fit points at total context 8192-32768.
# kv-cache.precision=q8_0: 3 fit points at total context 8192-32768, intercept +67000000 from the default.
# Residuals against these constants: 0 to 67000000.
- bindingId: "sized"
  parameters:
    base: 1000000
    marginBytes: 67000000
    perToken: 36864
    perTokenQ8: 19584
"#
    );
}

#[test]
fn a_group_missing_a_slope_solves_the_slopes_it_has() {
    let catalog = catalog_with_kv_cache_option();
    let records = mixed_precision_records()
        .into_iter()
        .filter(|record| record.record_id.starts_with("f16"))
        .collect::<Vec<_>>();

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(report.unfitted_parameters, ["perTokenQ8"]);
    assert_eq!(
        report.joint.parameters,
        BTreeMap::from([
            ("base".to_owned(), BASE),
            ("perToken".to_owned(), PER_TOKEN)
        ])
    );
    assert_eq!(
        (
            report.groups.len(),
            &report.groups[0].slopes,
            &report.groups[0].error
        ),
        (
            1,
            &BTreeMap::from([("perToken".to_owned(), PER_TOKEN)]),
            &None
        )
    );
    assert_eq!(report.proposal.undetermined, BTreeMap::new());
    assert_eq!(report.proposal.margin, Some(0));
    assert!(
        report.proposal.block.ends_with(
            r#"- bindingId: "sized"
  parameters:
    base: 1000000
    marginBytes: 0
    perToken: 36864
    perTokenQ8: 1  # stored, not fitted
"#
        ),
        "{}",
        report.proposal.block
    );
}

#[test]
fn an_additive_term_takes_what_its_group_intercept_leaves_after_the_base() {
    let catalog = catalog_with_vision_option(&additive_vision_sizing());
    let request = FitRequest {
        series: SeriesSelection::from_values(&["all".to_owned()]).expect("all series"),
        ..estimate_request()
    };

    let report = fit(&catalog, &request, &vision_series_records([false, true])).expect("fit");

    assert_eq!(
        report
            .proposal
            .constants
            .iter()
            .map(|(name, constant)| (name.as_str(), constant.value, constant.source.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "base",
                BASE,
                "intercept of the default group vision.enabled=false"
            ),
            (
                "perToken",
                PER_TOKEN,
                "slope of the default group vision.enabled=false"
            ),
            (
                "visionBytes",
                VISION_BASE - BASE,
                "intercept of the group vision.enabled=true"
            ),
        ]
    );
    assert_eq!(report.proposal.margin, Some(VISION_GROWTH));
}

#[test]
fn a_slope_measured_at_one_context_comes_from_its_group_intercept() {
    let catalog = catalog_with_kv_cache_option();
    let records = mixed_precision_records()
        .into_iter()
        .filter(|record| record.record_id.starts_with("f16") || record.record_id == "q8_0-16384")
        .collect::<Vec<_>>();

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        report.groups[1].intercept_parameters,
        BTreeMap::from([("base".to_owned(), 1), ("perTokenQ8".to_owned(), 16_384)])
    );
    assert_eq!(
        report.proposal.constants["perTokenQ8"].value,
        // The q8_0 records sit Q8_OFFSET above the base, which the single context folds into
        // the slope, rounded up.
        PER_TOKEN_Q8 + Q8_OFFSET.div_ceil(16_384)
    );
}

#[test]
fn near_collinear_columns_still_solve_when_the_diagnosis_accepts_them() {
    let rows = [1_000_000.0, 1_000_002.0, 1_000_004.0]
        .map(|fact| solve::Row {
            target: 5.0 + 3.0 * fact,
            coefficients: vec![1.0, fact],
        })
        .to_vec();
    let unknowns = ["a", "b"].map(str::to_owned);

    assert!(solve::diagnose(&unknowns, &rows).is_ok());
    let values = solve::least_squares(&rows).expect("solvable");
    assert!(
        (values[0] - 5.0).abs() < 1e-3 && (values[1] - 3.0).abs() < 1e-9,
        "{values:?}"
    );
}

#[test]
fn records_without_the_default_choices_propose_no_base() {
    let catalog = catalog_with_kv_cache_option();
    let records = mixed_precision_records()
        .into_iter()
        .filter(|record| record.record_id.starts_with("q8_0"))
        .collect::<Vec<_>>();

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        report
            .groups
            .iter()
            .map(|group| (group.default, group.intercept))
            .collect::<Vec<_>>(),
        [(false, Some((BASE + Q8_OFFSET) as i64))]
    );
    assert_eq!(
        report.proposal.constants.keys().collect::<Vec<_>>(),
        ["perTokenQ8"]
    );
    assert_eq!(
        report.proposal.undetermined,
        BTreeMap::from([(
            "base".to_owned(),
            "no record has the configuration's default option choices".to_owned()
        )])
    );
    assert_eq!(report.proposal.margin, None);
    assert!(
        report.proposal.block.contains(
            "# base is undetermined: no record has the configuration's default option choices.\n"
        ),
        "{}",
        report.proposal.block
    );
}

#[test]
fn unsolvable_fits_name_their_cause_and_a_solvable_parameter_set() {
    let kv_cache = catalog_with_kv_cache_option();
    let f16_only = mixed_precision_records()
        .into_iter()
        .filter(|record| record.record_id.starts_with("f16"))
        .collect::<Vec<_>>();
    let linear = catalog(LINEAR_EQUATION, None);
    let contexts = [8_192, 16_384, 32_768]
        .map(|context| {
            record(
                &format!("r{context}"),
                &format!("r{context}"),
                WorkloadResourceObservationKind::Startup,
                context,
                BASE + PER_TOKEN * context,
            )
        })
        .to_vec();
    let inline = |equation: &str, parameters: &[&str]| FitRequest {
        equation: FitEquation::Inline(equation.to_owned()),
        parameters: parameters.iter().map(|name| (*name).to_owned()).collect(),
        ..estimate_request()
    };

    for (catalog, request, records, expected) in [
        (
            &kv_cache,
            FitRequest {
                parameters: ["base", "perToken", "perTokenQ8"]
                    .map(str::to_owned)
                    .to_vec(),
                ..estimate_request()
            },
            &f16_only,
            "sizing fit is unsolvable: parameter \"perTokenQ8\" has no data: its coefficient is zero on every fit point; fitting only base, perToken would solve it",
        ),
        (
            &linear,
            estimate_request(),
            &contexts[..1].to_vec(),
            "sizing fit is unsolvable: 1 distinct fit points cannot determine 2 parameters; fitting only base would solve it",
        ),
        (
            &linear,
            inline(
                "${a} * ${total-context} + ${b} * 2 * ${total-context} + ${c}",
                &["a", "b", "c"],
            ),
            &contexts,
            "sizing fit is unsolvable: parameters \"a\" and \"b\" are indistinguishable: their coefficients are proportional on every fit point; fitting only a, c would solve it",
        ),
        (
            &linear,
            inline(
                "${a} * ${total-context} + ${b} + ${c} * (${total-context} + 1)",
                &["a", "b", "c"],
            ),
            &contexts,
            "sizing fit is unsolvable: the fit points determine only 2 of 3 parameters; fitting only a, b would solve it",
        ),
    ] {
        assert_eq!(
            fit(catalog, &request, records)
                .expect_err("unsolvable fit")
                .to_string(),
            expected
        );
    }
}

#[test]
fn check_reports_residuals_against_the_equation_and_headroom_against_the_estimate() {
    let catalog = kv_cache_catalog(&format!(
        "base: {BASE}, perToken: {PER_TOKEN}, perTokenQ8: {PER_TOKEN_Q8}, marginBytes: 1000"
    ));
    let records = [
        kv_record(
            "f16-8192",
            "f16-8192",
            WorkloadResourceObservationKind::Startup,
            "f16",
            8_192,
            BASE + PER_TOKEN * 8_192 + 300,
        ),
        kv_record(
            "q8_0-16384",
            "q8_0-16384",
            WorkloadResourceObservationKind::Startup,
            "q8_0",
            16_384,
            BASE + PER_TOKEN_Q8 * 16_384 + 1_500,
        ),
    ];

    let report = check(&catalog, &estimate_request(), &records).expect("check");

    assert_eq!(
        report
            .points
            .iter()
            .map(|point| (
                point.record_id.as_str(),
                point.equation,
                point.allowance,
                point.residual,
                point.headroom
            ))
            .collect::<Vec<_>>(),
        [
            ("f16-8192", BASE + PER_TOKEN * 8_192, 1_000, 300, 700),
            (
                "q8_0-16384",
                BASE + PER_TOKEN_Q8 * 16_384,
                1_000,
                1_500,
                -500
            ),
        ]
    );
    assert_eq!(
        report
            .groups
            .iter()
            .map(|group| (
                group.choices.clone(),
                group.residuals.maximum,
                group.minimum_headroom
            ))
            .collect::<Vec<_>>(),
        [
            (precision_choice("f16"), 300, 700),
            (precision_choice("q8_0"), 1_500, -500),
        ]
    );
    assert_eq!(
        report
            .under_predictions
            .iter()
            .map(|point| point.record_id.as_str())
            .collect::<Vec<_>>(),
        ["q8_0-16384"]
    );
    for request in [
        FitRequest {
            equation: FitEquation::Inline("${base}".to_owned()),
            ..estimate_request()
        },
        FitRequest {
            parameters: vec!["base".to_owned()],
            ..estimate_request()
        },
    ] {
        assert!(matches!(
            check(&catalog, &request, &records),
            Err(FitError::CheckRequiresStoredEstimate)
        ));
    }
}

#[test]
fn recorded_gb10_sweep_reproduces_its_constants_and_checks_clean() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/sizing-fit");
    let catalog = Catalog::load_with_policy(
        &fixture.join("catalog"),
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("sizing fit fixture catalog");
    let records = crate::telemetry::WorkloadResourceRecords::read(&[fixture.join("telemetry")])
        .expect("sizing fit fixture records");
    assert_eq!((records.records.len(), records.malformed.len()), (42, 0));

    // The q4-k-m sweep repeats one context of each precision at 2 parallel requests, which weights
    // that context twice in the q8_0 slope.
    for (binding_id, base, slope_q8, residuals_q8, minimum_headroom) in [
        (
            "q3-iq3-s",
            16_833_888_256,
            118_008,
            (-35_274_752, 36_618_240),
            (2_247_483_648, 2_278_285_568),
        ),
        (
            "q4-k-m",
            20_922_744_832,
            118_046,
            (-26_663_322, 47_408_742),
            (2_247_483_648, 2_278_596_864),
        ),
    ] {
        let request = FitRequest {
            model_id: "nanbeige-4-3-33b".to_owned(),
            configuration_id: "llama-cpp".to_owned(),
            hardware_class_id: "nvidia-unified".to_owned(),
            binding_id: binding_id.to_owned(),
            ..estimate_request()
        };

        let report = fit(&catalog, &request, &records.records).expect("fit");
        let check = check(&catalog, &request, &records.records).expect("check");

        assert_eq!(
            (
                report
                    .proposal
                    .constants
                    .iter()
                    .map(|(name, constant)| (name.as_str(), constant.value))
                    .collect::<Vec<_>>(),
                report
                    .groups
                    .iter()
                    .map(|group| group
                        .residuals
                        .as_ref()
                        .map(|residuals| (residuals.minimum, residuals.maximum)))
                    .collect::<Vec<_>>(),
            ),
            (
                vec![
                    ("accelBytesPerContextToken", 214_016),
                    ("accelBytesPerContextTokenQ8", slope_q8),
                    ("baseAccelBytes", base),
                ],
                vec![Some((0, 0)), Some(residuals_q8)],
            ),
            "{binding_id}"
        );
        assert_eq!(
            (
                check.under_predictions.len(),
                check
                    .groups
                    .iter()
                    .map(|group| group.minimum_headroom)
                    .collect::<Vec<_>>(),
            ),
            (0, vec![minimum_headroom.0, minimum_headroom.1]),
            "{binding_id}"
        );
    }
}

#[test]
fn records_with_custom_arguments_are_skipped_and_listed_unless_included() {
    let catalog = catalog(LINEAR_EQUATION, None);
    let mut records = [8_192, 16_384, 32_768]
        .into_iter()
        .map(|context| {
            record(
                &format!("r{context}"),
                &format!("d{context}"),
                WorkloadResourceObservationKind::Startup,
                context,
                BASE + PER_TOKEN * context,
            )
        })
        .collect::<Vec<_>>();
    records[2]
        .inputs
        .as_mut()
        .expect("inputs")
        .custom_argument_tokens = ["--cache-type-k", "q8_0"].map(str::to_owned).to_vec();

    let skipped = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(skipped.point_count, 2);
    assert_eq!(
        skipped.custom_argument_records,
        [CustomArgumentRecord {
            record_id: "r32768".to_owned(),
            custom_argument_tokens: vec!["--cache-type-k".to_owned(), "q8_0".to_owned()],
        }]
    );
    assert_eq!(skipped.series.read[0].record_count, 2);

    let included = fit(
        &catalog,
        &FitRequest {
            include_custom_argument_records: true,
            ..estimate_request()
        },
        &records,
    )
    .expect("fit");

    assert_eq!(included.point_count, 3);
    assert_eq!(included.custom_argument_records, []);
}

#[test]
fn peaks_widen_the_margin_and_report_warm_growth_without_moving_the_fit() {
    let catalog = catalog("${base} + ${perToken} * ${total-context}", None);
    let mut records = [8_192, 16_384]
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            record(
                &format!("r{index}"),
                &format!("d{index}"),
                WorkloadResourceObservationKind::Startup,
                context,
                BASE + PER_TOKEN * context,
            )
        })
        .collect::<Vec<_>>();
    records.push(record(
        "peak",
        "d0",
        WorkloadResourceObservationKind::Peak,
        8_192,
        BASE + PER_TOKEN * 8_192 + 500,
    ));
    records.push(record(
        "restarted",
        "d9",
        WorkloadResourceObservationKind::Peak,
        8_192,
        BASE + PER_TOKEN * 8_192 + 900,
    ));

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(report.joint.parameters["perToken"], PER_TOKEN);
    assert_eq!(report.point_count, 2);
    assert_eq!(report.proposal.margin, Some(900));
    assert_eq!(
        report
            .joint
            .largest_under_prediction
            .expect("under-prediction")
            .record_id,
        "restarted"
    );
    assert_eq!(report.warm_growth.maximum_growth_bytes, 500);
    assert_eq!(report.warm_growth.realizations.len(), 2);
}

#[test]
fn ignores_other_bindings_engine_artifacts_and_unavailable_points() {
    let catalog = catalog(
        "${base} + ${perToken} * ${total-context}",
        Some("${total-context} > 1000"),
    );
    let mut records = [8_192, 16_384]
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            record(
                &format!("r{index}"),
                &format!("d{index}"),
                WorkloadResourceObservationKind::Startup,
                context,
                BASE + PER_TOKEN * context,
            )
        })
        .collect::<Vec<_>>();
    let mut other_binding = record(
        "other-binding",
        "x1",
        WorkloadResourceObservationKind::Startup,
        4_096,
        1,
    );
    other_binding
        .inputs
        .as_mut()
        .expect("inputs")
        .model_artifact_id = "second-weights".to_owned();
    let mut other_engine = record(
        "other-engine",
        "x2",
        WorkloadResourceObservationKind::Startup,
        4_096,
        1,
    );
    other_engine
        .inputs
        .as_mut()
        .expect("inputs")
        .engine_artifact_id = "linux-arm64".to_owned();
    let unavailable = record(
        "unavailable",
        "x3",
        WorkloadResourceObservationKind::Startup,
        512,
        1,
    );
    records.extend([other_binding, other_engine, unavailable]);

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(report.joint.parameters["perToken"], PER_TOKEN);
    assert_eq!(report.point_count, 2);
    assert_eq!(
        report
            .skipped
            .iter()
            .map(|skipped| skipped.record_id.as_str())
            .collect::<Vec<_>>(),
        vec!["unavailable"]
    );
}

#[test]
fn fits_each_hardware_class_of_one_configuration_separately() {
    let catalog = two_class_catalog();
    let mut records = Vec::new();
    for (class, base, per_token) in [
        ("nvidia", BASE, PER_TOKEN),
        (NVIDIA_AMD64, AMD64_BASE, AMD64_PER_TOKEN),
    ] {
        for context in [8_192, 16_384] {
            let mut record = record(
                &format!("{class}-{context}"),
                &format!("{class}-{context}"),
                WorkloadResourceObservationKind::Startup,
                context,
                base + per_token * context,
            );
            record.inputs.as_mut().expect("inputs").hardware_class_id = class.to_owned();
            records.push(record);
        }
    }

    let nvidia = fit(&catalog, &estimate_request(), &records).expect("nvidia fit");
    let amd64 = fit(
        &catalog,
        &FitRequest {
            hardware_class_id: NVIDIA_AMD64.to_owned(),
            ..estimate_request()
        },
        &records,
    )
    .expect("amd64 fit");

    assert_eq!(
        (
            nvidia.joint.parameters,
            fitted_record_ids(&nvidia.joint.points)
        ),
        (
            BTreeMap::from([
                ("base".to_owned(), BASE),
                ("perToken".to_owned(), PER_TOKEN)
            ]),
            vec!["nvidia-16384", "nvidia-8192"],
        )
    );
    assert_eq!(
        (
            amd64.joint.parameters,
            fitted_record_ids(&amd64.joint.points)
        ),
        (
            BTreeMap::from([
                ("base".to_owned(), AMD64_BASE),
                ("perToken".to_owned(), AMD64_PER_TOKEN)
            ]),
            vec!["nvidia-amd64-16384", "nvidia-amd64-8192"],
        )
    );
}

#[test]
fn excludes_records_without_a_hardware_class() {
    let catalog = catalog(LINEAR_EQUATION, None);
    let mut records = [8_192, 16_384]
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            record(
                &format!("r{index}"),
                &format!("d{index}"),
                WorkloadResourceObservationKind::Startup,
                context,
                BASE + PER_TOKEN * context,
            )
        })
        .collect::<Vec<_>>();
    let mut unclassified = record(
        "unclassified",
        "u0",
        WorkloadResourceObservationKind::Startup,
        32_768,
        1,
    );
    unclassified
        .inputs
        .as_mut()
        .expect("inputs")
        .hardware_class_id = String::new();
    records.push(unclassified);

    let report = fit(&catalog, &estimate_request(), &records).expect("fit");

    assert_eq!(
        (
            report.joint.parameters,
            fitted_record_ids(&report.joint.points),
            report.skipped,
            report.proposal.margin,
        ),
        (
            BTreeMap::from([
                ("base".to_owned(), BASE),
                ("perToken".to_owned(), PER_TOKEN)
            ]),
            vec!["r0", "r1"],
            Vec::new(),
            Some(0),
        )
    );
    assert!(matches!(
        fit(&catalog, &estimate_request(), &records[2..],),
        Err(FitError::NoMatchingRecords { .. })
    ));
}

#[test]
fn rejects_a_hardware_class_the_configuration_does_not_support() {
    let catalog = catalog(LINEAR_EQUATION, None);
    let request = FitRequest {
        hardware_class_id: "apple".to_owned(),
        ..estimate_request()
    };

    let error = fit(&catalog, &request, &[]).expect_err("unsupported class");

    assert!(
        format!("{:#}", anyhow::Error::new(error))
            .contains("model configuration does not support the hardware class")
    );
}

#[test]
fn inline_equation_fits_engine_parameters_against_vllm_facts() {
    let catalog = catalog("${base}", None);
    let records = [
        (50_176, 2_000_000_000),
        (111_328, 4_000_000_000),
        (233_632, 8_000_000_000),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (tokens, kv_bytes))| {
        let mut record = record(
            &format!("r{index}"),
            &format!("d{index}"),
            WorkloadResourceObservationKind::Measurement,
            kv_bytes,
            1,
        );
        record
            .observation
            .as_mut()
            .expect("observation")
            .engine_facts = Some(EngineFacts::Vllm(VllmStartupFacts {
            kv_cache_token_capacity: Some(tokens),
            ..VllmStartupFacts::default()
        }));
        record
    })
    .collect::<Vec<_>>();
    let request = FitRequest {
        equation: FitEquation::Inline(
            "${kvBase} + ${kvPerToken} * ${vllm.kv-token-capacity}".to_owned(),
        ),
        parameters: vec!["kvBase".to_owned(), "kvPerToken".to_owned()],
        observed: "engine.total-context".parse().expect("quantity"),
        ..estimate_request()
    };

    let report = fit(&catalog, &request, &records).expect("fit");

    let per_token = report.joint.parameters["kvPerToken"];
    assert!((32_700..=32_710).contains(&per_token), "{per_token}");
    for point in &report.joint.points {
        let error = point.residual.unsigned_abs() as f64 / point.observed as f64;
        assert!(error < 0.02, "{point:?}");
    }
}

#[test]
fn fact_equations_still_report_growth_from_peaks_without_facts() {
    let catalog = catalog("${base}", None);
    let mut records = [(50_176, 2_000_000_000_u64), (111_328, 4_000_000_000)]
        .into_iter()
        .enumerate()
        .map(|(index, (tokens, bytes))| {
            let mut record = record(
                &format!("r{index}"),
                &format!("d{index}"),
                WorkloadResourceObservationKind::Startup,
                8_192,
                bytes,
            );
            record
                .observation
                .as_mut()
                .expect("observation")
                .engine_facts = Some(EngineFacts::Vllm(VllmStartupFacts {
                kv_cache_token_capacity: Some(tokens),
                ..VllmStartupFacts::default()
            }));
            record
        })
        .collect::<Vec<_>>();
    records.push(record(
        "peak",
        "d0",
        WorkloadResourceObservationKind::Peak,
        8_192,
        2_000_000_700,
    ));
    let request = FitRequest {
        equation: FitEquation::Inline(
            "${kvBase} + ${kvPerToken} * ${vllm.kv-token-capacity}".to_owned(),
        ),
        parameters: vec!["kvBase".to_owned(), "kvPerToken".to_owned()],
        ..estimate_request()
    };

    let report = fit(&catalog, &request, &records).expect("fit");

    assert_eq!(report.warm_growth.maximum_growth_bytes, 700);
    assert_eq!(
        report
            .skipped
            .iter()
            .map(|skipped| skipped.record_id.as_str())
            .collect::<Vec<_>>(),
        vec!["peak"]
    );
}

#[test]
fn rejects_parameters_absent_from_the_equation() {
    let catalog = catalog("${base}", None);
    let request = FitRequest {
        equation: FitEquation::Inline("${kvBase} + ${total-context}".to_owned()),
        parameters: vec!["kvBase".to_owned(), "unused".to_owned()],
        ..estimate_request()
    };

    let error = fit(&catalog, &request, &[]).expect_err("unused parameter");

    assert!(
        format!("{:#}", anyhow::Error::new(error))
            .contains("fit parameter \"unused\" does not appear in the equation")
    );
    assert_eq!(
        fit(
            &catalog,
            &estimate_request(),
            &[
                record("r0", "d0", WorkloadResourceObservationKind::Startup, 1, 5),
                record("r1", "d1", WorkloadResourceObservationKind::Startup, 2, 5),
            ]
        )
        .expect("estimate fits only the parameters its equation uses")
        .joint
        .parameters,
        BTreeMap::from([("base".to_owned(), 5)])
    );
}

#[test]
fn rejects_nonlinear_underdetermined_and_negative_fits() {
    let conditional = catalog("if(${total-context} > 1, ${base}, 0) + ${perToken}", None);
    assert!(
        format!(
            "{:#}",
            anyhow::Error::new(
                fit(&conditional, &estimate_request(), &[]).expect_err("conditional")
            )
        )
        .contains("under a condition or comparison")
    );

    let available_when = catalog("${base} + ${perToken}", Some("${base} > 1"));
    assert!(
        format!(
            "{:#}",
            anyhow::Error::new(
                fit(&available_when, &estimate_request(), &[]).expect_err("availableWhen")
            )
        )
        .contains("availableWhen")
    );

    let linear = catalog("${base} + ${perToken} * ${total-context}", None);
    let one_point = [record(
        "r0",
        "d0",
        WorkloadResourceObservationKind::Startup,
        8_192,
        5,
    )];
    assert!(matches!(
        fit(&linear, &estimate_request(), &one_point),
        Err(FitError::Unsolvable {
            cause: UnsolvableCause::TooFewPoints {
                points: 1,
                parameters: 2
            },
            solvable,
        }) if solvable == ["base"]
    ));

    let decreasing = [
        record(
            "r0",
            "d0",
            WorkloadResourceObservationKind::Startup,
            8_192,
            10_000_000,
        ),
        record(
            "r1",
            "d1",
            WorkloadResourceObservationKind::Startup,
            16_384,
            5_000_000,
        ),
    ];
    assert!(matches!(
        fit(&linear, &estimate_request(), &decreasing),
        Err(FitError::NegativeParameter { parameter, .. }) if parameter == "perToken"
    ));
}

#[test]
fn fits_one_constant_from_two_series_of_one_binding() {
    let catalog = catalog_with_vision_option(&vision_sizing());
    let records = vision_series_records([false, true]);

    let declared = fit(&catalog, &estimate_request(), &records).expect("one series only");
    assert_eq!(
        (declared.joint.parameters, declared.unfitted_parameters),
        (
            BTreeMap::from([
                ("base".to_owned(), BASE),
                ("perToken".to_owned(), PER_TOKEN),
            ]),
            vec!["baseVision".to_owned()],
        )
    );

    let request = FitRequest {
        series: SeriesSelection::from_values(&["all".to_owned()]).expect("all series"),
        ..estimate_request()
    };
    let report = fit(&catalog, &request, &records).expect("joint fit");

    assert_eq!(
        report.joint.parameters,
        BTreeMap::from([
            ("base".to_owned(), BASE),
            ("baseVision".to_owned(), VISION_BASE),
            ("perToken".to_owned(), PER_TOKEN),
        ])
    );
    assert_eq!(report.point_count, 4);
    assert_eq!(report.proposal.margin, Some(VISION_GROWTH));
    assert_eq!(report.series.mode, "all");
    assert_eq!(
        report.series.read,
        vec![
            SeriesRecords {
                auxiliary_artifact_ids: Vec::new(),
                record_count: 4,
            },
            SeriesRecords {
                auxiliary_artifact_ids: vec![VISION_PROJECTOR.to_owned()],
                record_count: 4,
            },
        ]
    );
    assert!(report.series.ignored.is_empty());
    assert_eq!(report.series.unmatched_record_count, 0);
}

#[test]
fn reports_the_realizable_series_a_selection_left_out() {
    let catalog = catalog_with_vision_option(&linear_sizing());
    let records = vision_series_records([false, true]);
    let request = FitRequest {
        series: SeriesSelection::from_values(&["none".to_owned()]).expect("one series"),
        ..estimate_request()
    };

    let report = fit(&catalog, &request, &records).expect("fit");

    assert_eq!(report.joint.parameters["perToken"], PER_TOKEN);
    assert_eq!(report.point_count, 2);
    assert_eq!(report.series.mode, "named");
    assert_eq!(
        report.series.ignored,
        vec![SeriesRecords {
            auxiliary_artifact_ids: vec![VISION_PROJECTOR.to_owned()],
            record_count: 4,
        }]
    );
    assert_eq!(report.series.unmatched_record_count, 0);
}

#[test]
fn refuses_a_selection_that_matches_no_record() {
    let catalog = catalog_with_vision_option(&linear_sizing());
    let records = vision_series_records([false]);
    let request = FitRequest {
        series: SeriesSelection::from_values(&[VISION_PROJECTOR.to_owned()]).expect("series"),
        ..estimate_request()
    };

    let error = fit(&catalog, &request, &records).expect_err("no matching record");

    let message = format!("{error}");
    assert!(message.contains("no telemetry record matches"), "{message}");
    assert!(message.contains("[vision-projector]"), "{message}");
}

#[test]
fn reports_a_selected_series_that_has_no_record() {
    let catalog = catalog_with_vision_option(&linear_sizing());
    let records = vision_series_records([false]);
    let request = FitRequest {
        series: SeriesSelection::from_values(&["none".to_owned(), VISION_PROJECTOR.to_owned()])
            .expect("two series"),
        ..estimate_request()
    };

    let report = fit(&catalog, &request, &records).expect("fit");

    assert_eq!(
        report.series.read,
        vec![
            SeriesRecords {
                auxiliary_artifact_ids: Vec::new(),
                record_count: 4,
            },
            SeriesRecords {
                auxiliary_artifact_ids: vec![VISION_PROJECTOR.to_owned()],
                record_count: 0,
            },
        ]
    );
}

#[test]
fn refuses_a_series_the_configuration_cannot_realize() {
    let catalog = catalog_with_vision_option(&linear_sizing());
    let request = FitRequest {
        series: SeriesSelection::from_values(&["unknown-projector".to_owned()]).expect("series"),
        ..estimate_request()
    };

    let error = fit(&catalog, &request, &[]).expect_err("unrealizable series");

    let message = format!("{error}");
    assert!(message.contains("unknown-projector"), "{message}");
    assert!(message.contains("cannot realize"), "{message}");
}

#[test]
fn parses_series_arguments() {
    assert!(matches!(
        SeriesSelection::from_values(&[]),
        Ok(SeriesSelection::Declared)
    ));
    assert!(matches!(
        SeriesSelection::from_values(&[SERIES_NONE.to_owned()]),
        Ok(SeriesSelection::Named(named)) if named == BTreeSet::from([BTreeSet::new()])
    ));
    assert!(matches!(
        SeriesSelection::from_values(&[SERIES_ALL.to_owned()]),
        Ok(SeriesSelection::AllRealizable)
    ));
    assert!(matches!(
        SeriesSelection::from_values(&[format!("{VISION_PROJECTOR},mtp-draft")]),
        Ok(SeriesSelection::Named(named)) if named == BTreeSet::from([BTreeSet::from([
            VISION_PROJECTOR.to_owned(),
            "mtp-draft".to_owned(),
        ])])
    ));
    assert!(matches!(
        SeriesSelection::from_values(&[SERIES_ALL.to_owned(), SERIES_NONE.to_owned()]),
        Err(FitError::AllWithNamedSeries)
    ));
    assert!(matches!(
        SeriesSelection::from_values(&[String::new()]),
        Err(FitError::InvalidSeries(value)) if value.is_empty()
    ));
    assert!(matches!(
        SeriesSelection::from_values(&[format!("{VISION_PROJECTOR},")]),
        Err(FitError::InvalidSeries(value)) if value == format!("{VISION_PROJECTOR},")
    ));
}

fn catalog(equation: &str, available_when: Option<&str>) -> Catalog {
    let root = temporary_catalog_root();
    create_empty_catalog_source(&root);
    write_sizing_engine(&root);
    let available_when = available_when
        .map(|predicate| format!("\n          availableWhen: \"{predicate}\""))
        .unwrap_or_default();
    write_sizing_model_with(
        &root,
        &format!(
            r#"    sizing:
      bindingParameters: ["base", "perToken"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "{equation}"{available_when}
"#
        ),
        r#"        bindingSizing:
          - bindingId: "sized"
            parameters: { base: 1, perToken: 1 }
          - bindingId: "second"
            parameters: { base: 1, perToken: 1 }
"#,
        &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
    );
    let catalog = Catalog::load_with_policy(
        &root,
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("fit fixture catalog");
    std::fs::remove_dir_all(root).expect("remove fixture catalog");
    catalog
}

/// Linear fixture whose configuration supports `nvidia` and the nested `nvidia-amd64` class.
fn two_class_catalog() -> Catalog {
    let root = temporary_catalog_root();
    create_empty_catalog_source(&root);
    write_sizing_engine(&root);
    std::fs::write(
            root.join(format!("hardware-classes/{NVIDIA_AMD64}.yaml")),
            format!(
                "hardwareClassId: \"{NVIDIA_AMD64}\"\ndisplayName: \"NVIDIA amd64\"\nselector:\n  platform: {{ operatingSystem: \"linux\", architecture: \"amd64\" }}\n  accelerators: [\"nvidia\"]\n"
            ),
        )
        .expect("nested hardware class");
    write_sizing_model_with(
        &root,
        &format!(
            r#"    sizing:
      bindingParameters: ["base", "perToken"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "{LINEAR_EQUATION}"
"#
        ),
        &format!(
            r#"        bindingSizing:
          - bindingId: "sized"
            parameters: {{ base: 1, perToken: 1 }}
          - bindingId: "second"
            parameters: {{ base: 1, perToken: 1 }}
      - hardwareClassId: "{NVIDIA_AMD64}"
        bindingSizing:
          - bindingId: "sized"
            parameters: {{ base: 1, perToken: 1 }}
          - bindingId: "second"
            parameters: {{ base: 1, perToken: 1 }}
"#
        ),
        &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
    );
    let catalog = Catalog::load_with_policy(
        &root,
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("two-class fixture catalog");
    std::fs::remove_dir_all(root).expect("remove fixture catalog");
    catalog
}

fn fitted_record_ids(points: &[FitPointReport]) -> Vec<&str> {
    let mut ids = points
        .iter()
        .filter(|point| point.fitted)
        .map(|point| point.record_id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

fn estimate_request() -> FitRequest {
    FitRequest {
        model_id: "fixture-model".to_owned(),
        configuration_id: "local".to_owned(),
        hardware_class_id: "nvidia".to_owned(),
        binding_id: "sized".to_owned(),
        equation: FitEquation::Estimate("acceleratorMemory".to_owned()),
        parameters: Vec::new(),
        observed: ObservedQuantity::AttributedBytes,
        series: SeriesSelection::Declared,
        include_custom_argument_records: false,
    }
}

fn record(
    record_id: &str,
    owner_id: &str,
    kind: WorkloadResourceObservationKind,
    context_size: u64,
    attributed_bytes: u64,
) -> WorkloadResourceRecord {
    WorkloadResourceRecord {
        record_id: record_id.to_owned(),
        recorded_at: Some(Timestamp {
            seconds: 1_790_298_000,
            nanos: 0,
        }),
        realization: Some(WorkloadRealizationKey {
            owner_id: owner_id.to_owned(),
            generation: 1,
        }),
        inputs: Some(WorkloadResourceInputs {
            model_id: "fixture-model".to_owned(),
            configuration_id: "local".to_owned(),
            hardware_class_id: "nvidia".to_owned(),
            profile_id: "balanced".to_owned(),
            model_artifact_id: "sized-weights".to_owned(),
            engine_release_id: "fixture-engine".to_owned(),
            engine_artifact_id: "linux-amd64".to_owned(),
            engine_parameter_values: Some(EngineParameterValues {
                values: vec![EngineParameterValue {
                    parameter_id: "total-context".to_owned(),
                    value: Some(engine_parameter_value::Value::UnsignedInteger(context_size)),
                }],
            }),
            ..WorkloadResourceInputs::default()
        }),
        node: Some(WorkloadResourceNode {
            node_id: "gpu1".to_owned(),
            devices: vec![WorkloadResourceDevice {
                model: "NVIDIA GeForce RTX 5090".to_owned(),
                ..WorkloadResourceDevice::default()
            }],
            ..WorkloadResourceNode::default()
        }),
        observation: Some(WorkloadResourceObservation {
            kind: kind.into(),
            attributed_memory_bytes: attributed_bytes,
            ..WorkloadResourceObservation::default()
        }),
        ..WorkloadResourceRecord::default()
    }
}
fn catalog_with_vision_option(sizing: &str) -> Catalog {
    let root = temporary_catalog_root();
    create_empty_catalog_source(&root);
    write_sizing_engine(&root);
    std::fs::write(
            root.join("models/fixture.yaml"),
            format!(
                r#"modelId: "fixture-model"
displayName: "Fixture model"
version: "fixture"
maximumContextTokens: 262144
options:
  - optionId: "vision"
    displayName: "Vision"
    vision: {{}}
artifacts:
  - artifactId: "sized-weights"
    weightQuantization: "SIZED"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "sized.gguf"
        provenanceDigest: "2222222222222222222222222222222222222222222222222222222222222222"
auxiliaryArtifacts:
  - auxiliaryArtifactId: "{VISION_PROJECTOR}"
    source:
      repository: "fixture/model"
      revision: "1111111111111111111111111111111111111111"
      file:
        path: "mmproj.gguf"
        provenanceDigest: "3333333333333333333333333333333333333333333333333333333333333333"
configurations:
  - configurationId: "local"
    engineArtifacts:
      - engineReleaseId: "fixture-engine"
        engineArtifactId: "linux-amd64"
    artifactBindings:
      - bindingId: "sized"
        modelArtifactId: "sized-weights"
{sizing}    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp:
        fixedSettings:
          gpuLayers: 1
          flashAttention: true
        optionRealizations:
          - optionId: "vision"
            vision:
              enabledParameterId: "enabled"
              auxiliaryArtifactId: "{VISION_PROJECTOR}"
              imageInputCapacity:
                maximumImageCount: 1
                supportedMediaTypes: ["LLAMA_CPP_IMAGE_MEDIA_TYPE_JPEG", "LLAMA_CPP_IMAGE_MEDIA_TYPE_PNG"]
                maximumEncodedBytes: "20971520"
                maximumDecodedBytes: "67108864"
                maximumWidth: 8192
                maximumHeight: 8192
                maximumPixels: "16000000"
                fixedVisualTokens: 256
                fixedDetail: "LLAMA_CPP_IMAGE_DETAIL_AUTO"
    executionCapabilities:
      supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"]
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
    options:
      - optionId: "vision"
        parameters:
          - parameterId: "enabled"
            displayName: "Enabled"
            boolean: {{}}
        defaultValues:
          optionId: "vision"
          values:
            - parameterId: "enabled"
              boolean: false
profiles:
  - profileId: "balanced"
    configurationId: "local"
    defaultModelArtifactBindingId: "sized"
    engineParameterDefaults:
      values:
{SIZING_ENGINE_CACHE_DEFAULT}    optionDefaults:
      - optionId: "vision"
        values:
          - parameterId: "enabled"
            boolean: false
"#
            ),
        )
        .expect("vision fixture model");
    let catalog = Catalog::load_with_policy(
        &root,
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("vision fixture catalog");
    std::fs::remove_dir_all(root).expect("remove fixture catalog");
    catalog
}

/// Fixture whose configuration declares a KV cache precision option, f16 by default, sized by
/// one slope per precision behind indicator terms, with a margin allowance.
fn catalog_with_kv_cache_option() -> Catalog {
    kv_cache_catalog("base: 1, perToken: 1, perTokenQ8: 1, marginBytes: 1")
}

/// The KV cache fixture with the given stored constants for both bindings.
fn kv_cache_catalog(binding_parameters: &str) -> Catalog {
    let root = temporary_catalog_root();
    create_empty_catalog_source(&root);
    write_sizing_engine(&root);
    write_sizing_model_with(
        &root,
        &format!(
            r#"    sizing:
      bindingParameters: ["base", "perToken", "perTokenQ8", "marginBytes"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "{KV_CACHE_EQUATION}"
          allowance: "${{marginBytes}}"
"#
        ),
        &format!(
            r#"        bindingSizing:
          - bindingId: "sized"
            parameters: {{ {binding_parameters} }}
          - bindingId: "second"
            parameters: {{ {binding_parameters} }}
"#
        ),
        &format!("      values:\n{SIZING_ENGINE_CACHE_DEFAULT}"),
    );
    let path = root.join("models/fixture.yaml");
    let model = std::fs::read_to_string(&path)
            .expect("sizing model fixture")
            .replace(
                "maximumContextTokens: 262144\n",
                "maximumContextTokens: 262144\noptions:\n  - { optionId: \"kv-cache\", displayName: \"KV cache precision\", kvCachePrecision: {} }\n",
            )
            .replace(
                "          flashAttention: true\n",
                r#"          flashAttention: true
        optionRealizations:
          - { optionId: "kv-cache", kvCachePrecision: {} }
"#,
            )
            .replace(
                "      supportedModalities: [\"MODEL_INPUT_MODALITY_TEXT\"]\n",
                r#"      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
    options:
      - optionId: "kv-cache"
        parameters:
          - { parameterId: "precision", displayName: "Precision", stringEnum: { values: ["f16", "q8_0"] } }
        defaultValues:
          optionId: "kv-cache"
          values:
            - { parameterId: "precision", string: "f16" }
"#,
            );
    std::fs::write(&path, model).expect("KV cache fixture model");
    let catalog = Catalog::load_with_policy(
        &root,
        dotcore_catalog::OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("KV cache fixture catalog");
    std::fs::remove_dir_all(root).expect("remove fixture catalog");
    catalog
}

/// Startup records at three contexts per precision, the q8_0 ones `Q8_OFFSET` above the shared
/// base.
fn mixed_precision_records() -> Vec<WorkloadResourceRecord> {
    let mut records = Vec::new();
    for (precision, per_token, offset) in [("f16", PER_TOKEN, 0), ("q8_0", PER_TOKEN_Q8, Q8_OFFSET)]
    {
        for context in [8_192, 16_384, 32_768] {
            let record_id = format!("{precision}-{context}");
            records.push(kv_record(
                &record_id,
                &record_id,
                WorkloadResourceObservationKind::Startup,
                precision,
                context,
                BASE + offset + per_token * context,
            ));
        }
    }
    records
}

/// A record of the given realization at one KV cache precision.
fn kv_record(
    record_id: &str,
    owner_id: &str,
    kind: WorkloadResourceObservationKind,
    precision: &str,
    context_size: u64,
    attributed_bytes: u64,
) -> WorkloadResourceRecord {
    let mut record = record(record_id, owner_id, kind, context_size, attributed_bytes);
    record.inputs.as_mut().expect("inputs").option_values = vec![ModelOptionValues {
        option_id: "kv-cache".to_owned(),
        values: vec![ModelOptionParameterValue {
            parameter_id: "precision".to_owned(),
            value: Some(model_option_parameter_value::Value::String(
                precision.to_owned(),
            )),
        }],
    }];
    record
}

fn precision_choice(precision: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("kv-cache.precision".to_owned(), precision.to_owned())])
}

/// Startup record of its own realization at the given precision, or without the option value,
/// observing the KV cache fixture's exact estimate; a record without a value ran at f16.
fn kv_cache_record(
    record_id: &str,
    precision: Option<&str>,
    context_size: u64,
) -> WorkloadResourceRecord {
    let per_token = if precision == Some("q8_0") {
        PER_TOKEN_Q8
    } else {
        PER_TOKEN
    };
    let mut record = record(
        record_id,
        record_id,
        WorkloadResourceObservationKind::Startup,
        context_size,
        BASE + per_token * context_size,
    );
    if let Some(precision) = precision {
        record.inputs.as_mut().expect("inputs").option_values = vec![ModelOptionValues {
            option_id: "kv-cache".to_owned(),
            values: vec![ModelOptionParameterValue {
                parameter_id: "precision".to_owned(),
                value: Some(model_option_parameter_value::Value::String(
                    precision.to_owned(),
                )),
            }],
        }];
    }
    record
}

/// Configuration sizing and `nvidia` hardware support of the vision fixture, whose binding
/// constants are all fitted by default.
fn vision_sizing() -> String {
    format!(
        r#"    sizing:
      bindingParameters: ["base", "baseVision", "perToken"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "{VISION_EQUATION}"
    hardwareSupport:
      - hardwareClassId: "nvidia"
        bindingSizing:
          - bindingId: "sized"
            parameters: {{ base: 1, baseVision: 1, perToken: 1 }}
"#
    )
}

/// Configuration sizing and `nvidia` hardware support of the vision fixture with the vision tower
/// as an additive term over the base, as catalog models write it.
fn additive_vision_sizing() -> String {
    r#"    sizing:
      bindingParameters: ["base", "perToken", "visionBytes", "marginBytes"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${base} + ${perToken} * ${total-context} + if(${options.vision.enabled}, 1, 0) * ${visionBytes}"
          allowance: "${marginBytes}"
    hardwareSupport:
      - hardwareClassId: "nvidia"
        bindingSizing:
          - bindingId: "sized"
            parameters: { base: 1, perToken: 1, visionBytes: 1, marginBytes: 1 }
"#
    .to_owned()
}

/// Configuration sizing and `nvidia` hardware support of the linear fixture.
fn linear_sizing() -> String {
    format!(
        r#"    sizing:
      bindingParameters: ["base", "perToken"]
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "{LINEAR_EQUATION}"
    hardwareSupport:
      - hardwareClassId: "nvidia"
        bindingSizing:
          - bindingId: "sized"
            parameters: {{ base: 1, perToken: 1 }}
"#
    )
}

fn vision_series_records(settings: impl IntoIterator<Item = bool>) -> Vec<WorkloadResourceRecord> {
    let settings = settings.into_iter().collect::<Vec<_>>();
    let mut records = Vec::new();
    for context_size in [8_192_u64, 16_384] {
        for vision in &settings {
            let observed = if *vision { VISION_BASE } else { BASE } + PER_TOKEN * context_size;
            let generation = context_size * 2 + u64::from(*vision);
            records.push(vision_record(
                &format!("fit-{context_size}-{vision}"),
                context_size,
                *vision,
                observed,
                1_790_298_000,
                generation,
            ));
            records.push(vision_record(
                &format!("peak-{context_size}-{vision}"),
                context_size,
                *vision,
                observed + VISION_GROWTH,
                1_790_298_010,
                generation,
            ));
        }
    }
    records
}

fn vision_record(
    record_id: &str,
    context_size: u64,
    vision: bool,
    attributed_bytes: u64,
    seconds: i64,
    generation: u64,
) -> WorkloadResourceRecord {
    let mut record = record(
        record_id,
        record_id,
        WorkloadResourceObservationKind::Measurement,
        context_size,
        attributed_bytes,
    );
    record.recorded_at = Some(Timestamp { seconds, nanos: 0 });
    record.realization = Some(WorkloadRealizationKey {
        owner_id: "series".to_owned(),
        generation,
    });
    let inputs = record.inputs.as_mut().expect("inputs");
    if vision {
        inputs.auxiliary_artifact_ids = vec![VISION_PROJECTOR.to_owned()];
    }
    inputs.option_values = vec![ModelOptionValues {
        option_id: "vision".to_owned(),
        values: vec![ModelOptionParameterValue {
            parameter_id: "enabled".to_owned(),
            value: Some(model_option_parameter_value::Value::Boolean(vision)),
        }],
    }];
    record
}
