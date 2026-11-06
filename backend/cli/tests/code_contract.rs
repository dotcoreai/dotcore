//! Process-level contracts for `dotc code`, run against a fake inference API with fake coding
//! agents on `PATH` that record how they were started.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use dotcore::testing::{ReservedPort, reserve_port};
use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

use support::{
    command::{detach_from_controlling_terminal, isolated_dotc},
    fake_inference::FakeInference,
    pty::{Session, Step, run_in_pty},
};

/// Exit code the fake agents end with, so the tests can see it propagated.
const AGENT_EXIT_CODE: i32 = 7;

/// A fake agent that writes each argument on its own line and the configuration variables of
/// OpenCode and Pi next to the record path, then exits with the requested code.
const FAKE_AGENT: &str = r#"#!/bin/sh
for argument in "$@"; do printf '%s\n' "$argument"; done > "$FAKE_AGENT_RECORD.args"
printf '%s' "$OPENCODE_CONFIG_CONTENT" > "$FAKE_AGENT_RECORD.config"
printf '%s' "$DOTCORE_PI_PROVIDER" > "$FAKE_AGENT_RECORD.provider"
exit "$FAKE_AGENT_EXIT"
"#;

/// The Pi extension `dotc` embeds.
const PI_EXTENSION: &str = include_str!("../src/code/dotcore-provider.ts");

const KEY_DOWN: &[u8] = b"\x1b[B";

const KEY_ENTER: &[u8] = b"\r";

#[test]
fn opencode_gets_every_enabled_route_and_the_selected_model_inline() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let output = sandbox.run(&inference, &["opencode", "--model", "chat"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(text(&output.stdout), "");
    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
    assert_eq!(sandbox.agent_args(), "");
    assert_eq!(
        sandbox.agent_config(),
        opencode_config(&inference.base_url(), "chat")
    );
    assert_eq!(inference.requests(), ["GET /v1/models HTTP/1.1"]);
    sandbox.assert_user_configuration_untouched();
}

#[test]
fn agent_arguments_after_the_separator_reach_opencode_unchanged() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let output = sandbox.run(
        &inference,
        &["opencode", "--model", "chat", "--", "run", "hello"],
    );

    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
    assert_eq!(sandbox.agent_args(), "run\nhello\n");
    sandbox.assert_user_configuration_untouched();
}

#[test]
fn an_existing_opencode_inline_configuration_is_kept_under_the_provider() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let mut command = sandbox.command(&inference, &["opencode", "--model", "chat"]);
    command.env(
        "OPENCODE_CONFIG_CONTENT",
        r#"{"theme":"dark","model":"other/m","provider":{"other":{"name":"Other"}}}"#,
    );
    let output = command.output().expect("run dotc code");

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
    let mut expected = opencode_config(&inference.base_url(), "chat");
    expected["theme"] = json!("dark");
    expected["provider"]["other"] = json!({ "name": "Other" });
    assert_eq!(sandbox.agent_config(), expected);
}

#[test]
fn an_existing_opencode_inline_configuration_that_is_not_json_fails() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let mut command = sandbox.command(&inference, &["opencode", "--model", "chat"]);
    command.env("OPENCODE_CONFIG_CONTENT", "theme: dark");
    let output = command.output().expect("run dotc code");

    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("the existing OpenCode inline configuration is not a JSON object: "),
        "{stderr}"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!sandbox.agent_ran());
}

#[test]
fn opencode_needs_no_home_or_cache_folder() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let mut command = sandbox.command(&inference, &["opencode", "--model", "chat"]);
    command.env_remove("HOME").env_remove("XDG_CACHE_HOME");
    let output = command.output().expect("run dotc code");

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
}

#[test]
fn pi_loads_the_cached_extension_with_every_enabled_route_on_the_selected_model() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("pi");

    let output = sandbox.run(&inference, &["pi", "--model", "chat", "--", "-p", "hello"]);

    assert_eq!(text(&output.stderr), "");
    assert_eq!(text(&output.stdout), "");
    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
    let extension = sandbox.pi_extension();
    let name = extension
        .file_name()
        .and_then(|name| name.to_str())
        .expect("extension name");
    let hash = name
        .strip_prefix("dotcore-provider-")
        .and_then(|rest| rest.strip_suffix(".ts"))
        .expect("content-named extension");
    assert!(
        hash.len() == 16 && hash.chars().all(|digit| digit.is_ascii_hexdigit()),
        "{name}"
    );
    assert_eq!(
        sandbox.agent_args(),
        format!(
            "-e\n{}\n--model\ndotcore/chat\n-p\nhello\n",
            extension.display()
        )
    );
    assert_eq!(
        fs::read_to_string(&extension).expect("Pi extension"),
        PI_EXTENSION
    );
    assert_eq!(
        sandbox.agent_provider(),
        json!({
            "baseUrl": inference.base_url(),
            "models": ["chat", "code"],
        })
    );
    sandbox.assert_user_configuration_untouched();
}

