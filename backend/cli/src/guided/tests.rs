//! Whole-flow tests of guided site initialization.
//!
//! Drive a scripted terminal through the recommended path, skipped steps, cancellation at every
//! prompt, a declined confirmation, and equivalence with non-interactive arguments. Step-specific
//! behavior is tested next to its step.

use std::{
    collections::BTreeSet,
    net::Ipv4Addr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use dotcore_domain::SiteNodeHost;
use dotcore_proto::dotcore::site::v1::{
    ModelRuntimeBinding, NativeRuntimeAdapter, NativeWorkloadLifetime, execution_target,
    model_runtime_binding,
};
use dotcore_site::initial::request::{
    InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
    derive_application_execution_target_id, derive_model_execution_target_id, derive_site_node_id,
};

use super::{
    GuidedInitialSiteCollection, GuidedInitialSiteDefaults,
    summary::NODE_HOST_CONSEQUENCE,
    test_support::{
        FixedIdentities, TemporaryRoot, apple_prober, available_prober, collect,
        collect_with_defaults, counting_prober, two_gpu_prober, unavailable_nvidia_prober,
    },
};
use crate::{
    hosts::{HostCandidate, SelectedHost},
    terminal::{
        InputPrompt, Message, PromptError, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal,
    },
};

#[test]
fn collects_recommended_defaults_validates_inputs_and_never_writes_before_confirmation() {
    let root = TemporaryRoot::new();
    let site_dir = root.path().join("guided-site");
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::SelectDefault,
        ScriptedAnswer::Input(String::new()),
        ScriptedAnswer::MultiSelectDefault,
        ScriptedAnswer::MultiSelectDefault,
        ScriptedAnswer::Confirm(true),
    ]);
    let prober = available_prober();
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    let collection = collect(
        site_dir.clone(),
        &mut terminal,
        lan_candidates(),
        &prober,
        &mut identities,
    );

    let GuidedInitialSiteCollection::Confirmed(plan) = collection.expect("confirmed plan") else {
        panic!("guided confirmation must return the prepared plan")
    };
    assert_eq!(plan.site_dir(), site_dir);
    assert_eq!(
        plan.request().site_node_host().ipv4(),
        Some(Ipv4Addr::new(192, 0, 2, 10))
    );
    assert_eq!(plan.request().site_node_host_override(), None);
    assert_eq!(
        plan.request().inference(),
        &BTreeSet::from([InitialInferenceRuntime::Nvidia])
    );
    assert_eq!(
        plan.request().applications(),
        &BTreeSet::from([InitialApplicationRuntime::Docker])
    );
    assert_eq!(plan.inputs().site().site_id, "site-0000000001");
    // Without a hostname default or typed name the node is called "manager"; ids derive from it.
    let node_id = derive_site_node_id("site-0000000001", "manager");
    assert_eq!(plan.inputs().site().nodes[0].node_id, node_id);
    assert_eq!(plan.inputs().site().nodes[0].display_name, "manager");
    assert_eq!(
        plan.inputs().site().execution_targets[0].execution_target_id,
        derive_model_execution_target_id(
            &node_id,
            InitialInferenceRuntime::Nvidia,
            Some("nvidia.com/gpu=gpu0")
        )
    );
    assert_eq!(
        plan.inputs().site().execution_targets[1].execution_target_id,
        derive_application_execution_target_id(&node_id, InitialApplicationRuntime::Docker)
    );
    assert_eq!(
        summary_row(&terminal, "models"),
        "NVIDIA RTX PRO 6000 Blackwell"
    );
    assert_eq!(
        summary_row(&terminal, "applications"),
        "Docker applications with NVIDIA RTX PRO 6000 Blackwell"
    );
    assert!(
        !site_dir.exists(),
        "collection must not persist site inputs"
    );
    assert_eq!(
        prompt_titles(&terminal),
        [
            "Node host",
            "Node name",
            "Inference",
            "Applications",
            "Write this site configuration?",
        ]
    );
    assert!(
        terminal
            .lines()
            .contains(&"Accelerator detection: nvidia detected, apple not detected".to_owned())
    );
    assert!(
        terminal
            .lines()
            .contains(&"Applications detection: docker detected".to_owned())
    );
}

