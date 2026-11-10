//! Launches Codex against the site's inference API through configuration overrides on its command
//! line and a model catalog kept in the cache folder, so nothing is written to the user's Codex
//! configuration and no login is needed.
//!
//! The overrides define a provider that speaks the Responses wire format, the only one Codex
//! supports, and make the selected route the active model. Codex never reads the API's model
//! listing, so the catalog is how it learns about a model: it holds the selected route only, which
//! keeps Codex's model picker from offering models the site does not serve. The route's entry is
//! derived from one model of Codex's own bundled catalog, so Codex's instructions and tool
//! behavior stay its own, with the features the Responses endpoint cannot serve switched off.
//! When the inference API states the route's context, the entry carries it, so Codex compacts the
//! conversation before a request outgrows the route; otherwise the base model's context applies.
//! Further overrides turn off the built-in tools and ChatGPT connectors the site's models cannot
//! serve and show the model's reasoning. The operator's arguments follow the overrides unchanged,
//! so subcommands such as a one-shot run keep working.

use std::{ffi::OsString, process::Command};

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use super::{
    CodingAgent,
    agent::{AgentLaunch, LaunchSetup, OwnedFile, content_named},
    start_failed,
};

/// The model of Codex's bundled catalog that the route's entry is derived from: a conventional
/// model that uses Codex's standard tool set rather than the code-mode tools of newer ones.
const BASE_MODEL: &str = "gpt-5.5";

/// Share of the route's context, in percent, that the conversation may fill before Codex compacts
/// it. The rest holds the next tool results and reply, and the compaction request itself, which a
/// site's smaller context windows leave little room for; Codex's own default is 90.
const AUTO_COMPACT_PERCENT: u64 = 80;

/// Builds the Codex launch: the configuration overrides, then the operator's arguments.
pub(super) fn launch(setup: &LaunchSetup<'_>) -> anyhow::Result<AgentLaunch> {
    build(setup, &bundled_catalog()?)
}

/// Builds the launch from the catalog bundled with Codex.
fn build(setup: &LaunchSetup<'_>, bundled: &str) -> anyhow::Result<AgentLaunch> {
    let catalog = route_catalog(bundled, setup.selected, setup.selected_context)?;
    let catalog_file = OwnedFile {
        path: content_named(
            &(setup.cache_root)()?.join("code").join("codex"),
            "catalog",
            "json",
            &catalog,
        ),
        content: catalog,
    };
    let catalog_path = catalog_file
        .path
        .to_str()
        .context("the Codex model catalog path is not valid text")?;

    let overrides = [
        "model_provider=\"dotcore\"".to_owned(),
        format!("model_providers.dotcore={}", provider(setup)),
        format!("model={}", json!(setup.selected)),
        format!("model_catalog_json={}", json!(catalog_path)),
        // Codex's hosted web search is executed by the provider, which Dotcore does not offer.
        "web_search=\"disabled\"".to_owned(),
        // Image viewing needs image inputs that the site's models are not assumed to accept.
        "features.view_image=false".to_owned(),
        // Codex only displays the model's reasoning text when asked to.
        "show_raw_agent_reasoning=true".to_owned(),
        // ChatGPT connectors run through OpenAI's backend, which a site's models cannot reach,
        // and their tool definitions would only inflate every request.
        "features.apps=false".to_owned(),
    ];

    let mut args = Vec::new();
    for configuration in overrides {
        args.push(OsString::from("-c"));
        args.push(OsString::from(configuration));
    }
    args.extend_from_slice(setup.passthrough);

    Ok(AgentLaunch {
        args,
        env: Vec::new(),
        files: vec![catalog_file],
    })
}

/// Returns the provider definition as an inline TOML table. JSON strings are valid TOML basic
/// strings, which keeps the quoting of the base URL correct.
fn provider(setup: &LaunchSetup<'_>) -> String {
    format!(
        "{{ name = \"Dotcore\", base_url = {}, wire_api = \"responses\" }}",
        json!(setup.base_url)
    )
}