#[test]
fn the_inference_address_comes_from_the_flag_before_the_environment() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let mut command = sandbox.command(
        &inference,
        &[
            "opencode",
            "--model",
            "chat",
            "--inference-addr",
            &inference.origin(),
        ],
    );
    command.env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:1");
    let output = command.output().expect("run dotc code");

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
    assert_eq!(
        sandbox.agent_config()["provider"]["dotcore"]["options"]["baseURL"],
        Value::String(inference.base_url())
    );
}

#[test]
fn the_command_never_needs_a_manager() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");
    let (_reserved, unused) = unused_address();

    let mut command = sandbox.command(&inference, &["opencode", "--model", "chat"]);
    command.env("DOTCORE_MANAGEMENT_ADDR", unused.to_string());
    let output = command.output().expect("run dotc code");

    assert_eq!(text(&output.stderr), "");
    assert_eq!(output.status.code(), Some(AGENT_EXIT_CODE));
    assert_eq!(sandbox.agent_config()["model"], "dotcore/chat");
}

#[test]
fn an_unknown_route_fails_listing_the_ready_routes() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let output = sandbox.run(&inference, &["opencode", "--model", "disabled"]);

    assert_eq!(
        text(&output.stderr),
        "no ready inference route is named \"disabled\"; ready routes: chat, code\n"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!sandbox.agent_ran());
}

#[test]
fn a_site_without_a_ready_route_fails_saying_so() {
    let inference = FakeInference::with_routes(&[]);
    let sandbox = Sandbox::with_agent("opencode");

    let output = sandbox.run(&inference, &["opencode", "--model", "chat"]);

    assert_eq!(
        text(&output.stderr),
        "the site has no ready inference route; create one, or wait for its model to be ready\n"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!sandbox.agent_ran());
}

#[test]
fn choosing_a_route_without_a_terminal_asks_for_the_model_flag() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");

    let output = sandbox.run(&inference, &["opencode"]);

    assert_eq!(
        text(&output.stderr),
        "choosing an inference route needs a terminal; pass --model with one of: chat, code\n"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!sandbox.agent_ran());
}

#[test]
fn an_unreachable_inference_api_fails_naming_the_address() {
    let (_reserved, address) = unused_address();
    let sandbox = Sandbox::with_agent("opencode");

    let mut command = sandbox.bare_command(&["opencode", "--model", "chat"]);
    command.env("DOTCORE_INFERENCE_ADDR", address.to_string());
    let output = command.output().expect("run dotc code");

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "cannot reach the inference API at http://{address}; pass its address with \
             --inference-addr or DOTCORE_INFERENCE_ADDR"
        )),
        "{stderr}"
    );
    assert!(!sandbox.agent_ran());
}

#[test]
fn a_refused_route_listing_fails_with_the_status() {
    let inference = FakeInference::answering(503, "busy");
    let sandbox = Sandbox::with_agent("opencode");

    let output = sandbox.run(&inference, &["opencode", "--model", "chat"]);

    assert_eq!(
        text(&output.stderr),
        format!(
            "the inference API at {} answered with status 503\n",
            inference.origin()
        )
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(!sandbox.agent_ran());
}

#[test]
fn an_answer_that_is_not_a_route_list_fails() {
    for body in ["<html></html>", r#"{"data":[{"name":"chat"}]}"#] {
        let inference = FakeInference::answering(200, body);
        let sandbox = Sandbox::with_agent("opencode");

        let output = sandbox.run(&inference, &["opencode", "--model", "chat"]);

        let stderr = text(&output.stderr);
        assert!(
            stderr.starts_with(&format!(
                "the answer from {} is not an inference route list",
                inference.origin()
            )),
            "{stderr}"
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(!sandbox.agent_ran());
    }
}

#[test]
fn an_agent_missing_from_path_fails_with_an_install_hint() {
    let inference = listed_routes();
    let sandbox = Sandbox::without_agent();

    let output = sandbox.run(&inference, &["opencode", "--model", "chat"]);

    assert_eq!(
        text(&output.stderr),
        "OpenCode is not installed: no `opencode` executable on PATH; to install it: \
         https://opencode.ai\n"
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn an_agent_on_path_that_cannot_run_fails_with_the_system_error() {
    let inference = listed_routes();
    let sandbox = Sandbox::without_agent();
    sandbox.install_agent("opencode", "#!/nonexistent/interpreter\n");

    let output = sandbox.run(&inference, &["opencode", "--model", "chat"]);

    assert_eq!(
        text(&output.stderr),
        format!(
            "cannot start OpenCode: {}\n",
            std::io::Error::from_raw_os_error(libc::ENOENT)
        )
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn the_terminal_picker_launches_the_chosen_route_with_the_terminal_restored() {
    let inference = listed_routes();
    let sandbox = Sandbox::with_agent("opencode");
    let mut command = isolated_dotc();
    command
        .args(["code", "opencode"])
        .env("DOTCORE_INFERENCE_ADDR", inference.address().to_string())
        .env("TERM", "xterm-256color");
    sandbox.configure(&mut command);

    let Some(run) = run_in_pty(
        command,
        Session::new([
            Step::Wait("Route for OpenCode"),
            Step::Send(KEY_DOWN),
            Step::Send(KEY_ENTER),
        ]),
    ) else {
        return;
    };

    assert_eq!(
        run.code(),
        Some(AGENT_EXIT_CODE),
        "screen:\n{}",
        run.screen()
    );
    assert_eq!(run.stderr(), "");
    assert_eq!(sandbox.agent_config()["model"], "dotcore/code");
    run.assert_terminal_restored();
}

/// A fake inference API listing the `chat` and `code` routes.
fn listed_routes() -> FakeInference {
    FakeInference::with_routes(&["chat", "code"])
}

/// A loopback address nothing listens on, with the reservation that keeps other test processes
/// from binding it while the caller holds it.
fn unused_address() -> (ReservedPort, std::net::SocketAddr) {
    let reserved = reserve_port();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], reserved.port()));
    (reserved, address)
}

/// The inline OpenCode configuration for the listed routes.
fn opencode_config(base_url: &str, model: &str) -> Value {
    json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "dotcore": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Dotcore",
                "options": { "baseURL": base_url },
                "models": {
                    "chat": { "name": "chat" },
                    "code": { "name": "code" },
                },
            },
        },
        "model": format!("dotcore/{model}"),
    })
}