#[test]
fn summary_is_one_heading_and_one_key_value_block_before_the_confirmation() {
    let root = TemporaryRoot::new();
    let site_dir = root.path().join("guided-site");
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);
    let defaults = resolved_defaults(site_dir.clone(), Ipv4Addr::new(192, 0, 2, 10));
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    let collection = collect_with_defaults(
        defaults,
        &mut terminal,
        &available_prober(),
        &mut identities,
    )
    .expect("declined plan");

    assert!(matches!(collection, GuidedInitialSiteCollection::Declined));
    assert_eq!(
        terminal.messages(),
        [
            Message::Heading("Initial site plan".to_owned()),
            Message::KeyValues(
                [
                    ("site directory", site_dir.display().to_string()),
                    ("host", "192.0.2.10 (from --host)".to_owned()),
                    ("node name", "studio".to_owned()),
                    ("models", "Simulated NVIDIA GeForce RTX 5090".to_owned()),
                    ("applications", "Simulated applications".to_owned()),
                ]
                .map(|(key, value)| (key.to_owned(), value))
                .to_vec()
            ),
            Message::Hint(NODE_HOST_CONSEQUENCE.to_owned()),
        ]
    );
    let [ScriptedPrompt::Confirm(confirm)] = terminal.prompts() else {
        panic!("only the confirmation is asked: {:?}", terminal.prompts())
    };
    assert!(confirm.default(), "confirmation must default to yes");
    assert!(!site_dir.exists());
}

#[test]
fn decline_at_confirmation_returns_no_plan() {
    let root = TemporaryRoot::new();
    let site_dir = root.path().join("guided-site");
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::SelectDefault,
        ScriptedAnswer::Input(String::new()),
        ScriptedAnswer::MultiSelect(vec![0]),
        ScriptedAnswer::Confirm(false),
    ]);
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    let collection = collect(
        site_dir.clone(),
        &mut terminal,
        Vec::new(),
        &unavailable_nvidia_prober(),
        &mut identities,
    )
    .expect("declined guided collection");

    assert!(matches!(collection, GuidedInitialSiteCollection::Declined));
    assert_eq!(terminal.remaining_answers(), 0);
    assert!(!site_dir.exists());
}

#[test]
fn cancellation_at_every_prompt_stops_without_a_plan() {
    let root = TemporaryRoot::new();
    let site_dir = root.path().join("guided-site");
    // Host selector, another address, node name, inference, applications, confirmation.
    let answers = || {
        vec![
            ScriptedAnswer::Select(2),
            ScriptedAnswer::Input("192.0.2.20".to_owned()),
            ScriptedAnswer::Input(String::new()),
            ScriptedAnswer::MultiSelect(vec![0]),
            ScriptedAnswer::MultiSelect(vec![0]),
            ScriptedAnswer::Confirm(true),
        ]
    };

    for step in 0..answers().len() {
        let mut script = answers();
        script.truncate(step);
        script.push(ScriptedAnswer::Cancelled);
        let mut terminal = ScriptedTerminal::interactive(script);
        let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

        let result = collect(
            site_dir.clone(),
            &mut terminal,
            lan_candidates(),
            &available_prober(),
            &mut identities,
        );

        let Err(error) = result else {
            panic!("cancellation at step {step} unexpectedly produced a collection")
        };
        assert!(
            error.chain().any(|cause| matches!(
                cause.downcast_ref::<PromptError>(),
                Some(PromptError::Cancelled)
            )),
            "step {step}: {error:#}"
        );
        assert_eq!(terminal.prompts().len(), step + 1, "step {step}");
        assert_eq!(terminal.remaining_answers(), 0, "step {step}");
    }
}

#[test]
fn end_of_input_stops_before_confirmation() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::EndOfInput]);
    let defaults = GuidedInitialSiteDefaults::new(root.path().join("site"))
        .with_host(SelectedHost::explicit(SiteNodeHost::LOOPBACK));
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    let result = collect_with_defaults(
        defaults,
        &mut terminal,
        &unavailable_nvidia_prober(),
        &mut identities,
    );

    let Err(error) = result else {
        panic!("guided end of input unexpectedly succeeded")
    };
    assert_eq!(
        format!("{error:#}"),
        "guided interaction reached end of input"
    );
    assert_eq!(prompt_titles(&terminal), ["Node name"]);
}

