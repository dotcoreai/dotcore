//! Whole-flow tests of the guided model deploy.
//!
//! Drive a scripted terminal through the forms against a fake site: the basic path, skipped
//! steps, command-line answers, going back, a refused deployment, cancellation and a line
//! terminal, and check that the payload it builds deploys the same way from a payload file.

use dotcore_proto::dotcore::{
    catalog::v1::{
        ModelOptionParameterValue, ModelOptionValues, ParameterValues,
        model_option_parameter_value, parameter_value,
    },
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        LocalModelDeploymentConfiguration, ModelDeploymentExposure,
        create_model_deployment_request, model_deployment_exposure,
    },
};
use tonic::Status;

use super::{
    Deployed, WizardDefaults, route_name, run,
    test_support::{FakeSite, gpu1, gpu2, value},
};
use crate::{
    deploy::Setting,
    model::{
        deploy::{create_request, resolve_payload},
        payload::ModelPayload,
    },
    terminal::{
        FormAnswer, FormPrompt, Message, PromptError, ScriptedAnswer, ScriptedPrompt,
        ScriptedTerminal,
    },
};

#[tokio::test(flavor = "multi_thread")]
async fn the_basic_path_shows_each_step_as_a_form_and_deploys_what_was_reviewed() {
    let site = FakeSite::two_machines();
    let mut terminal = ScriptedTerminal::interactive(basic_answers());

    let deployed = run(&site, &mut terminal, WizardDefaults::default())
        .await
        .expect("deployed");

    assert_eq!(deployed.deployment.model_deployment_id, "created-1");
    assert_eq!(terminal.remaining_answers(), 0);
    assert_eq!(
        terminal.lines(),
        [
            "Model & machine",
            "✘ Llama 4 Tiny can't be deployed: no profile runs on any machine",
            "✘ long-context can't be deployed: not supported on gpu1's hardware",
            "✔ Model & machine: Qwen 3.8 27B · balanced on gpu1 CUDA",
            "✔ Capabilities: Vision enabled true",
            "✘ Parallel requests expects a whole number",
            "✔ Context: 32768 per request · 2 parallel · 131072 total",
            "✔ Review & deploy: Qwen 3.8 27B · route qwen-3.8-27b",
            "✔ Memory on gpu1 CUDA: GPU 0 16.0 GiB of 24.0 GiB · fits",
        ],
        "one line per step, no memory block"
    );
    assert_eq!(
        titles(&terminal),
        [
            "select Model",
            "select Model",
            "select Machine",
            "select Profile",
            "select Profile",
            "form Capabilities: Vision enabled",
            "form Context: Parallel requests, Context per request, Total context, Batch size",
            "form Context: Parallel requests, Context per request, Total context, Batch size",
            "form Review & deploy: Name, Route",
        ]
    );
    assert_eq!(
        items(&terminal, 0),
        [
            "Llama 4 Tiny: llama-4-tiny (unavailable: no profile runs on any machine)",
            "Qwen 3.8 27B: qwen3-8-27b · fits now",
            "Gemma 4 E4B: gemma-4-e4b · fits now",
        ]
    );
    let ScriptedPrompt::Select(model) = &terminal.prompts()[0] else {
        panic!("the first prompt picks the model");
    };
    assert_eq!(model.default(), 1, "the first model that fits now");
    assert_eq!(
        items(&terminal, 2),
        ["gpu1 CUDA: fits now", "gpu2 CUDA: needs memory freed"]
    );
    assert_eq!(
        items(&terminal, 3),
        [
            "balanced: fits now · fits up to 49152 per request, all requests at once",
            "long-context (unavailable: not supported on gpu1's hardware)",
        ]
    );

    let context = form(&terminal, 6);
    assert_eq!(
        context.steps,
        [
            "Model & machine",
            "Capabilities",
            "Context",
            "Review & deploy"
        ]
    );
    assert_eq!(context.current, 2);
    assert_eq!(
        fields(context),
        [
            "Parallel requests = [4] (1-64) What parallel requests does.",
            "Context per request = [32768] (4096-262144 tokens) What context per request does. ⇥ Fits all 4 at once: 49152",
            "Total context = [131072] (follows the two above) What total context does.",
            "Batch size = [512] (1-8192 tokens) What batch size does.",
        ]
    );
    let review = form(&terminal, 8);
    assert_eq!(
        fields(review),
        [
            "Name = [Qwen 3.8 27B] () The name the console and dotc list the deployment by.",
            "Route = [qwen-3.8-27b] (follows the name) The model name callers use.",
        ]
    );
    assert_eq!(
        review
            .notes
            .iter()
            .flat_map(Message::plain_lines)
            .collect::<Vec<_>>(),
        [
            "  Runs on         gpu1 CUDA",
            "  Profile         balanced",
            "  Context         32768 per request · 2 parallel · 131072 total",
            "  Vision enabled  true",
            "  Batch size      1024",
        ]
    );
    assert_eq!(review.action.as_deref(), Some("Deploy"));
    assert_eq!(
        terminal.footers(),
        [footer(4), footer(2)],
        "the footer follows the parallel requests typed"
    );

    let requests = site.requests();
    assert_eq!(requests.previews.len(), 1);
    assert_eq!(
        requests.estimates.len(),
        4,
        "the starting values, vision on, two parallel requests, then the batch size"
    );
    assert_eq!(
        requests.creates,
        [create(
            gpu1(),
            vec![
                value("batch-size", parameter_value::Value::UnsignedInteger(1024)),
                value(
                    "parallel-requests",
                    parameter_value::Value::UnsignedInteger(2)
                ),
            ],
            vec![vision(true)],
            "Qwen 3.8 27B",
            "qwen-3.8-27b",
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_target_and_no_basic_option_skip_the_machine_and_capabilities() {
    let site = FakeSite::one_machine();
    let mut terminal = ScriptedTerminal::interactive(accepting_answers());

    run(&site, &mut terminal, named("qwen"))
        .await
        .expect("deployed");

    assert_eq!(
        titles(&terminal),
        [
            "select Profile",
            "form Context: Parallel requests, Context per request, Total context, Batch size",
            "form Review & deploy: Name, Route",
        ]
    );
    assert_eq!(
        form(&terminal, 1).steps,
        ["Model & machine", "Context", "Review & deploy"]
    );
    let requests = site.requests();
    assert_eq!(
        requests.estimates.len(),
        1,
        "nothing changed after the first estimate"
    );
    assert_eq!(
        requests.creates,
        [create(
            gpu1(),
            Vec::new(),
            Vec::new(),
            "Qwen 3.8 27B",
            "qwen-3.8-27b"
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn command_line_choices_answer_their_fields_without_asking() {
    let site = FakeSite::two_machines();
    let mut terminal = ScriptedTerminal::interactive([next(&[]), next(&[])]);

    run(
        &site,
        &mut terminal,
        WizardDefaults {
            profile: Some("balanced".to_owned()),
            node: Some("gpu2".to_owned()),
            name: Some("Chat".to_owned()),
            route: Some("chat".to_owned()),
            settings: vec![
                Setting::parse("parallel-requests=8").expect("setting"),
                Setting::parse("option.vision.enabled=false").expect("setting"),
            ],
            ..named("qwen")
        },
    )
    .await
    .expect("deployed");

    assert_eq!(
        titles(&terminal),
        [
            "form Context: Context per request, Total context, Batch size",
            "form Review & deploy: ",
        ]
    );
    assert_eq!(
        site.requests().creates,
        [create(
            gpu2(),
            vec![value(
                "parallel-requests",
                parameter_value::Value::UnsignedInteger(8)
            )],
            vec![vision(false)],
            "Chat",
            "chat",
        )]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn going_back_reopens_the_previous_step_with_its_values() {
    let site = FakeSite::two_machines();
    let mut answers = basic_answers();
    // Back from the review to the context, then from the context to the capabilities.
    answers.truncate(8);
    answers.extend([
        back(),
        back(),
        next(&[]),
        next(&[("Context per request", "65536")]),
        next(&[]),
    ]);
    let mut terminal = ScriptedTerminal::interactive(answers);

    run(&site, &mut terminal, WizardDefaults::default())
        .await
        .expect("deployed");

    assert_eq!(
        titles(&terminal)[8..],
        [
            "form Review & deploy: Name, Route",
            "form Context: Parallel requests, Context per request, Total context, Batch size",
            "form Capabilities: Vision enabled",
            "form Context: Parallel requests, Context per request, Total context, Batch size",
            "form Review & deploy: Name, Route",
        ]
    );
    assert_eq!(
        values(form(&terminal, 9)),
        ["2", "", "", "1024"],
        "the context reopens with the values typed"
    );
    assert_eq!(values(form(&terminal, 10)), ["true"]);
    assert_eq!(
        site.requests().creates[0],
        create(
            gpu1(),
            vec![
                value("batch-size", parameter_value::Value::UnsignedInteger(1024)),
                value(
                    "context-per-request",
                    parameter_value::Value::UnsignedInteger(65536)
                ),
                value(
                    "parallel-requests",
                    parameter_value::Value::UnsignedInteger(2)
                ),
            ],
            vec![vision(true)],
            "Qwen 3.8 27B",
            "qwen-3.8-27b",
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn going_back_from_the_first_form_asks_the_model_and_machine_again() {
    let site = FakeSite::one_machine();
    let mut answers = vec![
        ScriptedAnswer::SelectDefault,
        back(),
        ScriptedAnswer::Select(0),
    ];
    answers.extend(accepting_answers().into_iter().skip(1));
    let mut terminal = ScriptedTerminal::interactive(answers);

    run(&site, &mut terminal, named("qwen"))
        .await
        .expect("deployed");

    assert_eq!(
        titles(&terminal)[..3],
        [
            "select Profile",
            "form Context: Parallel requests, Context per request, Total context, Batch size",
            "select Profile",
        ]
    );
    assert_eq!(
        terminal.lines()[..4],
        [
            "Model & machine",
            "✔ Model & machine: Qwen 3.8 27B · balanced on gpu1 CUDA",
            "Model & machine",
            "✔ Model & machine: Qwen 3.8 27B · balanced on gpu1 CUDA",
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_deployment_returns_to_the_review_with_the_reason() {
    let site = FakeSite::one_machine();
    site.create_failures
        .lock()
        .expect("failures")
        .push_back(Status::already_exists("route qwen-3.8-27b already exists"));
    let mut answers = accepting_answers();
    answers.push(next(&[("Route", "qwen-2")]));
    let mut terminal = ScriptedTerminal::interactive(answers);

    run(&site, &mut terminal, named("qwen"))
        .await
        .expect("deployed");

    assert_eq!(
        form(&terminal, 3).notes.last().map(Message::plain_lines),
        Some(vec![
            "✘ The manager refused the deployment: route qwen-3.8-27b already exists".to_owned()
        ])
    );
    assert_eq!(
        site.requests().creates,
        [
            create(
                gpu1(),
                Vec::new(),
                Vec::new(),
                "Qwen 3.8 27B",
                "qwen-3.8-27b"
            ),
            create(gpu1(), Vec::new(), Vec::new(), "Qwen 3.8 27B", "qwen-2"),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_at_a_form_or_a_list_sends_nothing() {
    let site = FakeSite::one_machine();
    let mut at_review = accepting_answers();
    at_review.pop();
    at_review.push(ScriptedAnswer::Cancelled);
    let mut at_review = ScriptedTerminal::interactive(at_review);
    let mut at_list = ScriptedTerminal::interactive([ScriptedAnswer::Cancelled]);

    for terminal in [&mut at_review, &mut at_list] {
        let error = run(&site, terminal, named("qwen"))
            .await
            .expect_err("cancelled");
        assert!(
            matches!(error.downcast_ref(), Some(PromptError::Cancelled)),
            "{error:#}"
        );
    }

    let requests = site.requests();
    assert_eq!(requests.estimates.len(), 1, "only the first walk estimated");
    assert_eq!(requests.creates, []);
}

#[tokio::test(flavor = "multi_thread")]
async fn going_back_with_nothing_to_ask_before_cancels() {
    let site = FakeSite::one_machine();
    let mut terminal = ScriptedTerminal::interactive([back()]);

    let error = run(
        &site,
        &mut terminal,
        WizardDefaults {
            profile: Some("balanced".to_owned()),
            ..named("qwen")
        },
    )
    .await
    .expect_err("cancelled");

    assert!(
        matches!(error.downcast_ref(), Some(PromptError::Cancelled)),
        "{error:#}"
    );
    assert_eq!(site.requests().creates, []);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_terminal_asks_each_field_and_states_the_memory_twice() {
    let site = FakeSite::one_machine();
    let mut answers = vec![ScriptedAnswer::SelectDefault];
    answers.extend((0..6).map(|_| ScriptedAnswer::Input(String::new())));
    answers.push(ScriptedAnswer::SelectDefault);
    let mut terminal = ScriptedTerminal::interactive(answers).without_live_footer();

    run(&site, &mut terminal, named("qwen"))
        .await
        .expect("deployed");

    assert_eq!(
        titles(&terminal),
        [
            "select Profile",
            "input Parallel requests (1-64) [4]",
            "input Context per request (4096-262144 tokens) [32768]",
            "input Total context (follows the two above) [131072]",
            "input Batch size (1-8192 tokens) [512]",
            "input Name [Qwen 3.8 27B]",
            "input Route (follows the name) [qwen-3.8-27b]",
            "select Deploy?",
        ]
    );
    let verdicts = terminal
        .lines()
        .iter()
        .filter(|line| *line == "✔ Memory: fits now")
        .count();
    assert_eq!(verdicts, 2, "after the context and at the review");
    let mut expected = lines(&[
        "Model & machine",
        "✔ Model & machine: Qwen 3.8 27B · balanced on gpu1 CUDA",
        "Context",
        "What parallel requests does.",
        "What context per request does.",
        "Fits all 4 at once: 49152",
        "What total context does.",
        "What batch size does.",
        "✔ Context: 32768 per request · 4 parallel · 131072 total",
    ]);
    expected.extend(memory(4));
    expected.push("Review & deploy".to_owned());
    expected.extend(memory(4));
    expected.extend(lines(&[
        "  Runs on  gpu1 CUDA",
        "  Profile  balanced",
        "  Context  32768 per request · 4 parallel · 131072 total",
        "The name the console and dotc list the deployment by.",
        "The model name callers use.",
        "✔ Review & deploy: Qwen 3.8 27B · route qwen-3.8-27b",
        "✔ Memory on gpu1 CUDA: GPU 0 16.0 GiB of 24.0 GiB · fits",
    ]));
    assert_eq!(terminal.lines(), expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_built_payload_deploys_the_same_from_a_payload_file() {
    let site = FakeSite::two_machines();
    let mut terminal = ScriptedTerminal::interactive(basic_answers());
    let Deployed { payload, .. } = run(&site, &mut terminal, WizardDefaults::default())
        .await
        .expect("deployed");

    let text = dotcore_proto::yaml::to_string(&payload).expect("payload text");
    let parsed = ModelPayload::parse(&text).expect("payload");
    let (selection, parsed) = resolve_payload(&site, parsed, false, &[])
        .await
        .expect("resolved");

    assert_eq!(
        [create_request(&selection, &parsed).expect("request")],
        *site.requests().creates
    );
}

#[test]
fn routes_follow_a_slug_of_the_name() {
    assert_eq!(route_name("Qwen 3.8 27B"), "qwen-3.8-27b");
    assert_eq!(route_name("  Chat -- Model.. "), "chat-model");
    assert_eq!(route_name("3B coder"), "model-3b-coder");
    assert_eq!(route_name("!!!"), "");
    assert_eq!(route_name(&"a".repeat(70)), "a".repeat(63));
}

/// The answers of the basic path on the two-machine site: try the model that runs nowhere, then
/// take the preselected Qwen and the first machine, try the profile that can't run then take
/// balanced, turn vision on, mistype then lower the parallel requests and raise the batch size,
/// keep the name and route, and deploy.
fn basic_answers() -> Vec<ScriptedAnswer> {
    vec![
        ScriptedAnswer::Select(0),
        ScriptedAnswer::SelectDefault,
        ScriptedAnswer::SelectDefault,
        ScriptedAnswer::Select(1),
        ScriptedAnswer::SelectDefault,
        next(&[("Vision enabled", "true")]),
        next(&[("Parallel requests", "many")]),
        next(&[("Parallel requests", "2"), ("Batch size", "1024")]),
        next(&[]),
    ]
}

/// The answers that keep every default on the one-machine site, then deploy.
fn accepting_answers() -> Vec<ScriptedAnswer> {
    vec![ScriptedAnswer::SelectDefault, next(&[]), next(&[])]
}

/// Types `edits` into a form by label, then finishes the step.
fn next(edits: &[(&str, &str)]) -> ScriptedAnswer {
    ScriptedAnswer::Form(
        edits
            .iter()
            .map(|(label, value)| ((*label).to_owned(), (*value).to_owned()))
            .collect(),
        FormAnswer::Next,
    )
}

/// Goes back from a form without editing it.
fn back() -> ScriptedAnswer {
    ScriptedAnswer::Form(Vec::new(), FormAnswer::Back)
}

fn named(model: &str) -> WizardDefaults {
    WizardDefaults {
        model: Some(model.to_owned()),
        ..WizardDefaults::default()
    }
}

/// The footer of the fake estimate on gpu1, for `parallel` requests.
fn footer(parallel: u64) -> Vec<String> {
    lines(&[
        "gpu1 CUDA",
        "  GPU 0  ██████▓▓▓▓▓▓▓▓▓▓░░░░░░│░░  16.0 GiB of 24.0 GiB · fits",
        &format!("✔ Memory: fits now · Fits all {parallel} at once: 49152"),
    ])
}

/// The memory block of the fake estimate, after its verdict, for `parallel` requests.
fn memory(parallel: u64) -> Vec<String> {
    lines(&[
        "✔ Memory: fits now",
        "  GPU 0  ██████▓▓▓▓▓▓▓▓▓▓░░░░░░│░░  16.0 GiB of 24.0 GiB · fits",
        "  █ in use  ▓ this deployment  │ fit line",
        &format!("  Context per request fitting all {parallel} at once  49152"),
        "  Derived total-context                      131072",
    ])
}

fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

/// The form prompt at `index` among all prompts, as it opened.
fn form(terminal: &ScriptedTerminal, index: usize) -> &FormPrompt {
    let ScriptedPrompt::Form(prompt) = &terminal.prompts()[index] else {
        panic!("prompt {index} is not a form");
    };
    prompt
}

/// Each field of a form: its label, value, the value it takes while empty, what it accepts, its
/// summary and its hint.
fn fields(form: &FormPrompt) -> Vec<String> {
    form.fields
        .iter()
        .map(|field| {
            let mut text = format!(
                "{} = {}[{}] ({}) {}",
                field.label,
                field.value,
                field.placeholder.as_deref().unwrap_or_default(),
                field.accepts,
                field.summary
            );
            if let Some(hint) = &field.hint {
                text.push_str(&format!(" ⇥ {}", hint.text));
            }
            text
        })
        .collect()
}

/// The values typed in a form's fields as it opened.
fn values(form: &FormPrompt) -> Vec<&str> {
    form.fields
        .iter()
        .map(|field| field.value.as_str())
        .collect()
}

/// The items of the select prompt at `index` among all prompts, with their description or why
/// they are unavailable.
fn items(terminal: &ScriptedTerminal, index: usize) -> Vec<String> {
    let ScriptedPrompt::Select(prompt) = &terminal.prompts()[index] else {
        panic!("prompt {index} is not a select prompt");
    };
    prompt
        .items()
        .iter()
        .map(|item| {
            let mut text = item.label().to_owned();
            if let Some(description) = item.description() {
                text.push_str(&format!(": {description}"));
            }
            if let Some(reason) = item.unavailable_reason() {
                text.push_str(&format!(" (unavailable: {reason})"));
            }
            text
        })
        .collect()
}

/// Every prompt's kind and title, with an input's default and a form's field labels.
fn titles(terminal: &ScriptedTerminal) -> Vec<String> {
    terminal
        .prompts()
        .iter()
        .map(|prompt| match prompt {
            ScriptedPrompt::Select(prompt) => format!("select {}", prompt.title()),
            ScriptedPrompt::MultiSelect(prompt) => format!("check {}", prompt.title()),
            ScriptedPrompt::Confirm(prompt) => format!("confirm {}", prompt.title()),
            ScriptedPrompt::Input(prompt) => format!(
                "input {}{}",
                prompt.title(),
                prompt
                    .default()
                    .map(|default| format!(" [{default}]"))
                    .unwrap_or_default()
            ),
            ScriptedPrompt::Secret(prompt) => format!("secret {}", prompt.title()),
            ScriptedPrompt::Countdown(prompt) => format!("countdown {}", prompt.title()),
            ScriptedPrompt::Form(prompt) => format!(
                "form {}: {}",
                prompt.title,
                prompt
                    .fields
                    .iter()
                    .map(|field| field.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect()
}

fn vision(enabled: bool) -> ModelOptionValues {
    ModelOptionValues {
        option_id: "vision".to_owned(),
        values: vec![ModelOptionParameterValue {
            parameter_id: "enabled".to_owned(),
            value: Some(model_option_parameter_value::Value::Boolean(enabled)),
        }],
    }
}

fn create(
    placement: dotcore_proto::dotcore::deployment::v1::ExactPlacement,
    values: Vec<dotcore_proto::dotcore::catalog::v1::ParameterValue>,
    options: Vec<ModelOptionValues>,
    name: &str,
    route: &str,
) -> CreateModelDeploymentRequest {
    CreateModelDeploymentRequest {
        source: Some(create_model_deployment_request::Source::Local(
            CreateLocalModelDeploymentRequest {
                model_id: "qwen3-8-27b".to_owned(),
                profile_id: "balanced".to_owned(),
                placement: Some(placement),
                configuration: Some(LocalModelDeploymentConfiguration {
                    engine_parameter_values: (!values.is_empty())
                        .then_some(ParameterValues { values }),
                    option_overrides: options,
                    ..LocalModelDeploymentConfiguration::default()
                }),
            },
        )),
        exposure: Some(ModelDeploymentExposure {
            kind: Some(model_deployment_exposure::Kind::Direct(
                DirectRouteExposure {
                    route_name: route.to_owned(),
                },
            )),
        }),
        model_deployment_name: Some(name.to_owned()),
    }
}