/// Empty home, configuration and `PATH` folders for one test, with an optional fake agent on
/// that `PATH`.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn with_agent(executable: &str) -> Self {
        let sandbox = Self::without_agent();
        sandbox.install_agent(executable, FAKE_AGENT);
        sandbox
    }

    fn install_agent(&self, executable: &str, script: &str) {
        let agent = self.bin().join(executable);
        fs::write(&agent, script).expect("write fake agent");
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))
            .expect("make agent runnable");
    }

    fn without_agent() -> Self {
        let root = support::canonical_tempdir().expect("sandbox");
        for folder in ["bin", "home", "config", "cache", "record"] {
            fs::create_dir(root.path().join(folder)).expect("sandbox folder");
        }
        Self { root }
    }

    /// Runs `dotc code <arguments>` against the fake without any terminal.
    fn run(&self, inference: &FakeInference, arguments: &[&str]) -> Output {
        self.command(inference, arguments)
            .output()
            .expect("run dotc code")
    }

    fn command(&self, inference: &FakeInference, arguments: &[&str]) -> Command {
        let mut command = self.bare_command(arguments);
        command.env("DOTCORE_INFERENCE_ADDR", inference.address().to_string());
        command
    }

    /// Builds `dotc code <arguments>` in this sandbox, with no inference API address and no
    /// terminal.
    fn bare_command(&self, arguments: &[&str]) -> Command {
        let mut command = isolated_dotc();
        command.arg("code").args(arguments);
        detach_from_controlling_terminal(&mut command);
        self.configure(&mut command);
        command
    }

    fn configure(&self, command: &mut Command) {
        command
            .env("PATH", self.bin())
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("FAKE_AGENT_RECORD", self.record())
            .env("FAKE_AGENT_EXIT", AGENT_EXIT_CODE.to_string());
    }

    fn agent_ran(&self) -> bool {
        self.record().with_extension("args").exists()
    }

    fn agent_args(&self) -> String {
        fs::read_to_string(self.record().with_extension("args")).expect("agent arguments")
    }

    fn agent_config(&self) -> Value {
        let content =
            fs::read_to_string(self.record().with_extension("config")).expect("agent config");
        serde_json::from_str(&content).expect("agent config JSON")
    }

    fn agent_provider(&self) -> Value {
        let content =
            fs::read_to_string(self.record().with_extension("provider")).expect("agent provider");
        serde_json::from_str(&content).expect("agent provider JSON")
    }

    /// The one Pi extension `dotc` keeps under this sandbox's cache root.
    fn pi_extension(&self) -> PathBuf {
        let files = entries(&self.root.path().join("cache/dotcore/code/pi"));
        let [extension] = files.as_slice() else {
            panic!("one Pi extension expected: {files:?}");
        };
        extension.clone()
    }

    fn assert_user_configuration_untouched(&self) {
        for folder in ["home", "config"] {
            assert_eq!(
                entries(&self.root.path().join(folder)),
                Vec::<PathBuf>::new(),
                "{folder}"
            );
        }
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    fn record(&self) -> PathBuf {
        self.root.path().join("record").join("agent")
    }
}

/// Every file and folder under `root`, recursively.
fn entries(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root).expect("read folder") {
        let path = entry.expect("folder entry").path();
        if path.is_dir() {
            found.extend(entries(&path));
        }
        found.push(path);
    }
    found
}

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}