#[test]
fn resolved_values_skip_collection_questions_and_only_confirm_prepared_plan() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);
    let defaults = resolved_defaults(
        root.path().join("guided-site"),
        Ipv4Addr::new(192, 0, 2, 10),
    );
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    let collection = collect_with_defaults(
        defaults,
        &mut terminal,
        &available_prober(),
        &mut identities,
    );

    let GuidedInitialSiteCollection::Confirmed(plan) = collection.expect("confirmed plan") else {
        panic!("guided confirmation must return the prepared plan")
    };
    assert_eq!(
        plan.request().site_node_host().ipv4(),
        Some(Ipv4Addr::new(192, 0, 2, 10))
    );
    assert_eq!(prompt_titles(&terminal), ["Write this site configuration?"]);
    assert!(!plan.site_dir().exists());
}

#[test]
fn resolved_workloads_skip_capability_recommendation_probes() {
    let root = TemporaryRoot::new();
    let docker_calls = Arc::new(AtomicUsize::new(0));
    let nvidia_calls = Arc::new(AtomicUsize::new(0));
    let prober = counting_prober(&docker_calls, &nvidia_calls);
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);
    let defaults = resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST);
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    collect_with_defaults(defaults, &mut terminal, &prober, &mut identities)
        .expect("resolved guided plan");

    assert_eq!(docker_calls.load(Ordering::SeqCst), 0);
    assert_eq!(nvidia_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn final_guided_simulated_choice_rejects_nvidia_selectors() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([]);
    let defaults = resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST)
        .with_nvidia_cdi_selectors(["nvidia.com/gpu=gpu0"]);
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);

    let result = collect_with_defaults(
        defaults,
        &mut terminal,
        &available_prober(),
        &mut identities,
    );
    let Err(error) = result else {
        panic!("Simulated inference accepted NVIDIA selectors")
    };

    assert!(
        error
            .to_string()
            .contains("requires the final guided inference choices to include nvidia")
    );
    assert!(terminal.prompts().is_empty());
}

#[test]
fn guided_and_equivalent_flags_produce_byte_equivalent_inputs() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);
    let prober = available_prober();
    let defaults = resolved_defaults(
        root.path().join("guided-site"),
        Ipv4Addr::new(192, 0, 2, 10),
    );
    let mut guided_identities = FixedIdentities::new([1, 2, 3, 4, 5]);
    let guided = collect_with_defaults(defaults, &mut terminal, &prober, &mut guided_identities)
        .expect("guided plan");
    let GuidedInitialSiteCollection::Confirmed(guided) = guided else {
        panic!("guided plan was declined")
    };

    let direct_request = InitialSiteRequest::new(
        [InitialInferenceRuntime::Simulated],
        [InitialApplicationRuntime::Simulated],
    )
    .with_site_node_host_override("192.0.2.10")
    .expect("direct host")
    .with_site_node_display_name_override("studio");
    let mut direct_identities = FixedIdentities::new([1, 2, 3, 4, 5]);
    let direct = crate::site::prepare_initial_site(
        direct_request,
        &prober,
        crate::site::test_support::lan_machine(),
        &mut direct_identities,
    );
    let direct = crate::build_runtime()
        .expect("runtime")
        .block_on(direct)
        .expect("direct plan");

    assert_eq!(
        guided.inputs().site_yaml().expect("guided site YAML"),
        direct.inputs.site_yaml().expect("direct site YAML")
    );
    assert_eq!(
        guided.inputs().node_yaml().expect("guided node YAML"),
        direct.inputs.node_yaml().expect("direct node YAML")
    );
}

#[test]
fn a_gpu_left_out_matches_the_equivalent_device_flags() {
    let root = TemporaryRoot::new();
    let prober = two_gpu_prober();
    let defaults = resolved_defaults(
        root.path().join("guided-site"),
        Ipv4Addr::new(192, 0, 2, 10),
    );
    let defaults = GuidedInitialSiteDefaults {
        inference: None,
        ..defaults
    };
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::MultiSelect(vec![1]),
        ScriptedAnswer::Confirm(true),
    ]);
    let guided = collect_with_defaults(
        defaults,
        &mut terminal,
        &prober,
        &mut FixedIdentities::new([1, 2, 3, 4, 5]),
    )
    .expect("guided plan");
    let GuidedInitialSiteCollection::Confirmed(guided) = guided else {
        panic!("guided plan was declined")
    };

    let direct_request = InitialSiteRequest::new(
        [InitialInferenceRuntime::Nvidia],
        [InitialApplicationRuntime::Simulated],
    )
    .with_nvidia_cdi_selectors(["nvidia.com/gpu=gpu1"])
    .with_site_node_host_override("192.0.2.10")
    .expect("direct host")
    .with_site_node_display_name_override("studio");
    let direct = crate::build_runtime()
        .expect("runtime")
        .block_on(crate::site::prepare_initial_site(
            direct_request,
            &prober,
            crate::site::test_support::lan_machine(),
            &mut FixedIdentities::new([1, 2, 3, 4, 5]),
        ))
        .expect("direct plan");

    assert_eq!(
        guided.inputs().site_yaml().expect("guided site YAML"),
        direct.inputs.site_yaml().expect("direct site YAML")
    );
    assert_eq!(
        summary_row(&terminal, "models"),
        "NVIDIA RTX PRO 6000 Blackwell · GPU 1"
    );
}

