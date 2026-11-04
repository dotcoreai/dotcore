//! Launches OpenCode against the site's inference API through its inline configuration variable,
//! which OpenCode merges over the user's own configuration without any file being written.
//!
//! The configuration adds the provider with every listed route and makes the selected route the
//! active model. An inline configuration the user already set is kept underneath. The operator's
//! arguments reach OpenCode unchanged, so its subcommands, such as a one-shot run, keep working.

use anyhow::Context;
use serde_json::{Map, Value, json};

use super::agent::{AgentLaunch, LaunchSetup};

/// The variable OpenCode reads its inline configuration from.
const CONFIG_CONTENT_ENV: &str = "OPENCODE_CONFIG_CONTENT";

/// Builds the OpenCode launch: the inline configuration and the operator's arguments.
pub(super) fn launch(setup: &LaunchSetup<'_>) -> anyhow::Result<AgentLaunch> {
    let inherited = std::env::var_os(CONFIG_CONTENT_ENV);
    let inherited = inherited
        .as_deref()
        .map(|raw| {
            raw.to_str()
                .context("the existing OpenCode inline configuration is not valid text")
        })
        .transpose()?;

    Ok(AgentLaunch {
        args: setup.passthrough.to_vec(),
        env: vec![(
            CONFIG_CONTENT_ENV,
            configuration(setup, inherited)?.to_string(),
        )],
        files: Vec::new(),
    })
}

/// Returns the inline configuration: `inherited`, when given, with the provider and the active
/// model merged over it.
fn configuration(setup: &LaunchSetup<'_>, inherited: Option<&str>) -> anyhow::Result<Value> {
    let models = setup
        .routes
        .iter()
        .map(|route| ((*route).to_owned(), json!({ "name": route })))
        .collect::<Map<_, _>>();
    // The provider shape matches the console's connection example. The model is chosen here
    // rather than with an argument, which would come before the operator's subcommand.
    let ours = json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "dotcore": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Dotcore",
                "options": { "baseURL": setup.base_url },
                "models": models,
            },
        },
        "model": format!("dotcore/{}", setup.selected),
    });

    let Some(inherited) = inherited else {
        return Ok(ours);
    };
    let mut merged = serde_json::from_str::<Map<String, Value>>(inherited)
        .map(Value::Object)
        .context("the existing OpenCode inline configuration is not a JSON object")?;
    merge(&mut merged, ours);
    Ok(merged)
}

/// Merges `overlay` into `base`: objects merge key by key, and any other overlay value replaces
/// the base one.
fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn setup() -> LaunchSetup<'static> {
        LaunchSetup {
            base_url: "http://127.0.0.1:50052/v1".to_owned(),
            routes: vec!["chat", "code"],
            selected: "code",
            passthrough: &[],
            cache_root: &no_cache_root,
        }
    }

    fn no_cache_root() -> anyhow::Result<PathBuf> {
        panic!("OpenCode needs no cache folder")
    }

    fn provider() -> Value {
        json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": "Dotcore",
            "options": { "baseURL": "http://127.0.0.1:50052/v1" },
            "models": {
                "chat": { "name": "chat" },
                "code": { "name": "code" },
            },
        })
    }

    #[test]
    fn the_configuration_carries_the_provider_with_every_route_and_the_selected_model() {
        assert_eq!(
            configuration(&setup(), None).expect("configuration"),
            json!({
                "$schema": "https://opencode.ai/config.json",
                "provider": { "dotcore": provider() },
                "model": "dotcore/code",
            })
        );
    }

    #[test]
    fn an_inherited_configuration_is_kept_under_the_provider_and_model() {
        let inherited = json!({
            "theme": "dark",
            "model": "other/m",
            "provider": {
                "other": { "name": "Other" },
                "dotcore": { "name": "Mine", "options": { "timeout": 5 } },
            },
        })
        .to_string();

        let mut expected = provider();
        expected["options"]["timeout"] = json!(5);
        assert_eq!(
            configuration(&setup(), Some(&inherited)).expect("configuration"),
            json!({
                "$schema": "https://opencode.ai/config.json",
                "theme": "dark",
                "model": "dotcore/code",
                "provider": {
                    "other": { "name": "Other" },
                    "dotcore": expected,
                },
            })
        );
    }

    #[test]
    fn an_inherited_configuration_must_be_a_json_object() {
        for inherited in ["not json", "[1]"] {
            let error = configuration(&setup(), Some(inherited)).expect_err(inherited);

            assert_eq!(
                error.to_string(),
                "the existing OpenCode inline configuration is not a JSON object",
                "{inherited}"
            );
        }
    }
}
