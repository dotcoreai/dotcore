//! Launches Codex against the site's inference API through configuration overrides on its command
//! line, so nothing is written to the user's Codex configuration and no login is needed.
//!
//! The overrides define a provider that speaks the Responses wire format, the only one Codex
//! supports, and make the selected route the active model. Codex learns the other routes from the
//! API's model listing. The operator's arguments follow the overrides unchanged, so subcommands
//! such as a one-shot run keep working.

use std::ffi::OsString;

use serde_json::json;

use super::agent::{AgentLaunch, LaunchSetup};

/// Builds the Codex launch: the configuration overrides, then the operator's arguments.
pub(super) fn launch(setup: &LaunchSetup<'_>) -> AgentLaunch {
    let overrides = [
        "model_provider=\"dotcore\"".to_owned(),
        format!("model_providers.dotcore={}", provider(setup)),
        format!("model={}", json!(setup.selected)),
    ];

    let mut args = Vec::new();
    for configuration in overrides {
        args.push(OsString::from("-c"));
        args.push(OsString::from(configuration));
    }
    args.extend_from_slice(setup.passthrough);

    AgentLaunch {
        args,
        env: Vec::new(),
        files: Vec::new(),
    }
}

/// Returns the provider definition as an inline TOML table. JSON strings are valid TOML basic
/// strings, which keeps the quoting of the base URL correct.
fn provider(setup: &LaunchSetup<'_>) -> String {
    format!(
        "{{ name = \"Dotcore\", base_url = {}, wire_api = \"responses\" }}",
        json!(setup.base_url)
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_launch_defines_the_provider_and_selected_model_before_the_arguments() {
        let passthrough = [OsString::from("exec"), OsString::from("hello")];
        let setup = LaunchSetup {
            base_url: "http://127.0.0.1:50052/v1".to_owned(),
            routes: vec!["chat", "code"],
            selected: "code",
            passthrough: &passthrough,
            cache_root: &no_cache_root,
        };

        let launch = launch(&setup);

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
                "exec",
                "hello",
            ]
            .map(OsString::from)
        );
        assert!(launch.env.is_empty());
        assert!(launch.files.is_empty());
    }

    fn no_cache_root() -> anyhow::Result<PathBuf> {
        panic!("Codex needs no cache folder")
    }
}