#[test]
fn the_gpu_picked_for_models_also_goes_to_docker_applications() {
    let root = TemporaryRoot::new();
    let defaults = GuidedInitialSiteDefaults {
        inference: None,
        ..resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST)
    }
    .with_applications([InitialApplicationRuntime::Docker]);
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::MultiSelect(vec![1]),
        ScriptedAnswer::Confirm(true),
    ]);

    let collection = collect_with_defaults(
        defaults,
        &mut terminal,
        &two_gpu_prober(),
        &mut FixedIdentities::new([1, 2, 3, 4, 5]),
    )
    .expect("guided plan");

    let GuidedInitialSiteCollection::Confirmed(plan) = collection else {
        panic!("guided plan was declined")
    };
    let devices = |model: bool| {
        plan.inputs()
            .site()
            .execution_targets
            .iter()
            .filter(|target| {
                matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ModelRuntime(_))
                ) == model
            })
            .map(|target| target.accelerator_device_ids.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(devices(true), [vec!["nvidia.com/gpu=gpu1".to_owned()]]);
    assert_eq!(devices(false), [vec!["nvidia.com/gpu=gpu1".to_owned()]]);
    assert_eq!(
        summary_row(&terminal, "applications"),
        "Docker applications with NVIDIA RTX PRO 6000 Blackwell · GPU 1"
    );
}

#[test]
fn checking_nothing_plans_a_node_without_targets() {
    let root = TemporaryRoot::new();
    let defaults = GuidedInitialSiteDefaults {
        inference: None,
        applications: None,
        ..resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST)
    };
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::MultiSelect(Vec::new()),
        ScriptedAnswer::MultiSelect(Vec::new()),
        ScriptedAnswer::Confirm(true),
    ]);

    let collection = collect_with_defaults(
        defaults,
        &mut terminal,
        &available_prober(),
        &mut FixedIdentities::new([1, 2, 3, 4, 5]),
    )
    .expect("confirmed plan");

    let GuidedInitialSiteCollection::Confirmed(plan) = collection else {
        panic!("confirmed plan")
    };
    assert!(plan.request().inference().is_empty());
    assert!(plan.request().applications().is_empty());
    assert!(plan.inputs().site().execution_targets.is_empty());
    assert_eq!(summary_row(&terminal, "models"), "none");
    assert_eq!(summary_row(&terminal, "applications"), "none");
    assert!(
        terminal
            .lines()
            .contains(&"No inference: this node runs no local models.".to_owned())
    );
}

#[test]
fn accepting_the_offered_node_name_is_not_operator_intent() {
    for answer in ["", "studio", " studio "] {
        let root = TemporaryRoot::new();
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Input(answer.to_owned()),
            ScriptedAnswer::Confirm(true),
        ]);
        let defaults = unnamed_defaults(root.path().join("site"), Ipv4Addr::LOCALHOST);

        let plan = confirmed_plan(defaults, &mut terminal, Some("studio"));

        assert_eq!(plan.inputs().site().nodes[0].display_name, "studio");
        assert_eq!(plan.request().site_node_display_name_override(), None);
        assert_eq!(summary_name(&terminal), "studio");
        assert_eq!(
            terminal.prompts()[0],
            ScriptedPrompt::Input(InputPrompt::new("Node name").with_default("studio"))
        );
    }
}