/// Reads the model catalog bundled with the installed Codex, as JSON text.
fn bundled_catalog() -> anyhow::Result<String> {
    let agent = CodingAgent::Codex;
    let output = Command::new(agent.name())
        .args(["debug", "models", "--bundled"])
        .output()
        .map_err(|error| start_failed(agent, error))?;
    if !output.status.success() {
        bail!(
            "cannot read the model catalog bundled with Codex ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    String::from_utf8(output.stdout).context("the model catalog bundled with Codex is not text")
}

/// Returns a catalog holding only `route`, as an entry derived from the bundled model, with the
/// route's context in tokens when it is known.
fn route_catalog(bundled: &str, route: &str, context: Option<u64>) -> anyhow::Result<String> {
    let mut catalog: Value = serde_json::from_str(bundled)
        .context("cannot parse the model catalog bundled with Codex")?;
    let models = catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("the model catalog bundled with Codex has no model list")?;
    let Some(Value::Object(entry)) = models
        .iter_mut()
        .find(|model| model["slug"] == BASE_MODEL)
        .map(Value::take)
    else {
        bail!("the model catalog bundled with Codex has no model {BASE_MODEL:?} to derive from");
    };

    *models = vec![Value::Object(route_entry(entry, route, context))];
    Ok(catalog.to_string())
}

/// Returns `base` as the route's entry: named after the route, sized to its context when known,
/// and without what the Responses endpoint or the site's models cannot serve.
fn route_entry(
    mut entry: Map<String, Value>,
    route: &str,
    context: Option<u64>,
) -> Map<String, Value> {
    let Value::Object(changes) = json!({
        "slug": route,
        "display_name": route,
        "description": "Dotcore inference route",
        // Reasoning effort and service tiers are controls of OpenAI's models.
        "default_reasoning_level": null,
        "supported_reasoning_levels": [],
        "additional_speed_tiers": [],
        "service_tiers": [],
        "default_service_tier": null,
        "upgrade": null,
        "availability_nux": null,
        // Codex then sends its patch tool as a function, not as a custom tool, and sends neither
        // a tool search nor a verbosity setting, none of which the endpoint accepts.
        "apply_patch_tool_type": null,
        "supports_search_tool": false,
        "support_verbosity": false,
        "default_verbosity": null,
        // Codex sub-agents are offered as a namespaced function tool.
        "multi_agent_version": "v1",
    }) else {
        unreachable!("a JSON object literal");
    };

    entry.extend(changes);
    if let Some(context) = context {
        // The base model's larger maximum would otherwise stay as the ceiling of a configured
        // context window.
        entry.insert("context_window".to_owned(), json!(context));
        entry.insert("max_context_window".to_owned(), json!(context));
        entry.insert(
            "auto_compact_token_limit".to_owned(),
            json!(context * AUTO_COMPACT_PERCENT / 100),
        );
    }
    entry
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// A trimmed bundled catalog: the base model with fields the entry changes and one it keeps,
    /// next to another model.
    const BUNDLED: &str = r#"{"models":[
        {"slug":"gpt-6.1-sol","display_name":"GPT-6.1-Sol","priority":1},
        {"slug":"gpt-5.5","display_name":"GPT-5.5","description":"Older model.",
         "default_reasoning_level":"low",
         "supported_reasoning_levels":[{"effort":"low","description":"Fast"}],
         "additional_speed_tiers":["fast"],
         "service_tiers":[{"id":"priority","name":"Fast","description":"2x speed"}],
         "default_service_tier":"priority",
         "upgrade":{"model":"gpt-6.1-sol"},
         "availability_nux":{"message":"Try it"},
         "apply_patch_tool_type":"freeform","supports_search_tool":true,
         "support_verbosity":true,"default_verbosity":"low",
         "shell_type":"unified_exec","context_window":272000,"max_context_window":272000,
         "base_instructions":"You are Codex."}]}"#;

    #[test]
    fn the_launch_defines_the_provider_catalog_and_selected_model_before_the_arguments() {
        let passthrough = [OsString::from("exec"), OsString::from("hello")];
        let setup = LaunchSetup {
            base_url: "http://127.0.0.1:50052/v1".to_owned(),
            routes: vec!["chat", "code"],
            selected: "code",
            selected_context: Some(65536),
            passthrough: &passthrough,
            cache_root: &|| Ok(PathBuf::from("/cache/dotcore")),
        };

        let launch = build(&setup, BUNDLED).expect("codex launch");

        let catalog = route_catalog(BUNDLED, "code", Some(65536)).expect("catalog");
        let path = content_named(
            Path::new("/cache/dotcore/code/codex"),
            "catalog",
            "json",
            &catalog,
        );
        assert_eq!(
            launch.args,
            [
                "-c",
                "model_provider=\"dotcore\"",
                "-c",
                "model_providers.dotcore={ name = \"Dotcore\", base_url = \
                 \"http://127.0.0.1:50052/v1\", wire_api = \"responses\" }",
                "-c",
                "model=\"code\"",
                "-c",
                &format!("model_catalog_json=\"{}\"", path.display()),
                "-c",
                "web_search=\"disabled\"",
                "-c",
                "features.view_image=false",
                "-c",
                "show_raw_agent_reasoning=true",
                "-c",
                "features.apps=false",
                "exec",
                "hello",
            ]
            .map(OsString::from)
        );
        assert!(launch.env.is_empty());
        assert_eq!(
            launch.files,
            [OwnedFile {
                path,
                content: catalog
            }]
        );
    }

    #[test]
    fn the_catalog_holds_only_the_route_derived_from_the_base_model() {
        let catalog = route_catalog(BUNDLED, "code", None).expect("catalog");

        assert_eq!(
            serde_json::from_str::<Value>(&catalog).expect("catalog JSON"),
            json!({"models": [{
                "slug": "code",
                "display_name": "code",
                "description": "Dotcore inference route",
                "default_reasoning_level": null,
                "supported_reasoning_levels": [],
                "additional_speed_tiers": [],
                "service_tiers": [],
                "default_service_tier": null,
                "upgrade": null,
                "availability_nux": null,
                "apply_patch_tool_type": null,
                "supports_search_tool": false,
                "support_verbosity": false,
                "default_verbosity": null,
                "multi_agent_version": "v1",
                "shell_type": "unified_exec",
                "context_window": 272000,
                "max_context_window": 272000,
                "base_instructions": "You are Codex.",
            }]})
        );
    }

    #[test]
    fn a_known_route_context_sizes_the_entry_and_compacts_before_it_fills() {
        let catalog = route_catalog(BUNDLED, "code", Some(65536)).expect("catalog");

        let catalog = serde_json::from_str::<Value>(&catalog).expect("catalog JSON");
        let entry = &catalog["models"][0];
        assert_eq!(
            [
                &entry["context_window"],
                &entry["max_context_window"],
                &entry["auto_compact_token_limit"],
            ],
            [&json!(65536), &json!(65536), &json!(52428)]
        );
    }

    #[test]
    fn a_bundled_catalog_that_cannot_be_used_fails_saying_why() {
        let cases = [
            (
                "not json",
                "cannot parse the model catalog bundled with Codex",
            ),
            (
                r#"{"other":[]}"#,
                "the model catalog bundled with Codex has no model list",
            ),
            (
                r#"{"models":[{"slug":"gpt-6.1-sol"}]}"#,
                "the model catalog bundled with Codex has no model \"gpt-5.5\" to derive from",
            ),
        ];

        for (bundled, message) in cases {
            let error = route_catalog(bundled, "code", Some(65536)).expect_err(bundled);

            assert_eq!(error.to_string(), message);
        }
    }
}
