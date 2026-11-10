//! Launches Pi against the site's inference API through an extension kept in the cache folder, so
//! nothing is written to the user's Pi configuration.
//!
//! The extension never changes between launches of one build and its file name carries its
//! content's hash; the provider definition it registers, with every listed route, comes from the
//! environment. The selected route is the starting model, and the operator's arguments follow
//! unchanged.

use std::ffi::OsString;

use serde_json::{Value, json};

use super::agent::{AgentLaunch, LaunchSetup, OwnedFile, content_named};

/// The extension that registers the provider from the environment.
const EXTENSION: &str = include_str!("dotcore-provider.ts");

/// The variable the extension reads the provider definition from.
const PROVIDER_ENV: &str = "DOTCORE_PI_PROVIDER";

/// Builds the Pi launch: the extension and starting model, then the operator's arguments.
pub(super) fn launch(setup: &LaunchSetup<'_>) -> anyhow::Result<AgentLaunch> {
    let folder = (setup.cache_root)()?.join("code").join("pi");
    let extension = content_named(&folder, "dotcore-provider", "ts", EXTENSION);
    let mut args = vec![
        OsString::from("-e"),
        extension.clone().into_os_string(),
        OsString::from("--model"),
        OsString::from(format!("dotcore/{}", setup.selected)),
    ];
    args.extend_from_slice(setup.passthrough);

    Ok(AgentLaunch {
        args,
        env: vec![(PROVIDER_ENV, provider(setup).to_string())],
        files: vec![OwnedFile {
            path: extension,
            content: EXTENSION.to_owned(),
        }],
    })
}

/// Returns the provider definition the extension expands: the base URL and the route names as
/// model ids.
fn provider(setup: &LaunchSetup<'_>) -> Value {
    json!({
        "baseUrl": setup.base_url,
        "models": setup.routes,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn the_launch_loads_the_cached_extension_on_the_selected_route_before_the_arguments() {
        let passthrough = [OsString::from("-p"), OsString::from("hello")];
        let setup = LaunchSetup {
            base_url: "http://127.0.0.1:50052/v1".to_owned(),
            routes: vec!["chat", "code"],
            selected: "code",
            selected_context: None,
            passthrough: &passthrough,
            cache_root: &|| Ok(PathBuf::from("/cache/dotcore")),
        };

        let launch = launch(&setup).expect("pi launch");

        let extension = content_named(
            Path::new("/cache/dotcore/code/pi"),
            "dotcore-provider",
            "ts",
            EXTENSION,
        );
        assert_eq!(
            launch.args,
            [
                OsString::from("-e"),
                extension.clone().into_os_string(),
                OsString::from("--model"),
                OsString::from("dotcore/code"),
                OsString::from("-p"),
                OsString::from("hello"),
            ]
        );
        assert_eq!(
            launch.files,
            [OwnedFile {
                path: extension,
                content: EXTENSION.to_owned(),
            }]
        );
        let [(variable, content)] = launch.env.as_slice() else {
            panic!("one variable expected: {:?}", launch.env);
        };
        assert_eq!(*variable, "DOTCORE_PI_PROVIDER");
        assert_eq!(
            serde_json::from_str::<Value>(content).expect("provider JSON"),
            json!({
                "baseUrl": "http://127.0.0.1:50052/v1",
                "models": ["chat", "code"],
            })
        );
    }
}