#[test]
fn a_typed_node_name_replaces_the_offered_one_and_is_explicit() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::Input("  Chosen node ".to_owned()),
        ScriptedAnswer::Confirm(true),
    ]);
    let defaults = unnamed_defaults(root.path().join("site"), Ipv4Addr::LOCALHOST);

    let plan = confirmed_plan(defaults, &mut terminal, Some("studio"));

    assert_eq!(plan.inputs().site().nodes[0].display_name, "Chosen node");
    assert_eq!(
        plan.request().site_node_display_name_override(),
        Some("Chosen node")
    );
    assert_eq!(summary_name(&terminal), "Chosen node");
}

#[test]
fn an_invalid_node_name_is_explained_and_asked_again() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::Input("bad\u{7}name".to_owned()),
        ScriptedAnswer::Input("a".repeat(129)),
        ScriptedAnswer::Input("Chosen".to_owned()),
        ScriptedAnswer::Confirm(true),
    ]);
    let defaults = unnamed_defaults(root.path().join("site"), Ipv4Addr::LOCALHOST);

    let plan = confirmed_plan(defaults, &mut terminal, Some("studio"));

    assert_eq!(plan.inputs().site().nodes[0].display_name, "Chosen");
    assert_eq!(
        prompt_titles(&terminal),
        [
            "Node name",
            "Node name",
            "Node name",
            "Write this site configuration?"
        ]
    );
    let error = Message::Error("Node name must contain 1 to 128 non-control characters".to_owned());
    assert_eq!(
        terminal
            .messages()
            .iter()
            .filter(|message| **message == error)
            .count(),
        2
    );
}

#[test]
fn the_name_flag_skips_the_node_name_prompt() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);
    let defaults =
        unnamed_defaults(root.path().join("site"), Ipv4Addr::LOCALHOST).with_node_name("Flagged");

    let plan = confirmed_plan(defaults, &mut terminal, Some("studio"));

    assert_eq!(prompt_titles(&terminal), ["Write this site configuration?"]);
    assert_eq!(plan.inputs().site().nodes[0].display_name, "Flagged");
    assert_eq!(summary_name(&terminal), "Flagged");
}

#[test]
fn without_a_hostname_default_an_empty_answer_names_the_node_manager() {
    let root = TemporaryRoot::new();
    let mut terminal = ScriptedTerminal::interactive([
        ScriptedAnswer::Input(String::new()),
        ScriptedAnswer::Confirm(true),
    ]);
    let defaults = unnamed_defaults(root.path().join("site"), Ipv4Addr::LOCALHOST);

    let plan = confirmed_plan(defaults, &mut terminal, None);

    assert_eq!(
        terminal.prompts()[0],
        ScriptedPrompt::Input(InputPrompt::new("Node name"))
    );
    assert_eq!(plan.inputs().site().nodes[0].display_name, "manager");
    assert_eq!(summary_name(&terminal), "manager");
}

#[test]
fn apple_inference_uses_the_observed_device_and_the_requested_lifetime() {
    let root = TemporaryRoot::new();
    let defaults = resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST)
        .with_inference([InitialInferenceRuntime::Apple])
        .with_native_workload_lifetime(NativeWorkloadLifetime::Ephemeral);
    let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

    let collection = collect_with_defaults(
        defaults,
        &mut terminal,
        &apple_prober(),
        &mut FixedIdentities::new([1, 2, 3, 4, 5]),
    )
    .expect("confirmed plan");

    let GuidedInitialSiteCollection::Confirmed(plan) = collection else {
        panic!("confirmed plan")
    };
    assert_eq!(
        plan.request().inference(),
        &BTreeSet::from([InitialInferenceRuntime::Apple])
    );
    assert_eq!(
        plan.request().native_workload_lifetime(),
        NativeWorkloadLifetime::Ephemeral
    );
    let model = &plan.inputs().site().execution_targets[0];
    assert_eq!(model.accelerator_device_ids, ["apple-gpu-0"]);
    assert_eq!(
        model.runtime_binding,
        Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                    NativeRuntimeAdapter {
                        workload_lifetime: NativeWorkloadLifetime::Ephemeral as i32,
                    }
                )),
            }
        ))
    );
}

#[test]
fn apple_inference_asks_for_the_lifetime_and_summarizes_the_choice() {
    for (answer, expected, label) in [
        (
            ScriptedAnswer::Select(1),
            NativeWorkloadLifetime::Ephemeral,
            "ephemeral",
        ),
        (
            ScriptedAnswer::SelectDefault,
            NativeWorkloadLifetime::Persistent,
            "persistent",
        ),
    ] {
        let root = TemporaryRoot::new();
        let defaults = resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST)
            .with_inference([InitialInferenceRuntime::Apple]);
        let mut terminal = ScriptedTerminal::interactive([answer, ScriptedAnswer::Confirm(true)]);

        let collection = collect_with_defaults(
            defaults,
            &mut terminal,
            &apple_prober(),
            &mut FixedIdentities::new([1, 2, 3, 4, 5]),
        )
        .expect("confirmed plan");

        let GuidedInitialSiteCollection::Confirmed(plan) = collection else {
            panic!("confirmed plan")
        };
        assert_eq!(plan.request().native_workload_lifetime(), expected);
        assert_eq!(
            prompt_titles(&terminal),
            ["Native workloads", "Write this site configuration?"]
        );
        assert_eq!(summary_row(&terminal, "native workloads"), label);
    }
}

#[test]
fn native_workload_lifetime_is_rejected_for_a_non_apple_final_choice() {
    let root = TemporaryRoot::new();
    let defaults = resolved_defaults(root.path().join("guided-site"), Ipv4Addr::LOCALHOST)
        .with_native_workload_lifetime(NativeWorkloadLifetime::Ephemeral);

    let error = collect_with_defaults(
        defaults,
        &mut ScriptedTerminal::interactive([]),
        &apple_prober(),
        &mut FixedIdentities::new([1, 2, 3, 4, 5]),
    )
    .err()
    .expect("lifetime without Apple");

    assert!(error.to_string().contains(
        "--native-workload-lifetime requires the final guided inference choices to include \
                 apple"
    ));
}

/// A machine with a LAN address and loopback, as discovery ranks them.
fn lan_candidates() -> Vec<HostCandidate> {
    [
        (Ipv4Addr::new(192, 0, 2, 10), "address on en0", true),
        (Ipv4Addr::LOCALHOST, "loopback", false),
    ]
    .into_iter()
    .map(|(address, label, recommendable)| {
        HostCandidate::new(
            SiteNodeHost::from_ipv4(address).expect("specific host"),
            label,
            recommendable,
        )
    })
    .collect()
}

fn resolved_defaults(site_dir: std::path::PathBuf, host: Ipv4Addr) -> GuidedInitialSiteDefaults {
    unnamed_defaults(site_dir, host).with_node_name("studio")
}

fn unnamed_defaults(site_dir: std::path::PathBuf, host: Ipv4Addr) -> GuidedInitialSiteDefaults {
    GuidedInitialSiteDefaults::new(site_dir)
        .with_host(SelectedHost::explicit(
            SiteNodeHost::from_ipv4(host).expect("specific host"),
        ))
        .with_inference([InitialInferenceRuntime::Simulated])
        .with_applications([InitialApplicationRuntime::Simulated])
}

fn confirmed_plan(
    defaults: GuidedInitialSiteDefaults,
    terminal: &mut ScriptedTerminal,
    suggested_name: Option<&str>,
) -> Box<super::GuidedInitialSitePlan> {
    let mut identities = FixedIdentities::new([1, 2, 3, 4, 5]);
    if let Some(name) = suggested_name {
        identities = identities.with_node_name(name);
    }
    let collection =
        collect_with_defaults(defaults, terminal, &available_prober(), &mut identities)
            .expect("guided plan");
    let GuidedInitialSiteCollection::Confirmed(plan) = collection else {
        panic!("guided plan was declined")
    };
    plan
}

fn summary_name(terminal: &ScriptedTerminal) -> String {
    summary_row(terminal, "node name")
}

fn summary_row(terminal: &ScriptedTerminal, key: &str) -> String {
    terminal
        .messages()
        .iter()
        .find_map(|message| match message {
            Message::KeyValues(rows) => rows
                .iter()
                .find(|(row_key, _)| row_key == key)
                .map(|(_, value)| value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("summary row {key}"))
}

fn prompt_titles(terminal: &ScriptedTerminal) -> Vec<&str> {
    terminal
        .prompts()
        .iter()
        .map(|prompt| match prompt {
            ScriptedPrompt::Select(prompt) => prompt.title(),
            ScriptedPrompt::MultiSelect(prompt) => prompt.title(),
            ScriptedPrompt::Confirm(prompt) => prompt.title(),
            ScriptedPrompt::Input(prompt) | ScriptedPrompt::Secret(prompt) => prompt.title(),
        })
        .collect()
}
