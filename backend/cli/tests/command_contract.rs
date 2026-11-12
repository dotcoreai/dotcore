//! Process-level contracts for the CLI command boundary.

use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixListener},
    },
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

mod support;

use dotcore_site::{
    membership::join::{JoinSecret, JoinToken},
    node::folder::{CertificateFingerprint, WorkerNodeCustody},
    reset::transaction::SiteResetLock,
};

use support::{
    command::{detach_from_controlling_terminal, dotc, isolated_dotc},
    pty::{Session, Stdin, Step, run_in_pty},
};

#[test]
fn version_command_runs_successfully() {
    let version = dotc().arg("--version").output().expect("run dotc version");
    assert!(version.status.success());
    assert_eq!(
        text(&version.stdout).trim(),
        concat!("dotc ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn config_show_reports_explicit_xdg_and_home_paths_without_mutation() {
    let fixture = TemporarySite::empty();
    let cases = [
        (
            vec![
                ("DOTCORE_SITE_DIR", fixture.root.join("explicit-site")),
                ("DOTCORE_CATALOG_DIR", fixture.root.join("catalog")),
                ("DOTCORE_CACHE_DIR", fixture.root.join("explicit-cache")),
                ("HOME", fixture.root.join("ignored-home")),
            ],
            serde_json::json!({
                "role": "manager",
                "siteDir": fixture.root.join("explicit-site"),
                "masterCatalogDir": fixture.root.join("catalog"),
                "cacheDir": fixture.root.join("explicit-cache"),
                "modelArtifactCacheDir": fixture.root.join("explicit-cache/model-artifacts/v1"),
                "workloadCacheDir": fixture.root.join("explicit-cache/workload-cache/v2"),
                "nodeHost": serde_json::Value::Null,
            }),
        ),
        (
            vec![
                ("XDG_STATE_HOME", fixture.root.join("xdg-state")),
                ("XDG_CACHE_HOME", fixture.root.join("xdg-cache")),
                ("HOME", fixture.root.join("ignored-home")),
            ],
            serde_json::json!({
                "role": "manager",
                "siteDir": fixture.root.join("xdg-state/dotcore/site"),
                "masterCatalogDir": serde_json::Value::Null,
                "cacheDir": fixture.root.join("xdg-cache/dotcore"),
                "modelArtifactCacheDir": fixture.root.join("xdg-cache/dotcore/model-artifacts/v1"),
                "workloadCacheDir": fixture.root.join("xdg-cache/dotcore/workload-cache/v2"),
                "nodeHost": serde_json::Value::Null,
            }),
        ),
        (
            vec![("HOME", fixture.root.join("home"))],
            serde_json::json!({
                "role": "manager",
                "siteDir": fixture.root.join("home/.local/state/dotcore/site"),
                "masterCatalogDir": serde_json::Value::Null,
                "cacheDir": fixture.root.join("home/.cache/dotcore"),
                "modelArtifactCacheDir": fixture.root.join("home/.cache/dotcore/model-artifacts/v1"),
                "workloadCacheDir": fixture.root.join("home/.cache/dotcore/workload-cache/v2"),
                "nodeHost": serde_json::Value::Null,
            }),
        ),
    ];

    for (environment, expected) in cases {
        let mut command = dotc();
        command
            .args(["config", "show", "--format", "json"])
            .clear_test_environment();
        for (name, value) in environment {
            command.env(name, value);
        }
        let output = command.output().expect("run config show");
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert!(output.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .expect("configuration JSON"),
            expected
        );
    }

    assert_eq!(
        std::fs::read_dir(&fixture.root)
            .expect("fixture root")
            .count(),
        0,
        "configuration inspection must not create resolved paths"
    );
}

#[test]
fn config_show_reports_the_node_role_from_the_folder_contents() {
    let fixture = TemporarySite::empty();
    let cases = [
        ("empty", &[][..], "manager"),
        ("worker-only", &["worker.yaml"][..], "joined-node"),
        ("site-only", &["site.yaml"][..], "manager"),
        ("both", &["worker.yaml", "site.yaml"][..], "manager"),
    ];

    for (name, files, expected) in cases {
        let site = fixture.root.join(name);
        std::fs::create_dir_all(&site).expect("create node folder");
        for file in files {
            std::fs::write(site.join(file), "").expect("create folder file");
        }
        let output = dotc()
            .args(["config", "show", "--format", "json"])
            .clear_test_environment()
            .env("DOTCORE_SITE_DIR", &site)
            .env("HOME", fixture.root.join("home"))
            .output()
            .expect("run config show");
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let json = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .expect("configuration JSON");
        assert_eq!(json["role"], expected, "role of the {name} folder");
        assert_eq!(
            json["nodeHost"],
            serde_json::Value::Null,
            "node host of the {name} folder"
        );
    }
}

#[test]
fn config_show_reports_the_node_host_a_manager_site_records() {
    let fixture = TemporarySite::new();

    let output = dotc()
        .args(["config", "show", "--format", "json", "--site-dir"])
        .arg(fixture.site_path())
        .clear_test_environment()
        .env("HOME", fixture.root.join("home"))
        .output()
        .expect("run config show");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let json =
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("configuration JSON");
    assert_eq!(json["role"], "manager");
    assert_eq!(json["nodeHost"], "127.0.0.2");
}

#[test]
fn config_show_rejects_invalid_xdg_inputs_with_actionable_diagnostics() {
    let output = dotc()
        .args(["config", "show", "--format", "json"])
        .clear_test_environment()
        .env("XDG_STATE_HOME", "relative-state")
        .env("XDG_CACHE_HOME", "/cache")
        .env("HOME", "/home/user")
        .output()
        .expect("run invalid config show");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        "Invalid site path: XDG_STATE_HOME must be an absolute path, got \"relative-state\"\n"
    );
}

#[test]
fn config_show_applies_the_root_paths_before_or_after_the_command_and_over_the_environment() {
    let fixture = TemporarySite::empty();
    let root = &fixture.root;
    let expected = serde_json::json!({
        "role": "manager",
        "siteDir": root.join("s"),
        "masterCatalogDir": root.join("c"),
        "cacheDir": root.join("k"),
        "modelArtifactCacheDir": root.join("k/model-artifacts/v1"),
        "workloadCacheDir": root.join("k/workload-cache/v2"),
        "nodeHost": serde_json::Value::Null,
    });
    let flags = [
        ("--site-dir", root.join("s")),
        ("--catalog-dir", root.join("c")),
        ("--cache-dir", root.join("k")),
    ];
    let run = |before: bool, with_environment: bool| {
        let mut command = dotc();
        command
            .clear_test_environment()
            .env("HOME", root.join("home"));
        if with_environment {
            command
                .env("DOTCORE_SITE_DIR", root.join("env-s"))
                .env("DOTCORE_CATALOG_DIR", root.join("env-c"))
                .env("DOTCORE_CACHE_DIR", root.join("env-k"));
        }
        if before {
            for (flag, value) in &flags {
                command.arg(flag).arg(value);
            }
        }
        command.args(["config", "show", "--format", "json"]);
        if !before {
            for (flag, value) in &flags {
                command.arg(format!("{flag}={}", value.display()));
            }
        }
        let output = command.output().expect("run config show");
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert!(output.stderr.is_empty());
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("configuration JSON")
    };

    assert_eq!(run(true, false), expected);
    assert_eq!(run(false, false), expected);
    assert_eq!(
        run(true, true),
        expected,
        "flags before the command beat the environment"
    );
    assert_eq!(
        run(false, true),
        expected,
        "flags after the command beat the environment"
    );
}

#[test]
fn root_path_flags_and_variables_with_unusable_values_fail_with_the_absolute_path_error() {
    let cases: [(&str, &str, &str, &str); 6] = [
        (
            "--site-dir",
            "relative",
            "DOTCORE_SITE_DIR",
            "DOTCORE_SITE_DIR must be an absolute path, got \"relative\"",
        ),
        (
            "--catalog-dir",
            "relative",
            "DOTCORE_CATALOG_DIR",
            "DOTCORE_CATALOG_DIR must be an absolute path, got \"relative\"",
        ),
        (
            "--cache-dir",
            "relative",
            "DOTCORE_CACHE_DIR",
            "DOTCORE_CACHE_DIR must be an absolute path, got \"relative\"",
        ),
        (
            "--site-dir",
            "",
            "DOTCORE_SITE_DIR",
            "DOTCORE_SITE_DIR must be an absolute path, got \"\"",
        ),
        (
            "--catalog-dir",
            "",
            "DOTCORE_CATALOG_DIR",
            "DOTCORE_CATALOG_DIR must be an absolute path, got \"\"",
        ),
        (
            "--cache-dir",
            "/",
            "DOTCORE_CACHE_DIR",
            "DOTCORE_CACHE_DIR cannot be the filesystem root",
        ),
    ];

    for (flag, value, variable, message) in cases {
        let expected = format!("Invalid site path: {message}\n");
        let by_flag = dotc()
            .args(["config", "show", "--format", "json", flag, value])
            .clear_test_environment()
            .env("HOME", "/home/user")
            .output()
            .expect("run config show with a flag");
        assert_eq!(by_flag.status.code(), Some(1), "{flag} {value:?}");
        assert!(by_flag.stdout.is_empty());
        assert_eq!(text(&by_flag.stderr), expected, "{flag} {value:?}");

        let by_variable = dotc()
            .args(["config", "show", "--format", "json"])
            .clear_test_environment()
            .env("HOME", "/home/user")
            .env(variable, value)
            .output()
            .expect("run config show with a variable");
        assert_eq!(by_variable.status.code(), Some(1), "{variable}={value:?}");
        assert_eq!(text(&by_variable.stderr), expected, "{variable}={value:?}");
    }
}

#[test]
fn an_unusable_path_variable_does_not_break_commands_that_do_not_use_it() {
    for (variable, value) in [
        ("DOTCORE_SITE_DIR", "relative"),
        ("DOTCORE_CATALOG_DIR", ""),
        ("DOTCORE_CACHE_DIR", "relative"),
    ] {
        let help = dotc()
            .args(["model", "list", "--help"])
            .clear_test_environment()
            .env(variable, value)
            .output()
            .expect("model list help");
        assert_eq!(help.status.code(), Some(0), "{variable}={value:?}");

        // The command parses and reaches the manager connection, which refuses.
        let closed = dotcore::testing::reserve_port();
        let address = format!("127.0.0.1:{}", closed.port());
        let mut list = support::command::dotc_at("model", &address, &["list"]);
        let output = list.env(variable, value).output().expect("model list");
        assert_eq!(output.status.code(), Some(1), "{variable}={value:?}");
        assert!(
            text(&output.stderr)
                .starts_with(&format!("cannot reach the manager at http://{address}")),
            "{}",
            text(&output.stderr)
        );
    }
}

#[test]
fn bare_command_groups_still_show_help_when_a_path_variable_is_set() {
    for (arguments, usage) in [
        (&[][..], "Usage: dotc [OPTIONS] <COMMAND>"),
        (&["node"][..], "Usage: dotc node [OPTIONS] [COMMAND]"),
        (&["server"][..], "Usage: dotc server [OPTIONS] [COMMAND]"),
    ] {
        let output = dotc()
            .args(arguments)
            .clear_test_environment()
            .env("DOTCORE_SITE_DIR", "/srv/site")
            .env("DOTCORE_CATALOG_DIR", "/srv/catalog")
            .output()
            .expect("bare command");
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        let help = text(&output.stderr);
        assert!(help.contains(usage), "{help}");
        assert!(help.contains("[env: DOTCORE_SITE_DIR]"), "{help}");
    }
}

#[test]
fn a_bare_command_group_with_only_root_path_options_shows_help() {
    for arguments in [
        &["site", "--site-dir", "/x"][..],
        &["--catalog-dir=/c", "node"][..],
    ] {
        let output = dotc()
            .args(arguments)
            .clear_test_environment()
            .output()
            .expect("bare group with a path option");
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(
            text(&output.stderr).contains("Usage: dotc"),
            "{}",
            text(&output.stderr)
        );
    }
}

#[test]
fn site_delete_and_reset_refuse_an_invalid_explicit_cache_dir_before_changing_anything() {
    let fixture = TemporarySite::empty();
    let site = fixture.root.join("site");
    std::fs::create_dir_all(&site).expect("site directory");
    std::fs::write(
        site.join("site.yaml"),
        "siteId: kept-site\nmodelDeployments: []\n",
    )
    .expect("site");

    let reset_flags = [
        "--inference",
        "simulated",
        "--applications",
        "simulated",
        "--node-id",
        "kept-node",
        "--host",
        "127.0.0.1",
    ];
    for command in [
        vec!["site", "delete", "--yes"],
        ["site", "reset", "--yes"]
            .into_iter()
            .chain(reset_flags)
            .collect(),
    ] {
        let output = dotc()
            .args(&command)
            .arg("--non-interactive")
            .arg("--site-dir")
            .arg(&site)
            .args(["--cache-dir", "relative/cache"])
            .clear_test_environment()
            .output()
            .expect("run with invalid cache dir");

        assert_eq!(output.status.code(), Some(1), "{command:?}");
        assert_eq!(
            text(&output.stderr),
            "Invalid site path: DOTCORE_CACHE_DIR must be an absolute path, got \"relative/cache\"\n",
            "{command:?}"
        );
        assert!(site.join("site.yaml").is_file(), "{command:?}");
    }
}

#[test]
fn site_delete_without_a_site_dir_targets_the_default_site_directory() {
    let fixture = TemporarySite::empty();
    let state = fixture.root.join("state");
    let default_site = state.join("dotcore/site");
    std::fs::create_dir_all(&default_site).expect("default site directory");
    std::fs::write(
        default_site.join("site.yaml"),
        "siteId: default-site\nmodelDeployments: []\n",
    )
    .expect("default site");

    let output = dotc()
        .args(["site", "delete", "--yes", "--non-interactive"])
        .clear_test_environment()
        .env("XDG_STATE_HOME", &state)
        .output()
        .expect("run site delete without a site directory");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(!text(&output.stderr).contains("required arguments were not provided"));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Site deleted\nDOTCORE_SITE_DIR={}\n",
            default_site.display()
        )
    );
    assert!(!default_site.exists());
}

#[test]
fn site_delete_takes_the_site_dir_from_the_variable_and_the_root_flag() {
    let fixture = TemporarySite::empty();
    for (name, by_flag_before_command) in [("variable", None), ("flag", Some(true))] {
        let site = fixture.root.join(name);
        std::fs::create_dir_all(&site).expect("Site directory");
        std::fs::write(
            site.join("site.yaml"),
            "siteId: other-site\nmodelDeployments: []\n",
        )
        .expect("Site");
        let mut command = dotc();
        command
            .clear_test_environment()
            .env("HOME", fixture.root.join("home"));
        match by_flag_before_command {
            None => {
                command.env("DOTCORE_SITE_DIR", &site);
            }
            Some(_) => {
                command.arg("--site-dir").arg(&site);
            }
        }
        let output = command
            .args(["site", "delete", "--yes", "--non-interactive"])
            .output()
            .expect("run site delete");

        assert_eq!(
            output.status.code(),
            Some(0),
            "{name}: {}",
            text(&output.stderr)
        );
        assert!(!site.exists(), "{name}");
    }
}

#[test]
fn workload_parse_failures_keep_the_json_protocol_with_root_options_before_the_action() {
    for arguments in [
        &[
            "--catalog-dir",
            "/c",
            "dev",
            "workload",
            "launch",
            "--unknown-flag",
        ][..],
        &[
            "--catalog-dir=/c",
            "--verbose",
            "dev",
            "workload",
            "launch",
            "--unknown-flag",
        ][..],
        &[
            "dev",
            "workload",
            "--cache-dir",
            "/k",
            "launch",
            "--unknown-flag",
        ][..],
    ] {
        let output = dotc()
            .args(arguments)
            .clear_test_environment()
            .output()
            .expect("invalid workload arguments");

        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("JSON error record");
        assert_eq!(response["operation"], "launch", "{arguments:?}");
        assert_eq!(response["result"], "error");
        assert_eq!(response["error"]["code"], "invalid_arguments");
    }
}

#[test]
fn dev_commands_without_a_master_catalog_name_the_option_and_variable() {
    let missing = "no master catalog: pass --catalog-dir or set DOTCORE_CATALOG_DIR to its folder";

    for operation in ["inspect", "logs"] {
        for (catalog, message) in [
            (None, missing.to_owned()),
            (
                Some("relative"),
                "Invalid site path: DOTCORE_CATALOG_DIR must be an absolute path, got \"relative\""
                    .to_owned(),
            ),
        ] {
            let mut command = dotc();
            command
                .args([
                    "dev",
                    "workload",
                    operation,
                    "--handle",
                    "/tmp/no-workload-handle",
                ])
                .clear_test_environment();
            if let Some(catalog) = catalog {
                command.env("DOTCORE_CATALOG_DIR", catalog);
            }
            let workload = command.output().expect("workload action");
            let label = format!("{operation} {catalog:?}");
            assert_eq!(workload.status.code(), Some(2), "{label}");
            let response: serde_json::Value =
                serde_json::from_slice(&workload.stdout).expect("JSON error record");
            assert_eq!(response["operation"], operation, "{label}");
            assert_eq!(response["result"], "error", "{label}");
            assert_eq!(response["error"]["code"], "invalid_arguments", "{label}");
            assert_eq!(response["error"]["message"], message, "{label}");
            assert_eq!(text(&workload.stderr), format!("{message}\n"), "{label}");
        }
    }

    let telemetry = dotc()
        .args([
            "dev",
            "telemetry",
            "fit",
            "--telemetry-dir",
            "/tmp/no-telemetry",
            "--model-id",
            "model",
            "--configuration-id",
            "configuration",
            "--hardware-class-id",
            "class",
            "--binding-id",
            "binding",
        ])
        .clear_test_environment()
        .output()
        .expect("telemetry fit");
    assert_eq!(telemetry.status.code(), Some(1));
    assert!(telemetry.stdout.is_empty());
    assert_eq!(text(&telemetry.stderr), format!("{missing}\n"));
}

#[test]
fn cleared_test_environment_forwards_the_ca_bundle_and_selected_docker_host() {
    let mut command = Command::new("dotc");
    command.env("DOTCORE_TEST_AMBIENT", "must-not-survive");
    command
        .clear_test_environment()
        .forward_test_docker_host("unix:///tmp/dotcore-test-docker.sock");

    let environment = command.get_envs().collect::<Vec<_>>();
    assert!(
        !environment
            .iter()
            .any(|(name, _)| *name == "DOTCORE_TEST_AMBIENT")
    );
    assert_eq!(
        environment
            .iter()
            .find(|(name, _)| *name == "SSL_CERT_FILE")
            .and_then(|(_, value)| value.map(std::ffi::OsStr::to_os_string)),
        std::env::var_os("SSL_CERT_FILE")
    );
    assert_eq!(
        environment
            .iter()
            .find(|(name, _)| *name == "DOCKER_HOST")
            .and_then(|(_, value)| value.map(std::ffi::OsStr::to_os_string)),
        Some("unix:///tmp/dotcore-test-docker.sock".into())
    );
}

#[test]
fn workload_help_and_operational_failures_use_the_json_process_contract() {
    let help = dotc()
        .args(["dev", "workload", "--help"])
        .output()
        .expect("workload help");
    assert_eq!(help.status.code(), Some(0));
    assert!(help.stderr.is_empty());
    let help_text = text(&help.stdout);
    for action in ["launch", "inspect", "logs", "vram", "teardown"] {
        assert!(help_text.contains(action), "missing {action} in help");
    }
    let vram_help = dotc()
        .args(["dev", "workload", "vram", "--help"])
        .output()
        .expect("workload VRAM help");
    assert_eq!(vram_help.status.code(), Some(0));
    assert!(vram_help.stderr.is_empty());
    let vram_help = text(&vram_help.stdout);
    for expected in [
        "--handle <HANDLE>",
        "--duration-ms <DURATION_MS>",
        "[default: 10000]",
        "--interval-ms <INTERVAL_MS>",
        "[default: 25]",
    ] {
        assert!(vram_help.contains(expected), "missing {expected} in help");
    }

    let output = dotc()
        .args([
            "dev",
            "workload",
            "inspect",
            "--catalog-dir",
            "/tmp/no-master-catalog",
            "--development-catalog",
            "/tmp/no-development-catalog",
            "--cache-dir",
            "/tmp/no-cache",
            "--handle",
            "/tmp/no-workload-handle",
        ])
        .output()
        .expect("workload inspect failure");
    assert_eq!(output.status.code(), Some(1));
    assert!(!output.stderr.is_empty());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON error");
    assert_eq!(response["format_version"], 2);
    assert_eq!(response["operation"], "inspect");
    assert_eq!(response["result"], "error");
    assert_eq!(response["error"]["code"], "operation_failed");

    let output = dotc()
        .args([
            "dev",
            "workload",
            "vram",
            "--catalog-dir",
            "/tmp/no-master-catalog",
            "--cache-dir",
            "/tmp/no-cache",
            "--handle",
            "/tmp/no-workload-handle",
        ])
        .output()
        .expect("workload VRAM failure");
    assert_eq!(output.status.code(), Some(1));
    assert!(!output.stderr.is_empty());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON error");
    assert_eq!(response["format_version"], 2);
    assert_eq!(response["operation"], "vram");
    assert_eq!(response["result"], "error");
    assert_eq!(response["error"]["code"], "operation_failed");
}

#[test]
fn workload_rejects_unsafe_paths_before_any_operation() {
    let output = dotc()
        .args([
            "dev",
            "workload",
            "launch",
            "--handle",
            "/tmp/../handle.json",
        ])
        .output()
        .expect("unsafe workload path");
    assert_eq!(output.status.code(), Some(2));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON error");
    assert_eq!(response["operation"], "launch");
    assert_eq!(response["error"]["code"], "invalid_arguments");
    assert!(text(&output.stderr).contains("path must be absolute, normalized"));
}

#[test]
fn workload_log_runtime_failures_do_not_write_json_to_log_stdout() {
    let output = dotc()
        .args([
            "dev",
            "workload",
            "logs",
            "--catalog-dir",
            "/tmp/no-master-catalog",
            "--cache-dir",
            "/tmp/no-cache",
            "--handle",
            "/tmp/no-workload-handle",
        ])
        .output()
        .expect("workload log failure");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[test]
fn workload_log_bounds_are_json_usage_errors() {
    let output = dotc()
        .args(["dev", "workload", "logs", "--maximum-entries", "201"])
        .output()
        .expect("invalid log bound");
    assert_eq!(output.status.code(), Some(2));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON error");
    assert_eq!(response["operation"], "logs");
    assert_eq!(response["error"]["code"], "invalid_arguments");
    assert!(text(&output.stderr).contains("201"));
}

#[test]
fn workload_vram_bounds_are_json_usage_errors() {
    for (argument, value) in [
        ("--duration-ms", "0"),
        ("--duration-ms", "3600001"),
        ("--interval-ms", "19"),
        ("--interval-ms", "51"),
    ] {
        let output = dotc()
            .args(["dev", "workload", "vram", argument, value])
            .output()
            .expect("invalid VRAM observation bound");

        assert_eq!(output.status.code(), Some(2));
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("JSON error");
        assert_eq!(response["operation"], "vram");
        assert_eq!(response["error"]["code"], "invalid_arguments");
        assert!(text(&output.stderr).contains(value));
    }
}

#[test]
fn workload_vram_health_requires_http_ok_status() {
    assert!(healthy_llama_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}"
    ));
    assert!(!healthy_llama_response(
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 20\r\n\r\n{\"status\":\"loading\"}"
    ));
    assert!(!healthy_llama_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot-json"
    ));
}

#[test]
#[ignore = "hardware: just backend::test-nvidia"]
fn hw_nvidia_workload_vram_attributes_the_owned_process_to_one_device() {
    let catalog = std::env::var("DOTCORE_CATALOG_DIR").expect("DOTCORE_CATALOG_DIR");
    let cache_dir = std::env::var("DOTCORE_CACHE_DIR").expect("DOTCORE_CACHE_DIR");
    let scope = format!("vram-test-{}", std::process::id());
    let handle = crate::support::canonical_temp_dir().join(format!("dotcore-{scope}.json"));
    let telemetry = crate::support::canonical_tempdir().expect("telemetry directory");
    let handle_text = handle.to_str().expect("UTF-8 test handle");
    let command = |action: &str| {
        let mut command = dotc();
        command.args([
            "dev",
            "workload",
            action,
            "--catalog-dir",
            &catalog,
            "--cache-dir",
            &cache_dir,
            "--runtime-scope",
            &scope,
            "--handle",
            handle_text,
        ]);
        command
    };

    let mut launch_command = command("launch");
    launch_command.args(["--model-id", "gemma-4-e2b", "--profile-id", "fast-start"]);
    let launch = output_with_timeout(launch_command, Duration::from_secs(600));
    let measurement = match &launch {
        Ok(launch) if launch.status.success() => {
            wait_for_workload_health(launch, Duration::from_secs(300)).and_then(|()| {
                let mut vram_command = command("vram");
                vram_command
                    .args(["--duration-ms", "250", "--interval-ms", "25"])
                    .env("DOTCORE_TELEMETRY_DIR", telemetry.path());
                output_with_timeout(vram_command, Duration::from_secs(30))
            })
        }
        Ok(launch) => Err(format!(
            "launch failed: stdout={} stderr={}",
            text(&launch.stdout),
            text(&launch.stderr)
        )),
        Err(error) => Err(error.clone()),
    };
    let teardown = output_with_timeout(command("teardown"), Duration::from_secs(60));
    let removal_proven = teardown.as_ref().ok().is_some_and(|output| {
        output.status.success()
            && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .is_ok_and(|response| response["payload"]["state"] == "removed")
    });
    if removal_proven {
        let _ = std::fs::remove_file(&handle);
    }

    let teardown = teardown.expect("bounded teardown process");
    assert!(
        teardown.status.success(),
        "teardown failed; ownership handle retained at {}: stdout={} stderr={}",
        handle.display(),
        text(&teardown.stdout),
        text(&teardown.stderr)
    );
    let teardown_response: serde_json::Value =
        serde_json::from_slice(&teardown.stdout).expect("teardown JSON");
    assert_eq!(teardown_response["payload"]["state"], "removed");
    let launch = launch.expect("bounded launch process");
    assert!(
        launch.status.success(),
        "launch failed: stdout={} stderr={}",
        text(&launch.stdout),
        text(&launch.stderr)
    );
    let launch_response: serde_json::Value =
        serde_json::from_slice(&launch.stdout).expect("launch JSON");
    let measurement = measurement.expect("successful launch before measurement");
    assert!(
        measurement.status.success(),
        "VRAM observation failed: stdout={} stderr={}",
        text(&measurement.stdout),
        text(&measurement.stderr)
    );
    let observation: serde_json::Value =
        serde_json::from_slice(&measurement.stdout).expect("VRAM JSON");
    let launch_payload = &launch_response["payload"];
    let payload = &observation["payload"];
    assert_eq!(observation["format_version"], 2);
    assert_eq!(observation["operation"], "vram");
    assert_eq!(observation["result"], "success");
    assert_eq!(payload["workload_id"], launch_payload["workload_id"]);
    assert_eq!(payload["container_id"], launch_payload["container_id"]);
    assert_eq!(payload["process_id"], launch_payload["process_id"]);
    // The gated device runs nothing else, so no memory belongs to another process.
    assert_eq!(payload["isolated"], true);
    assert_eq!(payload["other_processes"], serde_json::json!([]));
    assert_eq!(payload["unattributed_max_bytes"], 0);
    assert!(payload["process_start_token"].as_u64().unwrap() > 0);
    assert!(payload["gpu"]["uuid"].as_str().unwrap().starts_with("GPU-"));
    assert!(!payload["gpu"]["name"].as_str().unwrap().is_empty());
    let samples = payload["samples"].as_array().expect("VRAM samples");
    let observed_max = samples
        .iter()
        .map(|sample| sample["used_bytes"].as_u64().expect("sample bytes"))
        .max()
        .expect("at least one VRAM sample");
    assert_eq!(payload["observed_max_bytes"], observed_max);
    let memory = &payload["gpu"]["memory"];
    let capacity = match memory["architecture"]
        .as_str()
        .expect("GPU memory architecture")
    {
        "dedicated" => memory["total_bytes"].as_u64(),
        "unified" => memory["host_total_bytes"].as_u64(),
        architecture => panic!("unexpected GPU memory architecture {architecture}"),
    };
    assert!(capacity.expect("GPU memory capacity") > observed_max);

    assert_eq!(payload["telemetry"]["status"], "recorded");
    assert_eq!(payload["telemetry"]["engine_facts"], "unsupported");
    let record_path = payload["telemetry"]["path"].as_str().expect("record path");
    assert!(record_path.starts_with(telemetry.path().to_str().expect("UTF-8 telemetry path")));
    let records = std::fs::read_to_string(record_path).expect("telemetry records");
    let lines = records.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    let record: serde_json::Value = serde_json::from_str(lines[0]).expect("record JSON");
    assert_eq!(record["recordId"], payload["telemetry"]["record_id"]);
    assert_eq!(record["source"], "WORKLOAD_RESOURCE_SOURCE_DEVELOPMENT");
    assert_eq!(record["inputs"]["modelId"], "gemma-4-e2b");
    assert_eq!(record["inputs"]["profileId"], "fast-start");
    assert!(
        !launch_payload["engine_parameter_values"]["values"]
            .as_array()
            .expect("launch parameter values")
            .is_empty()
    );
    assert_eq!(
        record["inputs"]["engineParameterValues"],
        launch_payload["engine_parameter_values"]
    );
    assert_eq!(
        record["observation"]["attributedMemoryBytes"].as_str(),
        Some(observed_max.to_string().as_str())
    );
}

#[test]
fn site_reset_replaces_broken_retained_state_offline_and_repeats_exactly() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("reset-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(
        site_dir.join("site.yaml"),
        "siteId: retained-site\nmodelDeployments:\n- broken: true\n",
    )
    .expect("broken retained declaration");
    std::fs::write(site_dir.join("node.yaml"), "broken retained node")
        .expect("broken retained node");
    std::fs::write(site_dir.join("state.pb"), b"broken current state")
        .expect("broken retained state");
    std::fs::create_dir(site_dir.join("node")).expect("retained node store");
    std::fs::write(site_dir.join("node/state.pb"), b"broken node state")
        .expect("broken retained node state");
    std::fs::write(site_dir.join("unknown"), b"unknown retained bytes")
        .expect("unknown retained state");

    let expected = format!(
        "Site reset\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=reset-site\nDOTCORE_NODE_ID=reset-node\nDOTCORE_NODE_HOST=127.0.0.1\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
        site_dir.display()
    );
    let docker = FakeDockerDaemon::start();
    for _ in 0..2 {
        let output = reset_command(&site_dir)
            .forward_test_docker_host(&docker.host())
            .output()
            .expect("run site reset");

        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
        assert_eq!(text(&output.stdout), expected);
        assert_generated_simulated_bundle(&site_dir, "reset-site", "reset-node");
        assert!(!site_dir.join("state.pb").exists());
        assert!(!site_dir.join("node").exists());
        assert!(!site_dir.join("unknown").exists());
        let validation = dotc()
            .args(["site", "validate", "--site-dir"])
            .arg(&site_dir)
            .clear_test_environment()
            .output()
            .expect("validate freshly reset site");
        assert_eq!(validation.status.code(), Some(0));
        assert!(validation.stderr.is_empty());
        assert_eq!(
            text(&validation.stdout),
            format!("Site valid\nDOTCORE_SITE_DIR={}\n", site_dir.display())
        );
    }
    docker.assert_empty_site_reset_requests(&["retained-site", "reset-site"]);
}

#[test]
fn site_reset_replaces_obsolete_manager_identity_and_invalidates_worker_trust() {
    let fixture = TemporarySite::new();
    let site_dir = fixture.site_path();
    let node_fixture = TemporarySite::empty();
    let node_dir = node_fixture.root.join("joined-node");
    join_node(site_dir, &node_dir, "Worker two", "127.0.0.3");
    let trust = site_dir.join("trust");
    let old_anchor = std::fs::read(trust.join("site-ca.der")).expect("old site anchor");
    let old_leaf = std::fs::read(trust.join("node.der")).expect("old manager node leaf");
    assert_eq!(pin_files(site_dir).len(), 1);
    std::fs::rename(trust.join("node.der"), trust.join("manager.der"))
        .expect("install old manager certificate layout");
    std::fs::rename(trust.join("node-key.pk8"), trust.join("manager-key.pk8"))
        .expect("install old manager key layout");

    let docker = FakeDockerDaemon::start();
    let reset = reset_command(site_dir)
        .forward_test_docker_host(&docker.host())
        .output()
        .expect("reset obsolete site trust offline");
    assert_eq!(reset.status.code(), Some(0), "{}", text(&reset.stderr));
    assert_generated_simulated_bundle(site_dir, "reset-site", "reset-node");
    assert_ne!(
        std::fs::read(trust.join("site-ca.der")).unwrap(),
        old_anchor
    );
    assert_ne!(std::fs::read(trust.join("node.der")).unwrap(), old_leaf);
    assert!(!trust.join("manager.der").exists());
    assert!(!trust.join("manager-key.pk8").exists());
    assert!(pin_files(site_dir).is_empty());
    assert_eq!(
        std::fs::read(node_dir.join("trust/site-ca.der")).unwrap(),
        old_anchor,
        "joined node folder remains stale until the node joins again"
    );
    docker.assert_empty_site_reset_requests(&["cli-test-site"]);
}

#[test]
fn site_reset_succeeds_for_a_deployment_free_site_without_management_configuration() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("deployment-free-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(
        site_dir.join("site.yaml"),
        "siteId: retained-site\nmodelDeployments: []\napplicationDeployments: []\ninferenceRoutes: []\n",
    )
    .expect("deployment-free retained site");
    std::fs::write(site_dir.join("node.yaml"), "retained invalid node").expect("retained node");

    let docker = FakeDockerDaemon::start();
    let output = reset_command(&site_dir)
        .env_remove("DOTCORE_MANAGEMENT_ORIGIN")
        .forward_test_docker_host(&docker.host())
        .output()
        .expect("run deployment-free site reset");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
    assert_generated_simulated_bundle(&site_dir, "reset-site", "reset-node");
    docker.assert_empty_site_reset_requests(&["retained-site"]);
}

#[test]
fn site_reset_of_a_docker_bound_site_fails_closed_when_docker_is_unreachable() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("docker-bound-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(
        site_dir.join("site.yaml"),
        format!("siteId: retained-site\nexecutionTargets:\n{SIMULATED_MODEL_TARGET}{DOCKER_APPLICATION_TARGET}"),
    )
    .expect("Docker-bound retained site");
    std::fs::write(site_dir.join("state.pb"), b"retained state").expect("retained state");
    let before_site = std::fs::read(site_dir.join("site.yaml")).expect("Site bytes");
    let before_state = std::fs::read(site_dir.join("state.pb")).expect("state bytes");

    let output = reset_command(&site_dir)
        .forward_test_docker_host(ABSENT_DOCKER_HOST)
        .output()
        .expect("run Docker-bound site reset");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("site runtime cleanup failed: reach Docker runtime for site cleanup"),
        "{stderr}"
    );
    assert!(!stderr.contains("Notice:"), "{stderr}");
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site bytes after failure"),
        before_site
    );
    assert_eq!(
        std::fs::read(site_dir.join("state.pb")).expect("state bytes after failure"),
        before_state
    );
}

#[test]
fn site_reset_of_a_simulated_site_skips_unreachable_docker_with_a_notice() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("simulated-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(
        site_dir.join("site.yaml"),
        format!("siteId: retained-site\nexecutionTargets:\n{SIMULATED_MODEL_TARGET}"),
    )
    .expect("Simulated retained site");
    std::fs::write(site_dir.join("state.pb"), b"retained state").expect("retained state");

    let output = reset_command(&site_dir)
        .forward_test_docker_host(ABSENT_DOCKER_HOST)
        .output()
        .expect("run simulated site reset");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Site reset\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=reset-site\nDOTCORE_NODE_ID=reset-node\nDOTCORE_NODE_HOST=127.0.0.1\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
            site_dir.display()
        )
    );
    let stderr = text(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert!(
        lines[0].starts_with("Notice: skipped Docker runtime cleanup because it is unreachable; the site does not bind it"),
        "{stderr}"
    );
    assert_eq!(format!("{}\n", lines[1]), LOOPBACK_FLAG_STDERR, "{stderr}");
    assert_generated_simulated_bundle(&site_dir, "reset-site", "reset-node");
    assert!(!site_dir.join("state.pb").exists());
}

#[test]
fn site_reset_of_a_simulated_site_still_cleans_its_docker_resources_when_reachable() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("simulated-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(
        site_dir.join("site.yaml"),
        format!("siteId: retained-site\nexecutionTargets:\n{SIMULATED_MODEL_TARGET}"),
    )
    .expect("Simulated retained site");

    let docker = FakeDockerDaemon::start();
    let output = reset_command(&site_dir)
        .forward_test_docker_host(&docker.host())
        .output()
        .expect("run simulated site reset");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
    assert_generated_simulated_bundle(&site_dir, "reset-site", "reset-node");
    docker.assert_empty_site_reset_requests(&["retained-site"]);
}

#[test]
fn site_reset_busy_lock_has_zero_mutation_and_actionable_stderr() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("busy-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(site_dir.join("site.yaml"), "siteId: retained-site\n")
        .expect("retained site identity");
    std::fs::write(site_dir.join("state.pb"), b"retained state").expect("retained state");
    let before_site = std::fs::read(site_dir.join("site.yaml")).expect("Site bytes");
    let before_state = std::fs::read(site_dir.join("state.pb")).expect("state bytes");
    let _lock = SiteResetLock::try_acquire(&site_dir).expect("hold conflicting site lock");

    let output = reset_command(&site_dir)
        .output()
        .expect("run busy site reset");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains(&site_dir.to_string_lossy().into_owned()));
    assert!(stderr.contains("stop the backend"));
    assert!(stderr.contains("retry"));
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site bytes after failure"),
        before_site
    );
    assert_eq!(
        std::fs::read(site_dir.join("state.pb")).expect("state bytes after failure"),
        before_state
    );
}

#[test]
fn site_reset_invalid_configuration_has_zero_filesystem_mutation() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("invalid-reset-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(site_dir.join("site.yaml"), "siteId: retained-site\n")
        .expect("retained site identity");
    std::fs::write(site_dir.join("state.pb"), b"retained bytes").expect("retained bytes");
    let before_site = std::fs::read(site_dir.join("site.yaml")).expect("Site bytes");
    let before_state = std::fs::read(site_dir.join("state.pb")).expect("state bytes");

    let output = dotc()
        .args(["site", "reset", "--site-dir"])
        .arg(&site_dir)
        .args([
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--nvidia-device",
            "nvidia.com/gpu=0",
            "--non-interactive",
        ])
        .clear_test_environment()
        .output()
        .expect("run invalid site reset");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("--nvidia-device requires --inference nvidia"));
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site bytes after rejection"),
        before_site
    );
    assert_eq!(
        std::fs::read(site_dir.join("state.pb")).expect("state bytes after rejection"),
        before_state
    );
    assert!(!std::path::PathBuf::from(format!("{}.lock", site_dir.display())).exists());
}

#[test]
fn site_reset_rejects_destructive_confirmation_and_server_options_at_parse_time() {
    for forbidden in ["--force", "--replace", "--management-endpoint"] {
        let output = dotc()
            .args([
                "site",
                "reset",
                "--site-dir",
                "/tmp/reset-invalid-option",
                "--inference",
                "simulated",
                "--applications",
                "simulated",
                forbidden,
            ])
            .clear_test_environment()
            .output()
            .expect("run invalid site reset");

        assert_eq!(output.status.code(), Some(2), "option {forbidden}");
        assert!(output.stdout.is_empty(), "option {forbidden}");
    }
}

#[test]
fn site_init_missing_workload_choice_exits_as_clap_usage_without_reading_stdin() {
    let mut child = dotc()
        .args([
            "site",
            "init",
            "--site-dir",
            "/tmp/site",
            "--applications",
            "simulated",
            "--non-interactive",
        ])
        .clear_test_environment()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run dotc site init");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for dotc");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("--inference"));
    assert!(stderr.contains("--applications"));
    assert!(stderr.contains("--interactive"));
    assert!(!stderr.contains('\x1b'));
}

#[test]
fn site_init_if_missing_conflicts_with_replacement_flags() {
    for replacement in ["--replace", "--force"] {
        let output = dotc()
            .args(["site", "init", "--if-missing", replacement, "--interactive"])
            .clear_test_environment()
            .env("HOME", "/tmp/dotcore-if-missing-home")
            .output()
            .expect("run conflicting if-missing initialization");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(text(&output.stderr).contains("cannot be used with"));
    }
}

#[test]
fn site_init_removal_flags_require_replace_force() {
    for (flags, message) in [
        (
            &["--yes"][..],
            "--yes and --skip-runtime-cleanup require --replace --force",
        ),
        (
            &["--replace", "--skip-runtime-cleanup"][..],
            "--yes and --skip-runtime-cleanup require --replace --force",
        ),
        (&["--force", "--yes"][..], "--force requires --replace"),
    ] {
        let output = dotc()
            .args([
                "site",
                "init",
                "--site-dir",
                "/tmp/init-removal-flags",
                "--inference",
                "simulated",
                "--applications",
                "simulated",
            ])
            .args(flags)
            .clear_test_environment()
            .output()
            .expect("run invalid site init");

        assert_eq!(output.status.code(), Some(2), "{flags:?}");
        assert!(output.stdout.is_empty(), "{flags:?}");
        assert!(text(&output.stderr).contains(message), "{flags:?}");
    }
}

#[test]
fn site_init_replace_force_is_the_same_operation_as_site_reset() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("simulated-site");
    std::fs::create_dir(&site_dir).expect("retained site root");
    std::fs::write(
        site_dir.join("site.yaml"),
        format!("siteId: retained-site\nexecutionTargets:\n{SIMULATED_MODEL_TARGET}"),
    )
    .expect("Simulated retained site");
    std::fs::write(site_dir.join("state.pb"), b"retained state").expect("retained state");

    let output = dotc()
        .args(["site", "init", "--replace", "--force", "--site-dir"])
        .arg(&site_dir)
        .args([
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--site-id",
            "reset-site",
            "--node-id",
            "reset-node",
            "--host",
            "127.0.0.1",
            "--non-interactive",
        ])
        .clear_test_environment()
        .forward_test_docker_host(ABSENT_DOCKER_HOST)
        .output()
        .expect("run init --replace --force");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Site reset\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=reset-site\nDOTCORE_NODE_ID=reset-node\nDOTCORE_NODE_HOST=127.0.0.1\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
            site_dir.display()
        )
    );
    let stderr = text(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert!(
        lines[0].starts_with("Notice: skipped Docker runtime cleanup because it is unreachable"),
        "{stderr}"
    );
    assert_eq!(format!("{}\n", lines[1]), LOOPBACK_FLAG_STDERR, "{stderr}");
    assert_generated_simulated_bundle(&site_dir, "reset-site", "reset-node");
    assert!(!site_dir.join("state.pb").exists());
}

#[test]
fn site_init_if_missing_returns_before_prompt_for_a_valid_site() {
    let fixture = TemporarySite::new();
    let site_before = std::fs::read(fixture.site_path().join("site.yaml")).expect("Site bytes");
    let node_before = std::fs::read(fixture.site_path().join("node.yaml")).expect("node bytes");
    let mut command = dotc();
    command
        .args([
            "site",
            "init",
            "--if-missing",
            "--interactive",
            "--site-dir",
        ])
        .arg(fixture.site_path())
        .clear_test_environment();
    detach_from_controlling_terminal(&mut command);
    let output = command.output().expect("run existing site initialization");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read(fixture.site_path().join("site.yaml")).expect("Site bytes after"),
        site_before
    );
    assert_eq!(
        std::fs::read(fixture.site_path().join("node.yaml")).expect("node bytes after"),
        node_before
    );
    assert!(!std::path::PathBuf::from(format!("{}.lock", fixture.site_path().display())).exists());
}

#[test]
fn site_init_if_missing_on_non_tty_fails_without_writing() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("missing-parent/interactive-site");
    let mut command = dotc();
    command
        .args(["site", "init", "--site-dir"])
        .arg(&site_dir)
        .args([
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--interactive",
            "--if-missing",
        ])
        .clear_test_environment();
    command.stdin(Stdio::piped());
    detach_from_controlling_terminal(&mut command);
    let output = command.output().expect("run explicit interactive init");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("requires a terminal"));
    assert!(!site_dir.exists());
}

#[test]
fn site_init_interactive_and_noninteractive_conflict_at_parse_time() {
    let output = dotc()
        .args([
            "site",
            "init",
            "--site-dir",
            "/tmp/interactive-conflict-site",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--interactive",
            "--non-interactive",
        ])
        .clear_test_environment()
        .output()
        .expect("run conflicting interaction flags");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("cannot be used with"));
}

#[test]
fn guided_presentation_uses_the_controlling_terminal_and_keeps_process_streams_exact() {
    for (answer, expected_code, writes) in [("\nyes\n", 0, true), ("\nno\n", 130, false)] {
        let fixture = TemporarySite::empty();
        let missing_parent = fixture.root.join("missing");
        let site_dir = missing_parent.join(".dotcore/site");
        assert!(!missing_parent.exists());
        let mut command = dotc();
        command
            .args(["site", "init", "--site-dir"])
            .arg(&site_dir)
            .args([
                "--host",
                "127.0.0.1",
                "--inference",
                "simulated",
                "--applications",
                "simulated",
                "--site-id",
                "site-test",
                "--node-id",
                "node-test",
                "--interactive",
            ])
            .clear_test_environment();

        // The answer is typed ahead of the prompt, on a terminal that answers no cursor query.
        let session =
            Session::new([Step::Send(answer.as_bytes())]).without_cursor_position_answers();
        let Some(run) = run_in_pty(command, session) else {
            return;
        };

        assert_eq!(run.code(), Some(expected_code), "screen:\n{}", run.screen());
        assert_eq!(run.stderr(), "");
        let expected_stdout = if writes {
            format!(
                "Site initialized\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=site-test\nDOTCORE_NODE_ID=node-test\nDOTCORE_NODE_HOST=127.0.0.1\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
                site_dir.display()
            )
        } else {
            String::new()
        };
        assert_eq!(run.stdout(), expected_stdout);
        assert_eq!(missing_parent.exists(), writes);
        assert_eq!(site_dir.exists(), writes);
        assert_eq!(site_dir.with_extension("lock").exists(), writes);
        if writes {
            assert!(missing_parent.is_dir());
            assert!(site_dir.is_dir());
            assert!(site_dir.with_extension("lock").is_file());
            assert_generated_simulated_bundle(&site_dir, "site-test", "node-test");
        }
    }
}

#[test]
fn rich_guided_golden_path_is_driven_by_arrow_keys_and_keeps_process_streams_exact() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("golden");
    let steps = [
        Step::Wait("Other nodes reach this node"),
        Step::Wait("? Node host"),
        Step::Send(KEY_DOWN),
        Step::Send(KEY_UP),
        Step::Send(KEY_ENTER),
        Step::Wait("? Node name"),
        Step::Send(b"Test node"),
        Step::Send(KEY_ENTER),
        // Simulated is listed last and never checked: move to it and check it with Space.
        Step::Wait("? Applications"),
        Step::Send(KEY_END),
        Step::Send(KEY_SPACE),
        Step::Send(KEY_ENTER),
        Step::Wait("? Write this site configuration?"),
        Step::Send(b"y"),
    ];

    // Inference comes from the flag, since which accelerators are listed depends on the machine.
    let Some(run) = run_in_pty(
        rich_guided_init(&site_dir, &["--inference", "simulated"]),
        Session::new(steps),
    ) else {
        return;
    };

    // The default host is the recommended one of the machine running the tests.
    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stderr(), "");
    let stdout = run.stdout();
    let host = record(&stdout, "DOTCORE_NODE_HOST=");
    assert_eq!(
        stdout,
        format!(
            "Site initialized\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=site-test\nDOTCORE_NODE_ID=node-test\nDOTCORE_NODE_HOST={host}\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
            site_dir.display()
        )
    );
    assert_generated_simulated_bundle_on(&site_dir, "site-test", "node-test", &host);
    assert!(run.color_sgr_count() > 0);
    for answer in [
        format!("✔ Node host · {host}"),
        "✔ Node name · Test node".to_owned(),
        "✔ Applications · simulated".to_owned(),
        "✔ Write this site configuration? · yes".to_owned(),
    ] {
        assert!(
            run.screen().contains(&answer),
            "{answer}; screen:\n{}",
            run.screen()
        );
    }
    // Each answer closes its block, while the plan stays attached to the final question.
    let screen = run.screen();
    for answer in [
        format!("✔ Node host · {host}"),
        "✔ Node name · Test node".to_owned(),
        "✔ Applications · simulated".to_owned(),
    ] {
        assert_eq!(
            neighbor_lines(screen, &answer).1,
            Some(""),
            "{answer}; screen:\n{screen}"
        );
    }
    let (plan, _) = neighbor_lines(screen, "✔ Write this site configuration? · yes");
    assert!(
        plan.is_some_and(|line| !line.is_empty()),
        "screen:\n{screen}"
    );
    let site_yaml = std::fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
    assert!(
        site_yaml.contains("displayName: Test node\n"),
        "{site_yaml}"
    );
    run.assert_terminal_restored();
}

#[test]
fn rich_guided_cancel_exits_130_without_output_or_site() {
    let cases: [(&str, &[&str], Vec<Step>); 6] = [
        (
            "Ctrl-C at the host selector",
            &[],
            vec![Step::Wait("? Node host"), Step::Send(KEY_CTRL_C)],
        ),
        (
            "Esc at the host selector",
            &[],
            vec![Step::Wait("? Node host"), Step::Send(KEY_ESC)],
        ),
        (
            "Ctrl-C at the node name prompt",
            &["--host", "127.0.0.1"],
            vec![Step::Wait("? Node name"), Step::Send(KEY_CTRL_C)],
        ),
        (
            "Esc at the inference selector",
            &["--host", "127.0.0.1", "--display-name", "Test node"],
            vec![Step::Wait("? Inference"), Step::Send(KEY_ESC)],
        ),
        (
            "Ctrl-C at the applications selector",
            &[
                "--host",
                "127.0.0.1",
                "--display-name",
                "Test node",
                "--inference",
                "simulated",
            ],
            vec![Step::Wait("? Applications"), Step::Send(KEY_CTRL_C)],
        ),
        (
            "Esc at the confirmation",
            &[
                "--host",
                "127.0.0.1",
                "--display-name",
                "Test node",
                "--inference",
                "simulated",
                "--applications",
                "simulated",
            ],
            vec![
                Step::Wait("? Write this site configuration?"),
                Step::Send(KEY_ESC),
            ],
        ),
    ];

    for (label, extra, steps) in cases {
        let fixture = TemporarySite::empty();
        let site_dir = fixture.root.join("cancelled");
        let Some(run) = run_in_pty(rich_guided_init(&site_dir, extra), Session::new(steps)) else {
            return;
        };

        assert_eq!(run.code(), Some(130), "{label}; screen:\n{}", run.screen());
        assert_eq!(run.stdout(), "", "{label}");
        assert_eq!(run.stderr(), "", "{label}");
        assert!(!site_dir.exists(), "{label}");
        run.assert_terminal_restored();
    }
}

#[test]
fn rich_guided_another_address_is_entered_by_keystrokes_after_a_rejected_wildcard() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("another-address");
    let steps = [
        Step::Wait("? Node host"),
        // Up wraps to the last entry, 'Enter another address or host name…'.
        Step::Send(KEY_UP),
        Step::Send(KEY_ENTER),
        Step::Wait("? Node host address"),
        Step::Send(b"0.0.0.0\r"),
        Step::Wait("must be a specific non-wildcard IPv4"),
        Step::Wait("? Node host address"),
        Step::Send(b"192.0.2.44\r"),
        Step::Wait("? Write this site configuration?"),
        Step::Send(b"y"),
    ];
    let command = rich_guided_init(
        &site_dir,
        &[
            "--display-name",
            "Test node",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ],
    );

    let Some(run) = run_in_pty(command, Session::new(steps).with_stdin(Stdin::Null)) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stderr(), "");
    assert!(run.stdout().starts_with("Site initialized\n"));
    let site = std::fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
    assert!(site.contains("host: 192.0.2.44\n"), "{site}");
}

#[test]
fn rich_guided_colors_follow_no_color_and_reach_only_the_terminal() {
    for (no_color, colored) in [(true, false), (false, true)] {
        let fixture = TemporarySite::empty();
        let site_dir = fixture.root.join("colors");
        let mut command = rich_guided_init(
            &site_dir,
            &[
                "--host",
                "127.0.0.1",
                "--display-name",
                "Test node",
                "--inference",
                "simulated",
                "--applications",
                "simulated",
            ],
        );
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let steps = [
            Step::Wait("? Write this site configuration?"),
            Step::Send(b"y"),
        ];

        let Some(run) = run_in_pty(command, Session::new(steps)) else {
            return;
        };

        assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
        assert_eq!(run.stderr(), "");
        assert_eq!(
            run.stdout(),
            format!(
                "Site initialized\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=site-test\nDOTCORE_NODE_ID=node-test\nDOTCORE_NODE_HOST=127.0.0.1\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
                site_dir.display()
            ),
            "NO_COLOR set: {no_color}"
        );
        assert!(!run.stdout().contains('\x1b'));
        assert_eq!(
            run.color_sgr_count() > 0,
            colored,
            "NO_COLOR set: {no_color}; colored SGR count {}",
            run.color_sgr_count()
        );
    }
}

#[test]
fn rich_guided_leaves_the_terminal_restored_and_inline() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("restored");
    let all_flags = [
        "--host",
        "127.0.0.1",
        "--display-name",
        "Test node",
        "--inference",
        "simulated",
        "--applications",
        "simulated",
    ];
    let accept = [
        Step::Wait("? Write this site configuration?"),
        Step::Send(b"y"),
    ];
    let Some(accepted) = run_in_pty(
        rich_guided_init(&site_dir, &all_flags),
        Session::new(accept),
    ) else {
        return;
    };
    assert_eq!(accepted.code(), Some(0), "screen:\n{}", accepted.screen());
    accepted.assert_terminal_restored();

    let cancelled_dir = fixture.root.join("restored-cancelled");
    let cancel = [Step::Wait("? Node host"), Step::Send(KEY_CTRL_C)];
    let Some(cancelled) = run_in_pty(rich_guided_init(&cancelled_dir, &[]), Session::new(cancel))
    else {
        return;
    };
    assert_eq!(cancelled.code(), Some(130));
    cancelled.assert_terminal_restored();
}

#[test]
fn rich_guided_falls_back_to_line_prompts_when_the_terminal_never_answers_the_cursor_query() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("silent-terminal");
    let steps = [
        Step::Wait("Other nodes reach this node at this host."),
        Step::Wait("Choice [1]: "),
        Step::Send(b"1\n"),
        Step::Wait("Write this site configuration? [yes]: "),
        Step::Send(b"yes\n"),
    ];
    let command = rich_guided_init(
        &site_dir,
        &[
            "--display-name",
            "Test node",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ],
    );

    let Some(run) = run_in_pty(
        command,
        Session::new(steps).without_cursor_position_answers(),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "session:\n{}", run.session());
    assert_eq!(run.stderr(), "");
    let stdout = run.stdout();
    let host = record(&stdout, "DOTCORE_NODE_HOST=");
    assert_eq!(
        stdout,
        format!(
            "Site initialized\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID=site-test\nDOTCORE_NODE_ID=node-test\nDOTCORE_NODE_HOST={host}\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
            site_dir.display()
        )
    );
    assert!(site_dir.join("site.yaml").is_file());
    assert_eq!(run.color_sgr_count(), 0, "session:\n{}", run.session());
    assert_eq!(run.cursor_query_count(), 1, "session:\n{}", run.session());
    // The host list names every host address and can scroll the heading off the screen, so
    // the heading checks read the whole session.
    assert!(
        run.session().contains("Node host\n  1) "),
        "{}",
        run.session()
    );
    assert!(!run.session().contains("? Node host"), "{}", run.session());
    let session = run.session();
    assert_eq!(
        neighbor_lines(session, "Choice [1]: 1").1,
        Some(""),
        "{session}"
    );
    let (plan, _) = neighbor_lines(session, "Write this site configuration? [yes]: yes");
    assert!(plan.is_some_and(|line| !line.is_empty()), "{session}");
    run.assert_terminal_restored();
}

#[test]
fn rich_guided_fails_with_one_line_when_the_terminal_stops_answering_the_cursor_query() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("silenced-terminal");
    // The first answer proves cursor reports when the terminal opens; the prompt's own query
    // then goes unanswered.
    let session =
        Session::new([Step::Wait("Other nodes reach this node")]).with_cursor_position_answers(1);

    let Some(run) = run_in_pty(rich_guided_init(&site_dir, &[]), session) else {
        return;
    };

    assert_eq!(run.code(), Some(1), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(
        run.stderr(),
        "guided site configuration: guided terminal I/O failed: terminal did not answer the cursor position query\n"
    );
    assert_eq!(run.cursor_query_count(), 2);
    assert!(!site_dir.exists());
    run.assert_terminal_restored();
}

#[test]
fn site_init_fully_specified_flags_run_directly_without_interaction_flag() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("fully-specified-site");
    let mut command = dotc();
    command
        .args(["site", "init", "--site-dir"])
        .arg(&site_dir)
        .args(["--inference", "simulated", "--applications", "simulated"])
        .args(["--host", "127.0.0.1"])
        .clear_test_environment();
    let output = command.output().expect("run fully specified init");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
    assert!(site_dir.join("site.yaml").is_file());
    assert!(site_dir.join("node.yaml").is_file());
}

#[test]
fn site_init_noninteractive_uses_the_home_fallback() {
    let fixture = TemporarySite::empty();
    let home = fixture.root.join("home");
    let site_dir = home.join(".local/state/dotcore/site");
    let output = dotc()
        .args([
            "site",
            "init",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--host",
            "127.0.0.1",
            "--non-interactive",
        ])
        .clear_test_environment()
        .env("HOME", &home)
        .output()
        .expect("run HOME-default site initialization");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
    assert!(site_dir.join("site.yaml").is_file());
    assert!(site_dir.join("node.yaml").is_file());
    assert!(text(&output.stdout).contains(&format!("DOTCORE_SITE_DIR={}\n", site_dir.display())));
}

#[test]
fn site_init_simulated_creates_a_complete_bundle_with_exact_output_and_defaults() {
    let fixture = TemporarySite::empty();
    let missing_parent = fixture.root.join("missing");
    let site_dir = missing_parent.join(".dotcore/site");
    assert!(!missing_parent.exists());
    let output = simulated_init(&site_dir)
        .output()
        .expect("run simulated init");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
    let stdout = text(&output.stdout);
    let site_id = record(&stdout, "DOTCORE_SITE_ID=");
    let node_id = record(&stdout, "DOTCORE_NODE_ID=");
    assert!(site_id.starts_with("site-"));
    assert!(node_id.starts_with("node-"));
    assert_eq!(
        stdout,
        format!(
            "Site initialized\nDOTCORE_SITE_DIR={}\nDOTCORE_SITE_ID={site_id}\nDOTCORE_NODE_ID={node_id}\nDOTCORE_NODE_HOST=127.0.0.1\nDOTCORE_INFERENCE=simulated\nDOTCORE_APPLICATIONS=simulated\n",
            site_dir.display()
        )
    );
    assert_generated_simulated_bundle(&site_dir, &site_id, &node_id);
    assert!(missing_parent.is_dir());
    assert!(site_dir.with_extension("lock").is_file());
}

#[test]
fn site_init_flag_overrides_site_directory_environment() {
    let fixture = TemporarySite::empty();
    let environment_site = fixture.root.join("environment-site");
    let flag_site = fixture.root.join("flag-site");
    let output = simulated_init(&flag_site)
        .env("DOTCORE_SITE_DIR", &environment_site)
        .output()
        .expect("run flag-over-environment init");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(text(&output.stderr), LOOPBACK_FLAG_STDERR);
    assert!(flag_site.join("site.yaml").is_file());
    assert!(flag_site.join("node.yaml").is_file());
    assert!(!environment_site.exists());
    assert!(text(&output.stdout).contains(&format!("DOTCORE_SITE_DIR={}\n", flag_site.display())));
}

#[test]
fn site_validate_flags_override_both_environment_path_sources() {
    let fixture = TemporarySite::new();
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(fixture.site_path())
        .args(["--catalog-dir"])
        .arg(fixture.catalog_path())
        .clear_test_environment()
        .env("DOTCORE_SITE_DIR", fixture.root.join("missing-site"))
        .env("DOTCORE_CATALOG_DIR", fixture.root.join("missing-catalog"))
        .output()
        .expect("run validate with flag-over-environment paths");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Site valid\nDOTCORE_SITE_DIR={}\n",
            fixture.site_path().display()
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn site_init_matching_rerun_preserves_generated_files_byte_for_byte() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("rerun-site");
    let first = simulated_init(&site_dir)
        .output()
        .expect("first simulated init");
    assert_eq!(first.status.code(), Some(0));
    let site_before = std::fs::read(site_dir.join("site.yaml")).expect("Site input");
    let node_before = std::fs::read(site_dir.join("node.yaml")).expect("node input");

    let second = simulated_init(&site_dir)
        .output()
        .expect("matching simulated init");
    assert_eq!(second.status.code(), Some(0));
    assert_eq!(text(&second.stderr), LOOPBACK_FLAG_STDERR);
    let first_stdout = text(&first.stdout);
    let second_stdout = text(&second.stdout);
    let site_id = record(&first_stdout, "DOTCORE_SITE_ID=");
    let node_id = record(&first_stdout, "DOTCORE_NODE_ID=");
    assert_eq!(record(&second_stdout, "DOTCORE_SITE_ID="), site_id);
    assert_eq!(record(&second_stdout, "DOTCORE_NODE_ID="), node_id);
    assert!(second_stdout.starts_with("Site already initialized\n"));
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site input"),
        site_before
    );
    assert_eq!(
        std::fs::read(site_dir.join("node.yaml")).expect("node input"),
        node_before
    );
}

#[test]
fn site_init_records_a_host_name_as_the_site_node_host() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("named-site");

    let output = unhosted_simulated_init(&site_dir)
        .args(["--host", "manager.example.ts.net"])
        .output()
        .expect("run simulated init with a host name");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).starts_with("Site initialized\n"));
    let site = std::fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
    assert!(site.contains("host: manager.example.ts.net\n"), "{site}");
}

#[test]
fn site_init_and_reset_take_the_host_the_listener_variables_name() {
    let fixture = TemporarySite::empty();
    let listeners = [
        ("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:6202"),
        ("DOTCORE_INFERENCE_ADDR", "127.0.0.1:6402"),
        ("DOTCORE_PRIVATE_ADDR", "127.0.0.1:6602"),
    ];
    let rejection = "the server listens on 127.0.0.1 (DOTCORE_MANAGEMENT_ADDR, \
                     DOTCORE_INFERENCE_ADDR, DOTCORE_PRIVATE_ADDR); use --host 127.0.0.1 or \
                     change those variables\n";

    for subcommand in ["init", "reset"] {
        let site_dir = fixture.root.join(format!("listener-{subcommand}"));
        let output = dotc()
            .args(["site", subcommand, "--site-dir"])
            .arg(&site_dir)
            .args([
                "--inference",
                "simulated",
                "--applications",
                "simulated",
                "--non-interactive",
                "--host",
                "100.64.0.1",
            ])
            .clear_test_environment()
            .envs(listeners)
            .output()
            .expect("run mismatched host");
        assert_eq!(output.status.code(), Some(1), "{subcommand}");
        assert!(output.stdout.is_empty(), "{subcommand}");
        assert_eq!(text(&output.stderr), rejection, "{subcommand}");
        assert!(!site_dir.join("site.yaml").exists(), "{subcommand}");
    }

    let site_dir = fixture.root.join("listener-matching");
    let output = unhosted_simulated_init(&site_dir)
        .envs(listeners)
        .args(["--host", "127.0.0.1"])
        .output()
        .expect("run matching host");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));

    let site_dir = fixture.root.join("listener-default");
    let output = unhosted_simulated_init(&site_dir)
        .env("DOTCORE_INFERENCE_ADDR", "192.0.2.10:6402")
        .output()
        .expect("run without a host");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let site = std::fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
    assert!(site.contains("host: 192.0.2.10\n"), "{site}");
}

#[test]
fn site_init_conflicting_intent_fails_without_mutating_the_existing_bundle() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("conflict-site");
    let first = simulated_init(&site_dir)
        .output()
        .expect("first simulated init");
    assert_eq!(first.status.code(), Some(0));
    let site_before = std::fs::read(site_dir.join("site.yaml")).expect("Site input");
    let node_before = std::fs::read(site_dir.join("node.yaml")).expect("node input");

    let output = unhosted_simulated_init(&site_dir)
        .args(["--host", "192.0.2.10"])
        .output()
        .expect("run conflicting simulated init");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        format!(
            "{}run dotc site validate --site-dir {site} to check the existing site: existing site inputs conflict with requested initialization; request their replacement with --replace\n",
            non_local_host_warning("192.0.2.10"),
            site = site_dir.display()
        )
    );
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site input"),
        site_before
    );
    assert_eq!(
        std::fs::read(site_dir.join("node.yaml")).expect("node input"),
        node_before
    );
}

#[test]
fn site_init_invalid_existing_bundle_fails_before_probe_or_mutation() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("invalid-site");
    std::fs::create_dir(&site_dir).expect("existing site root");
    let site_before = b"siteId: [\n";
    std::fs::write(site_dir.join("site.yaml"), site_before).expect("invalid site input");
    std::fs::write(site_dir.join("node.yaml"), "nodeId: node-test\n").expect("node input");

    let output = simulated_init(&site_dir)
        .output()
        .expect("run invalid init");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("site validate"));
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site input"),
        site_before
    );
}

#[test]
fn site_init_replace_rejects_retained_state_without_deleting_or_overwriting_it() {
    let fixture = TemporarySite::empty();
    let site_dir = fixture.root.join("retained-state-site");
    let first = simulated_init(&site_dir)
        .args(["--site-id", "site-test", "--node-id", "node-test"])
        .output()
        .expect("first simulated init");
    assert_eq!(first.status.code(), Some(0));
    let site_before = std::fs::read(site_dir.join("site.yaml")).expect("Site input");
    let node_before = std::fs::read(site_dir.join("node.yaml")).expect("node input");
    let retained_state = b"retained runtime state";
    std::fs::write(site_dir.join("state.pb"), retained_state).expect("retained state");

    let output = unhosted_simulated_init(&site_dir)
        .args([
            "--site-id",
            "site-test",
            "--node-id",
            "node-test",
            "--host",
            "192.0.2.10",
            "--replace",
        ])
        .output()
        .expect("run guarded replacement");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert_eq!(
        stderr,
        format!(
            "{}Site at {} has deployments or state from a backend run that --replace keeps; rerun with --replace --force, or run dotc site reset, to start a fresh site\n",
            non_local_host_warning("192.0.2.10"),
            site_dir.display()
        )
    );
    assert!(!stderr.contains("retained site state requires explicit reset confirmation"));
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site input"),
        site_before
    );
    assert_eq!(
        std::fs::read(site_dir.join("node.yaml")).expect("node input"),
        node_before
    );
    assert_eq!(
        std::fs::read(site_dir.join("state.pb")).expect("retained state"),
        retained_state
    );
}

#[test]
fn site_init_nvidia_selector_without_nvidia_mode_is_a_process_usage_error() {
    let output = dotc()
        .args([
            "site",
            "init",
            "--site-dir",
            "/tmp/nvidia-selector-site",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--nvidia-device",
            "nvidia.com/gpu=gpu0",
        ])
        .clear_test_environment()
        .output()
        .expect("run invalid NVIDIA selector usage");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("requires --inference nvidia"));
}

#[test]
fn site_init_explicit_interactive_mode_requires_a_tty_without_reading_stdin() {
    let mut command = dotc();
    command
        .args([
            "site",
            "init",
            "--site-dir",
            "/tmp/interactive-site",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--interactive",
        ])
        .clear_test_environment()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    detach_from_controlling_terminal(&mut command);
    let mut child = command.spawn().expect("run interactive non-TTY command");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for dotc");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("requires a terminal"));
}

#[test]
fn site_init_rejects_site_directory_symlinks_before_reading_or_writing() {
    use std::os::unix::fs::symlink;

    let fixture = TemporarySite::empty();
    let target = fixture.root.join("target-site");
    let link = fixture.root.join("link-site");
    let first = simulated_init(&target).output().expect("initial site");
    assert_eq!(first.status.code(), Some(0));
    let site_before = std::fs::read(target.join("site.yaml")).expect("Site input");
    symlink(&target, &link).expect("Site directory symlink");

    let output = simulated_init(&link).output().expect("symlink site init");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("must not be a symbolic link"));
    assert_eq!(
        std::fs::read(target.join("site.yaml")).expect("Site input"),
        site_before
    );
}

#[test]
fn site_init_rejects_crlf_and_invalid_utf8_paths_before_writing() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = TemporarySite::empty();
    let crlf_path = fixture.root.join("bad\nsite");
    let crlf = simulated_init(&crlf_path).output().expect("CRLF path init");
    assert_eq!(crlf.status.code(), Some(1));
    assert!(crlf.stdout.is_empty());
    assert_eq!(
        text(&crlf.stderr),
        "DOTCORE_SITE_DIR is invalid: path must not contain carriage returns or newlines\n"
    );
    assert!(!crlf_path.exists());

    let invalid_utf8 = OsString::from_vec(
        fixture
            .root
            .join("invalid")
            .into_os_string()
            .into_encoded_bytes()
            .into_iter()
            .chain([0xff])
            .collect(),
    );
    let invalid = dotc()
        .args([
            "site",
            "init",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--non-interactive",
            "--site-dir",
        ])
        .arg(invalid_utf8)
        .clear_test_environment()
        .output()
        .expect("invalid UTF-8 path init");
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert_eq!(
        text(&invalid.stderr),
        "DOTCORE_SITE_DIR must be valid UTF-8\n"
    );
}

#[test]
fn site_init_help_describes_composable_choices_and_generated_defaults() {
    let output = dotc()
        .args(["site", "init", "--help"])
        .clear_test_environment()
        .output()
        .expect("run site init help");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let help = text(&output.stdout).to_lowercase();
    for term in [
        "simulated",
        "nvidia",
        "docker",
        "generated",
        "127.0.0.1",
        "always wins",
        "tailnet name when magicdns resolves it here",
        "start the server after tailscale",
    ] {
        assert!(help.contains(term), "help missing {term}: {help}");
    }
    for profile_term in ["profile", "simulated-docker", "direct-linux", "upstream"] {
        assert!(
            !help.contains(profile_term),
            "help contains {profile_term}: {help}"
        );
    }
}

#[test]
fn node_add_and_join_run_non_interactively_and_install_a_node_folder() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("joined-node");
    let token_file = nodes.root.join("join.token");
    let site_before = std::fs::read(fixture.site_path().join("site.yaml")).expect("Site input");

    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let joined = node_join(&token_file, &node_dir, "Worker two", "127.0.0.3")
        .output()
        .expect("run node join");
    let added = add.finish();

    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(text(&joined.stderr), "");
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));
    // The non-default manager private port is called out first, then the loopback manager host.
    // The loopback test node proposes 127.0.0.3 but connects from the source address the host
    // picks toward the 127.0.0.2 listener: 127.0.0.1 on Linux, 127.0.0.2 on macOS.
    assert_eq!(
        text(&added.stderr),
        format!(
            "Warning: nodes will reach the manager on port 9443; start the manager with DOTCORE_PRIVATE_ADDR=127.0.0.2:9443 (or dotcore --private-addr 127.0.0.2:9443)\nWarning: the manager host 127.0.0.2 is a loopback address, which only this machine reaches; a remote node must join with --manager-host <address it reaches this machine at>, and this command and the manager need DOTCORE_PRIVATE_ADDR=<that address>:9443 (or dotcore --private-addr <that address>:9443); for the node's apps to reach inference, the manager also needs DOTCORE_INFERENCE_ADDR=<that address>:50052\nWarning: the node proposes host 127.0.0.3 but connected from {}; confirm that other nodes reach it at the proposed host\n",
            loopback_peer_address_toward(std::net::Ipv4Addr::new(127, 0, 0, 2))
        )
    );
    let node_id = record(&text(&added.stdout), "DOTCORE_NODE_ID=");
    assert!(node_id.starts_with("node-"), "{node_id}");
    assert_eq!(
        text(&added.stdout),
        format!(
            "Node join ready\nDOTCORE_JOIN_COMMAND=dotc node join --token-file {}\nNode added\nDOTCORE_NODE_ID={node_id}\nDOTCORE_NODE_NAME=Worker two\nDOTCORE_NODE_HOST=127.0.0.3\n",
            token_file.display(),
        )
    );
    assert_eq!(
        text(&joined.stdout),
        format!(
            "Node joined\nDOTCORE_SITE_ID=cli-test-site\nDOTCORE_NODE_ID={node_id}\nDOTCORE_NODE_NAME=Worker two\nDOTCORE_NODE_HOST=127.0.0.3\nDOTCORE_PRIVATE_ADDR=127.0.0.3:50053\nDOTCORE_START_COMMAND=dotcore --site-dir {}\n",
            node_dir.display()
        )
    );
    assert!(
        !token_file.exists(),
        "the token file is removed when node add ends"
    );

    let site_after =
        std::fs::read_to_string(fixture.site_path().join("site.yaml")).expect("updated site input");
    assert_ne!(site_after.as_bytes(), site_before);
    for expected in [
        format!("nodeId: {node_id}"),
        "displayName: Worker two".to_owned(),
        "host: 127.0.0.3".to_owned(),
        format!("siteNodeId: {node_id}"),
    ] {
        assert!(
            site_after.contains(&expected),
            "missing {expected} in {site_after}"
        );
    }
    assert_eq!(site_after.matches("siteManager:").count(), 1);
    assert_eq!(
        site_after
            .matches(&format!("siteNodeId: {node_id}"))
            .count(),
        2
    );
    assert_eq!(
        pin_files(fixture.site_path()),
        [format!("{node_id}.sha256")]
    );

    assert!(!node_dir.join("site.yaml").exists());
    for artifact in [
        "worker.yaml",
        "manager.der",
        "trust/site-ca.der",
        "trust/node.der",
        "trust/node-key.pk8",
    ] {
        let metadata = std::fs::metadata(node_dir.join(artifact)).expect("node folder artifact");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600, "{artifact}");
    }
    for directory in [node_dir.clone(), node_dir.join("trust")] {
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "{}",
            directory.display()
        );
    }
    assert!(!node_dir.join("trust/site-ca-key.pk8").exists());
    let manifest = std::fs::read_to_string(node_dir.join("worker.yaml")).expect("worker manifest");
    assert!(
        manifest.contains(&format!("workerNodeId: {node_id}")),
        "{manifest}"
    );
    assert!(manifest.contains("nodeId: cli-test-node"), "{manifest}");
    assert!(
        manifest.contains("privateEndpoint: https://127.0.0.2:9443/"),
        "{manifest}"
    );
    assert!(manifest.contains("127.0.0.3:50053"), "{manifest}");
    assert!(!manifest.contains("privateKey"));
}

#[test]
fn node_add_advertises_the_declared_manager_host_when_it_listens_on_another_address() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let token_file = nodes.root.join("join.token");
    let node_dir = nodes.root.join("joined-node");

    // The site declares 127.0.0.2 while the listener binds the wildcard address.
    {
        let listen = "0.0.0.0:0";
        let mut command =
            node_add_command_listening(fixture.site_path(), Some(&token_file), listen);
        command
            .arg("--yes")
            .env("DOTCORE_PRIVATE_ADDR", "127.0.0.1:50999")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let add = NodeAdd::start_command(command, &token_file);
        let token = JoinToken::parse(&add.token()).expect("token");
        assert_eq!(token.endpoint.host_str(), Some("127.0.0.2"), "{listen}");
        assert_ne!(token.endpoint.port(), Some(0));

        let joined = node_join(&token_file, &node_dir, "Wildcard node", "127.0.0.3")
            .output()
            .expect("run node join");
        assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
        let added = add.finish();
        assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));

        // The recorded private endpoint is the declared host with the private listener's port.
        let manifest = std::fs::read_to_string(node_dir.join("worker.yaml")).expect("manifest");
        assert!(
            manifest.contains("privateEndpoint: https://127.0.0.2:50999/"),
            "{manifest}"
        );
    }
}

#[test]
fn node_join_manager_host_reaches_a_listener_the_token_address_does_not_name() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let token_file = nodes.root.join("join.token");
    let node_dir = nodes.root.join("joined-node");

    // The token names the declared host 127.0.0.2 while the node reaches the manager at 127.0.0.1.
    let mut command =
        node_add_command_listening(fixture.site_path(), Some(&token_file), "0.0.0.0:0");
    command
        .arg("--yes")
        .args(["--manager-private-port", "50999"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let add = NodeAdd::start_command(command, &token_file);
    let token = JoinToken::parse(&add.token()).expect("token");
    assert_eq!(token.endpoint.host_str(), Some("127.0.0.2"));

    let mut join = node_join(&token_file, &node_dir, "Split node", "127.0.0.3");
    join.args(["--manager-host", "127.0.0.1"]);
    let joined = join.output().expect("run node join");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    let added = add.finish();
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));

    let site = std::fs::read_to_string(fixture.site_path().join("site.yaml")).expect("Site");
    assert!(site.contains("managerHost: 127.0.0.1"), "{site}");
    let manifest = std::fs::read_to_string(node_dir.join("worker.yaml")).expect("manifest");
    assert!(
        manifest.contains("privateEndpoint: https://127.0.0.1:50999/"),
        "{manifest}"
    );
}

#[test]
fn node_add_prints_a_join_command_that_joins_when_run_on_the_new_machine() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("joined-node");

    let mut command = node_add_command_listening(fixture.site_path(), None, "0.0.0.0:0");
    command
        .arg("--yes")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn node add");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut records = String::new();
    while !records.contains("DOTCORE_JOIN_COMMAND=") {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).expect("read record"),
            0,
            "{records}"
        );
        records.push_str(&line);
    }
    let printed = record(&records, "DOTCORE_JOIN_COMMAND=");

    // Running the printed command on the new machine: its flags are passed as printed, and the
    // operator adds the flags that suit the machine.
    let (program, token) = printed
        .split_once(" --token ")
        .expect("the command passes the token as a flag");
    assert_eq!(program, "dotc node join");
    assert!(token.starts_with("dcjoin1."), "{printed}");
    let mut join = node_join_without_token(&node_dir, "Pasted node", "127.0.0.3");
    join.args(["--token", token, "--manager-host", "127.0.0.1"]);
    let joined = join.output().expect("run the printed command");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    let _ = child.wait();

    assert!(node_dir.join("trust/node-key.pk8").is_file());
}

#[test]
fn node_join_without_a_terminal_names_the_manager_host_flag_when_the_token_address_is_unreachable()
{
    let nodes = TemporarySite::empty();
    let closed = dotcore::testing::reserve_port();
    let port = closed.port();
    let token = JoinToken::generate(
        url::Url::parse(&format!("https://127.0.0.1:{port}")).unwrap(),
        "cli-test-site",
        "cli-test-node",
        CertificateFingerprint::from_bytes([1; 32]),
        CertificateFingerprint::from_bytes([2; 32]),
    )
    .expect("token");
    let node_dir = nodes.root.join("unreachable");

    let mut join = node_join_without_token(&node_dir, "Lost node", "127.0.0.3");
    join.args(["--token", &token.encode()]);
    let output = join.output().expect("run node join");

    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "cannot reach the manager at https://127.0.0.1:{port}/"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "pass --manager-host <address or host name this machine reaches the manager at>"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("DOTCORE_MANAGER_HOST"), "{stderr}");
    assert!(!node_dir.exists());
}

#[test]
fn node_join_reads_the_token_from_the_flag_and_warns_about_a_shared_token_file() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();

    let add = NodeAdd::start(fixture.site_path(), &nodes.root.join("env.token"));
    let token = add.token();
    let joined = node_join_with_token_flag(&token, &nodes.root.join("flag-node"))
        .output()
        .expect("join with token flag");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(text(&joined.stderr), "");
    assert_eq!(add.finish().status.code(), Some(0));

    // A separate site frees the aliased loopback host the first join took, since macOS only treats
    // configured loopback aliases as local addresses.
    let shared_fixture = TemporarySite::new();
    let shared = nodes.root.join("shared.token");
    let add = NodeAdd::start(shared_fixture.site_path(), &shared);
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).expect("loosen");
    let joined = node_join(
        &shared,
        &nodes.root.join("shared-node"),
        "Worker three",
        "127.0.0.3",
    )
    .output()
    .expect("join with a shared token file");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(
        text(&joined.stderr),
        format!(
            "Warning: the join token file {} is readable by other users (mode 644); restrict it with chmod 600\n",
            shared.display()
        )
    );
    assert_eq!(add.finish().status.code(), Some(0));
}

#[test]
fn rejected_joins_leave_the_site_unchanged_and_the_token_usable_for_a_corrected_retry() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let token_file = nodes.root.join("join.token");
    let site_before = std::fs::read(fixture.site_path().join("site.yaml")).expect("Site input");
    let add = NodeAdd::start(fixture.site_path(), &token_file);

    // A token with the wrong secret is refused before anything is recorded.
    let wrong_token_file = nodes.root.join("wrong.token");
    std::fs::write(&wrong_token_file, add.token_with_wrong_secret()).expect("wrong token");
    let wrong_secret = node_join(
        &wrong_token_file,
        &nodes.root.join("wrong"),
        "Worker",
        "127.0.0.3",
    )
    .output()
    .expect("join with a wrong secret");
    assert_eq!(wrong_secret.status.code(), Some(1));
    assert!(
        text(&wrong_secret.stderr).contains("node join failed"),
        "{}",
        text(&wrong_secret.stderr)
    );

    let duplicate_name = node_join(
        &token_file,
        &nodes.root.join("name"),
        "CLI test node",
        "127.0.0.3",
    )
    .output()
    .expect("join with a duplicate name");
    assert_eq!(duplicate_name.status.code(), Some(1));
    assert!(
        text(&duplicate_name.stderr).contains("choose another name with --name"),
        "{}",
        text(&duplicate_name.stderr)
    );

    let duplicate_host = node_join(
        &token_file,
        &nodes.root.join("host"),
        "Worker two",
        "127.0.0.2",
    )
    .output()
    .expect("join with a duplicate host");
    assert_eq!(duplicate_host.status.code(), Some(1));
    assert!(
        text(&duplicate_host.stderr).contains(
            "node join failed; set this machine's address with --host: host 127.0.0.2 is already used by node \"CLI test node\""
        ),
        "{}",
        text(&duplicate_host.stderr)
    );

    // Without --host the manager observes a loopback address it cannot record.
    let observed_host = dotc()
        .args([
            "node",
            "join",
            "--yes",
            "--name",
            "Worker two",
            "--site-dir",
        ])
        .arg(nodes.root.join("observed"))
        .arg("--token-file")
        .arg(&token_file)
        .args(["--inference", "simulated", "--applications", "simulated"])
        .clear_test_environment()
        .output()
        .expect("join without a host");
    assert_eq!(observed_host.status.code(), Some(1));
    assert!(
        text(&observed_host.stderr).contains(
            "node join failed; set this machine's address with --host: the manager observed this node at 127."
        ),
        "{}",
        text(&observed_host.stderr)
    );

    assert_eq!(
        std::fs::read(fixture.site_path().join("site.yaml")).unwrap(),
        site_before
    );
    assert!(pin_files(fixture.site_path()).is_empty());
    for rejected in ["wrong", "name", "host", "observed"] {
        assert!(!nodes.root.join(rejected).exists(), "{rejected}");
    }

    let retry = node_join(
        &token_file,
        &nodes.root.join("retry"),
        "Worker two",
        "127.0.0.3",
    )
    .output()
    .expect("corrected retry");
    assert_eq!(retry.status.code(), Some(0), "{}", text(&retry.stderr));
    assert_eq!(add.finish().status.code(), Some(0));
    assert_eq!(pin_files(fixture.site_path()).len(), 1);
}

#[test]
fn node_add_refuses_a_locked_site_with_the_exact_diagnostic() {
    let fixture = TemporarySite::new();
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(format!("{}.lock", fixture.site_path().display()))
        .expect("Site lock");
    // SAFETY: flock is called with a valid owned file descriptor and released when the file closes.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH) }, 0);

    let output = node_add_command(fixture.site_path(), None)
        .arg("--yes")
        .output()
        .expect("run locked node add");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        format!(
            "cannot add a node to the site at {site}: site at {site} is running or locked; stop the manager and retry\n",
            site = fixture.site_path().display()
        )
    );
}

#[test]
fn node_add_without_yes_needs_a_terminal() {
    let fixture = TemporarySite::new();
    let mut command = node_add_command(fixture.site_path(), None);
    detach_from_controlling_terminal(&mut command);

    let output = command.output().expect("run node add without a terminal");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        text(&output.stderr).contains("allowing a node needs a terminal"),
        "{}",
        text(&output.stderr)
    );
}

const WORKER_NODE_ID: &str = "node-7b836d678d251736";
const WORKER_APPLICATION_TARGET: &str = "application-execution-target-7182af3d809190c5-simulated";

#[test]
fn node_remove_leave_and_list_help_describe_their_arguments() {
    let help = |action: &str| {
        let output = dotc()
            .args(["node", action, "--help"])
            .clear_test_environment()
            .output()
            .expect("help");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        text(&output.stdout)
    };

    assert_eq!(
        help("remove"),
        "Remove a worker node from this stopped site; runs on the stopped manager. Run `dotc node leave` on the removed machine afterwards\n\nUsage: dotc node remove [OPTIONS] <NODE>\n\nArguments:\n  <NODE>  Display name or node id of the worker node to remove\n\nOptions:\n      --forget-deployments  Drop the declarations of deployments placed on the node, and the routes only they serve, without cleanup evidence from the node. Without it, a node with deployments is refused\n      --verbose             Show the complete cause chain of a failure on a terminal\n      --site-dir <PATH>     Absolute site or node folder; defaults to $XDG_STATE_HOME/dotcore/site, else ~/.local/state/dotcore/site [env: DOTCORE_SITE_DIR]\n      --yes                 Remove the node without asking for confirmation\n      --catalog-dir <PATH>  Absolute master catalog folder [env: DOTCORE_CATALOG_DIR]\n      --cache-dir <PATH>    Absolute root of the recreatable caches; defaults to $XDG_CACHE_HOME/dotcore, else ~/.cache/dotcore [env: DOTCORE_CACHE_DIR]\n  -h, --help                Print help\n"
    );
    assert_eq!(
        help("leave"),
        "Stop this node's workloads and delete its node folder; runs on the removed machine with its server stopped\n\nUsage: dotc node leave [OPTIONS]\n\nOptions:\n      --verbose             Show the complete cause chain of a failure on a terminal\n      --yes                 Stop the node's workloads and delete its folder without asking for confirmation\n      --site-dir <PATH>     Absolute site or node folder; defaults to $XDG_STATE_HOME/dotcore/site, else ~/.local/state/dotcore/site [env: DOTCORE_SITE_DIR]\n      --catalog-dir <PATH>  Absolute master catalog folder [env: DOTCORE_CATALOG_DIR]\n      --cache-dir <PATH>    Absolute root of the recreatable caches; defaults to $XDG_CACHE_HOME/dotcore, else ~/.cache/dotcore [env: DOTCORE_CACHE_DIR]\n  -h, --help                Print help\n"
    );
    assert_eq!(
        help("list"),
        "List the nodes of this site with their addresses and deployments; runs on the manager, which may be running\n\nUsage: dotc node list [OPTIONS]\n\nOptions:\n      --details             Also show the address each node uses to reach the manager and whether the manager trusts the node's certificate; terminal output only\n      --verbose             Show the complete cause chain of a failure on a terminal\n      --execution-targets   Also list each node's execution targets; terminal output only\n      --site-dir <PATH>     Absolute site or node folder; defaults to $XDG_STATE_HOME/dotcore/site, else ~/.local/state/dotcore/site [env: DOTCORE_SITE_DIR]\n      --catalog-dir <PATH>  Absolute master catalog folder [env: DOTCORE_CATALOG_DIR]\n      --cache-dir <PATH>    Absolute root of the recreatable caches; defaults to $XDG_CACHE_HOME/dotcore, else ~/.cache/dotcore [env: DOTCORE_CACHE_DIR]\n  -h, --help                Print help\n"
    );
}

#[test]
fn node_remove_refuses_a_locked_site_with_the_exact_diagnostic() {
    let fixture = TemporarySite::new();
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(format!("{}.lock", fixture.site_path().display()))
        .expect("Site lock");
    // SAFETY: flock is called with a valid owned file descriptor and released when the file closes.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH) }, 0);

    let output = node_remove_command(fixture.site_path(), "Worker two")
        .output()
        .expect("run locked node remove");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        format!(
            "cannot remove a node from the site at {site}: site at {site} is running or locked; stop the manager and retry\n",
            site = fixture.site_path().display()
        )
    );
}

#[test]
fn node_remove_without_yes_needs_a_terminal() {
    let fixture = TemporarySite::new();
    let mut command = dotc();
    command
        .args(["node", "remove", "Worker two", "--site-dir"])
        .arg(fixture.site_path())
        .clear_test_environment();
    detach_from_controlling_terminal(&mut command);

    let output = command
        .output()
        .expect("run node remove without a terminal");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        "removing a node needs a terminal; run in a terminal or pass --yes to remove it without confirmation\n"
    );
}

#[test]
fn node_remove_yes_drops_the_node_its_targets_and_its_pin_and_prints_the_records() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    join_node(
        fixture.site_path(),
        &nodes.root.join("worker"),
        "Worker two",
        "127.0.0.3",
    );
    assert_eq!(
        pin_files(fixture.site_path()),
        [format!("{WORKER_NODE_ID}.sha256")]
    );

    let output = node_remove_command(fixture.site_path(), "Worker two")
        .output()
        .expect("run node remove");

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!("Node removed\nDOTCORE_NODE_ID={WORKER_NODE_ID}\nDOTCORE_NODE_NAME=Worker two\n")
    );
    assert_eq!(
        text(&output.stderr),
        "Notice: Stop the removed node's server, then run `dotc node leave` on that machine to stop its workloads and delete its node folder.\n"
    );
    assert_eq!(pin_files(fixture.site_path()), Vec::<String>::new());
    let site = std::fs::read_to_string(fixture.site_path().join("site.yaml")).expect("Site");
    assert!(!site.contains(WORKER_NODE_ID), "{site}");
    assert!(!site.contains("Worker two"), "{site}");
    assert_eq!(site.matches("executionTargetId:").count(), 2, "{site}");
    assert_eq!(node_list_stdout(fixture.site_path()), MANAGER_LIST_BLOCK);
}

#[test]
fn node_remove_refuses_placed_deployments_unless_they_are_forgotten() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    join_node(
        fixture.site_path(),
        &nodes.root.join("worker"),
        "Worker two",
        "127.0.0.3",
    );
    place_worker_application(fixture.site_path(), "worker-app");
    let site_before = std::fs::read(fixture.site_path().join("site.yaml")).expect("Site");

    let refused = node_remove_command(fixture.site_path(), WORKER_NODE_ID)
        .output()
        .expect("run refused node remove");

    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    assert_eq!(
        text(&refused.stderr),
        format!(
            "cannot remove node {WORKER_NODE_ID:?}: node \"Worker two\" still has 1 deployment(s) placed on it:\n  - application deployment worker-app (Worker app, present)\nremove them through the console while the site runs, or drop their declarations without cleanup by passing --forget-deployments\n"
        )
    );
    assert_eq!(
        std::fs::read(fixture.site_path().join("site.yaml")).expect("Site"),
        site_before
    );
    assert_eq!(
        pin_files(fixture.site_path()),
        [format!("{WORKER_NODE_ID}.sha256")]
    );

    let forgotten = node_remove_command(fixture.site_path(), "Worker two")
        .arg("--forget-deployments")
        .output()
        .expect("run forgetting node remove");

    assert_eq!(
        forgotten.status.code(),
        Some(0),
        "{}",
        text(&forgotten.stderr)
    );
    assert_eq!(
        text(&forgotten.stdout),
        format!(
            "Node removed\nDOTCORE_NODE_ID={WORKER_NODE_ID}\nDOTCORE_NODE_NAME=Worker two\nDOTCORE_FORGOTTEN_DEPLOYMENT=worker-app\n"
        )
    );
    let site = std::fs::read_to_string(fixture.site_path().join("site.yaml")).expect("Site");
    assert!(!site.contains("worker-app"), "{site}");
    assert_eq!(pin_files(fixture.site_path()), Vec::<String>::new());
}

#[test]
fn node_remove_of_an_already_removed_node_id_succeeds_and_an_unknown_name_fails() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    join_node(
        fixture.site_path(),
        &nodes.root.join("worker"),
        "Worker two",
        "127.0.0.3",
    );
    let removed = node_remove_command(fixture.site_path(), WORKER_NODE_ID)
        .output()
        .expect("remove");
    assert_eq!(removed.status.code(), Some(0));

    let retry = node_remove_command(fixture.site_path(), WORKER_NODE_ID)
        .output()
        .expect("retry by id");
    assert_eq!(retry.status.code(), Some(0), "{}", text(&retry.stderr));
    assert_eq!(
        text(&retry.stdout),
        format!("Node already removed\nDOTCORE_NODE_ID={WORKER_NODE_ID}\n")
    );

    let by_name = node_remove_command(fixture.site_path(), "Worker two")
        .output()
        .expect("retry by name");
    assert_eq!(by_name.status.code(), Some(1));
    assert_eq!(
        text(&by_name.stderr),
        "cannot remove node \"Worker two\": no node \"Worker two\" in this site; declared nodes: \"CLI test node\" (cli-test-node)\n"
    );

    let manager = node_remove_command(fixture.site_path(), "CLI test node")
        .output()
        .expect("remove manager");
    assert_eq!(manager.status.code(), Some(1));
    assert_eq!(
        text(&manager.stderr),
        "cannot remove node \"CLI test node\": node \"CLI test node\" is the site manager and cannot be removed; only worker nodes can leave the site\n"
    );
}

#[test]
fn a_removed_node_leaves_and_joins_again_with_the_same_name_host_and_node_id() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");

    let removed = node_remove_command(fixture.site_path(), "Worker two")
        .output()
        .expect("remove");
    assert_eq!(removed.status.code(), Some(0), "{}", text(&removed.stderr));

    // The folder is left, then the same folder path joins again.
    let left = node_leave_command(&node_dir)
        .forward_test_docker_host(ABSENT_DOCKER_HOST)
        .output()
        .expect("leave");
    assert_eq!(left.status.code(), Some(0), "{}", text(&left.stderr));
    assert_eq!(
        text(&left.stdout),
        format!("Node left\nDOTCORE_SITE_ID=cli-test-site\nDOTCORE_NODE_ID={WORKER_NODE_ID}\n")
    );
    assert_eq!(
        text(&left.stderr),
        "Notice: skipped Docker runtime cleanup because it is unreachable; the site does not bind it, so only resources left by an earlier configuration could remain: connect to local Docker daemon for site cleanup: Docker Engine API request failed: Socket not found: /nonexistent/docker.sock: Socket not found: /nonexistent/docker.sock\n"
    );
    assert!(!node_dir.exists());

    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");

    assert_eq!(
        pin_files(fixture.site_path()),
        [format!("{WORKER_NODE_ID}.sha256")]
    );
    assert!(node_dir.join("worker.yaml").is_file());
    assert!(
        std::fs::read_to_string(node_dir.join("worker.yaml"))
            .expect("manifest")
            .contains(&format!("workerNodeId: {WORKER_NODE_ID}"))
    );
    assert_eq!(
        node_list_stdout(fixture.site_path()),
        format!("{MANAGER_LIST_BLOCK}\n{}", worker_list_block())
    );
}

/// The node folder files and site files a join again must leave alone or change only in the
/// node's addresses.
struct JoinedState {
    site: String,
    pins: Vec<(String, Vec<u8>)>,
    worker: String,
    certificate: Vec<u8>,
    key: Vec<u8>,
    trust_anchor: Vec<u8>,
}

impl JoinedState {
    fn read(site_dir: &Path, node_dir: &Path) -> Self {
        let pins = pin_files(site_dir)
            .into_iter()
            .map(|name| {
                let bytes = std::fs::read(site_dir.join("trust/worker-pins").join(&name))
                    .expect("pin file");
                (name, bytes)
            })
            .collect();
        let trust = node_dir.join("trust");
        Self {
            site: std::fs::read_to_string(site_dir.join("site.yaml")).expect("site.yaml"),
            pins,
            worker: std::fs::read_to_string(node_dir.join("worker.yaml")).expect("worker.yaml"),
            certificate: std::fs::read(trust.join("node.der")).expect("node certificate"),
            key: std::fs::read(trust.join("node-key.pk8")).expect("node key"),
            trust_anchor: std::fs::read(trust.join("site-ca.der")).expect("trust anchor"),
        }
    }
}

/// Runs `node add --yes` on the fixture site and `node join` from the intact folder, with the
/// extra join arguments.
fn rejoin_node(site_dir: &Path, node_dir: &Path, extra: &[&str]) -> (Output, Output) {
    let token_file = node_dir.with_extension("rejoin.token");
    let add = NodeAdd::start(site_dir, &token_file);
    let joined = rejoin_command(&token_file, node_dir, extra)
        .output()
        .expect("run node join again");
    (joined, add.finish())
}

fn rejoin_command(token_file: &Path, node_dir: &Path, extra: &[&str]) -> Command {
    let mut command = dotc();
    command
        .args(["node", "join", "--yes", "--site-dir"])
        .arg(node_dir)
        .arg("--token-file")
        .arg(token_file)
        .args(extra)
        .clear_test_environment();
    command
}

#[test]
fn a_node_joins_again_from_its_intact_folder_on_a_new_host() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let before = JoinedState::read(fixture.site_path(), &node_dir);

    let (joined, added) = rejoin_node(fixture.site_path(), &node_dir, &["--host", "127.0.0.4"]);

    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(
        text(&joined.stdout),
        format!(
            "Node rejoined\nDOTCORE_SITE_ID=cli-test-site\nDOTCORE_NODE_ID={WORKER_NODE_ID}\nDOTCORE_NODE_NAME=Worker two\nDOTCORE_NODE_HOST=127.0.0.4\nDOTCORE_PRIVATE_ADDR=127.0.0.4:50053\nDOTCORE_START_COMMAND=dotcore --site-dir {}\n",
            node_dir.display()
        )
    );
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));
    assert_eq!(
        text(&added.stdout),
        format!(
            "Node join ready\nDOTCORE_JOIN_COMMAND=dotc node join --token-file {}\nNode rejoined\nDOTCORE_NODE_ID={WORKER_NODE_ID}\nDOTCORE_NODE_NAME=Worker two\nDOTCORE_NODE_HOST=127.0.0.4\n",
            node_dir.with_extension("rejoin.token").display()
        )
    );

    // Only the node's addresses change; its identity, pin, certificate, key and trust stay.
    let after = JoinedState::read(fixture.site_path(), &node_dir);
    assert_eq!(before.site.matches("127.0.0.3").count(), 1);
    assert_eq!(after.site, before.site.replace("127.0.0.3", "127.0.0.4"));
    assert_eq!(
        after.worker,
        before.worker.replace("127.0.0.3", "127.0.0.4")
    );
    assert_ne!(after.worker, before.worker);
    assert_eq!(after.pins, before.pins);
    assert_eq!(after.certificate, before.certificate);
    assert_eq!(after.key, before.key);
    assert_eq!(after.trust_anchor, before.trust_anchor);
    assert_eq!(
        node_list_stdout(fixture.site_path()),
        format!(
            "{MANAGER_LIST_BLOCK}\n{}",
            worker_list_block().replace("127.0.0.3", "127.0.0.4")
        )
    );
}

#[test]
fn a_node_joins_again_on_the_same_host_without_changing_the_site() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let before = JoinedState::read(fixture.site_path(), &node_dir);

    // The name is optional and, when given, must be the folder's node.
    let (joined, added) = rejoin_node(
        fixture.site_path(),
        &node_dir,
        &["--host", "127.0.0.3", "--name", "Worker two"],
    );

    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));
    let after = JoinedState::read(fixture.site_path(), &node_dir);
    assert_eq!(after.site, before.site);
    assert_eq!(after.worker, before.worker);
    assert_eq!(after.pins, before.pins);
    assert_eq!(after.certificate, before.certificate);
}

#[test]
fn a_rejoin_without_a_host_keeps_the_declared_host() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let before = JoinedState::read(fixture.site_path(), &node_dir);

    let (joined, added) = rejoin_node(fixture.site_path(), &node_dir, &[]);

    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));
    assert_eq!(
        JoinedState::read(fixture.site_path(), &node_dir).site,
        before.site
    );
    assert!(text(&joined.stdout).contains("DOTCORE_NODE_HOST=127.0.0.3\n"));
}

#[test]
fn the_operator_reviews_a_rejoin_and_declining_leaves_the_site_and_the_folder_unchanged() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    place_worker_application(fixture.site_path(), "worker-app");
    let before = JoinedState::read(fixture.site_path(), &node_dir);
    let token_file = nodes.root.join("rejoin.token");

    // The listener outlives the decline until its short timeout ends the command.
    let mut add = dotc();
    add.args(["node", "add", "--site-dir"])
        .arg(fixture.site_path())
        .args([
            "--listen",
            "127.0.0.2:0",
            "--timeout",
            "3s",
            "--manager-private-port",
            "9443",
            "--token-file",
        ])
        .arg(&token_file)
        .clear_test_environment()
        .stdin(Stdio::null());
    let add_run = thread::spawn(move || {
        run_in_pty(
            add,
            Session::new([
                Step::Wait("Allow this node to join again?"),
                Step::Send(b"no\n"),
            ]),
        )
    });
    wait_for_file(&token_file);
    let mut join = rejoin_command(&token_file, &node_dir, &["--host", "127.0.0.4"]);
    let joined = join.output().expect("run node join again");
    let Some(added) = add_run.join().expect("node add session") else {
        return;
    };

    let screen = added.screen();
    let peer = loopback_peer_address_toward(std::net::Ipv4Addr::new(127, 0, 0, 2));
    let review = format!(
        "A node asks to join again\n  name             Worker two\n  host             127.0.0.3 -> 127.0.0.4\n  manager host     default (no change)\n  deployments      1 stays on this node\n  connected from   {peer}:{}\n  key fingerprint  {}\n! Proposed host 127.0.0.4 differs from where the node connected from ({peer})\nAllow this node to join again? [no]: no",
        review_value(screen, "connected from")
            .rsplit(':')
            .next()
            .unwrap(),
        review_value(screen, "key fingerprint"),
    );
    assert!(
        screen.contains(&review),
        "expected:\n{review}\nscreen:\n{screen}"
    );
    assert_eq!(joined.status.code(), Some(1));
    assert!(
        text(&joined.stderr).contains("the manager refused"),
        "{}",
        text(&joined.stderr)
    );
    let after = JoinedState::read(fixture.site_path(), &node_dir);
    assert_eq!(after.site, before.site);
    assert_eq!(after.worker, before.worker);
    assert_eq!(after.pins, before.pins);
    assert_eq!(after.certificate, before.certificate);
}

/// Reads a review fact's value off the screen.
fn review_value(screen: &str, label: &str) -> String {
    let line = screen
        .lines()
        .find(|line| line.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("no {label} line:\n{screen}"));
    line.trim_start()
        .strip_prefix(label)
        .expect("label")
        .trim()
        .to_owned()
}

#[test]
fn a_folder_of_another_site_or_an_earlier_trust_is_refused_before_contacting_the_manager() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let before = JoinedState::read(fixture.site_path(), &node_dir);
    let token_file = nodes.root.join("other.token");
    let leave_then_join = "using `dotc node leave` here, then `dotc node add` on the manager and `dotc node join` here";

    for (site_id, expected) in [
        (
            "another-site",
            format!(
                "cannot join again from {dir}: the node folder at {dir} belongs to another site; leave that site, then join this one as a new node {leave_then_join}\n",
                dir = node_dir.display()
            ),
        ),
        (
            "cli-test-site",
            format!(
                "cannot join again from {dir}: the node folder at {dir} belongs to an earlier trust of this site; leave it, then join as a new node {leave_then_join}\n",
                dir = node_dir.display()
            ),
        ),
    ] {
        let token = JoinToken::generate(
            url::Url::parse("https://127.0.0.1:9").unwrap(),
            site_id,
            "cli-test-node",
            CertificateFingerprint::from_bytes([1; 32]),
            CertificateFingerprint::from_bytes([2; 32]),
        )
        .expect("token");
        std::fs::write(&token_file, token.encode()).expect("token file");
        std::fs::set_permissions(
            &token_file,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("token mode");

        let output = rejoin_command(&token_file, &node_dir, &[])
            .output()
            .expect("run node join again");

        assert_eq!(output.status.code(), Some(1));
        assert_eq!(text(&output.stdout), "");
        assert_eq!(text(&output.stderr), expected);
    }
    let after = JoinedState::read(fixture.site_path(), &node_dir);
    assert_eq!(after.worker, before.worker);
    assert_eq!(after.certificate, before.certificate);
}

#[test]
fn a_running_node_or_a_mismatched_name_is_refused_before_the_manager_is_asked() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let before = JoinedState::read(fixture.site_path(), &node_dir);
    let token_file = nodes.root.join("rejoin.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);

    let renamed = rejoin_command(&token_file, &node_dir, &["--name", "Worker three"])
        .output()
        .expect("run node join again");
    let custody = WorkerNodeCustody::try_acquire(&node_dir)
        .expect("hold the node's custody like a running node");
    let running = rejoin_command(&token_file, &node_dir, &[])
        .output()
        .expect("run node join again");
    drop(custody);
    drop(add);

    assert_eq!(renamed.status.code(), Some(1));
    assert_eq!(
        text(&renamed.stderr),
        format!(
            "--name \"Worker three\" is not the name of the node in {}\n",
            node_dir.display()
        )
    );
    assert_eq!(running.status.code(), Some(1));
    assert_eq!(
        text(&running.stderr),
        format!(
            "cannot join again from {dir}: the node using the folder at {dir} is running; stop it, then join again\n",
            dir = node_dir.display()
        )
    );
    let after = JoinedState::read(fixture.site_path(), &node_dir);
    assert_eq!(after.site, before.site);
    assert_eq!(after.worker, before.worker);
}

#[test]
fn the_intact_folder_of_a_removed_node_is_refused_with_the_leave_then_join_steps() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let removed = node_remove_command(fixture.site_path(), "Worker two")
        .output()
        .expect("remove");
    assert_eq!(removed.status.code(), Some(0), "{}", text(&removed.stderr));
    let site_before = std::fs::read(fixture.site_path().join("site.yaml")).expect("site.yaml");
    let worker_before = std::fs::read(node_dir.join("worker.yaml")).expect("worker.yaml");

    let token_file = nodes.root.join("rejoin.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let joined = rejoin_command(&token_file, &node_dir, &[])
        .output()
        .expect("run node join again");
    drop(add);

    assert_eq!(joined.status.code(), Some(1));
    assert_eq!(
        text(&joined.stderr),
        format!(
            "node join failed: node {WORKER_NODE_ID} is no longer declared in this site; leave this machine's node folder, then join as a new node using `dotc node leave` here, then `dotc node add` on the manager and `dotc node join` here\n"
        )
    );
    assert_eq!(
        std::fs::read(fixture.site_path().join("site.yaml")).expect("site.yaml"),
        site_before
    );
    assert_eq!(
        std::fs::read(node_dir.join("worker.yaml")).expect("worker.yaml"),
        worker_before
    );
    assert!(pin_files(fixture.site_path()).is_empty());
}

#[test]
fn a_lost_folder_with_a_declared_name_points_to_remove_then_join() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    join_node(
        fixture.site_path(),
        &nodes.root.join("worker"),
        "Worker two",
        "127.0.0.3",
    );
    let site_before = std::fs::read(fixture.site_path().join("site.yaml")).expect("site.yaml");
    let token_file = nodes.root.join("fresh.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);

    let joined = node_join(
        &token_file,
        &nodes.root.join("reinstalled"),
        "Worker two",
        "127.0.0.3",
    )
    .output()
    .expect("run node join");
    drop(add);

    assert_eq!(joined.status.code(), Some(1));
    assert_eq!(
        text(&joined.stderr),
        "node join failed; choose another name with --name, or if this machine replaces that node, run `dotc node remove <node name>` on the manager and join again: a node named \"Worker two\" already exists in this site; choose another name, or remove that node first if this machine replaces it\n"
    );
    assert_eq!(
        std::fs::read(fixture.site_path().join("site.yaml")).expect("site.yaml"),
        site_before
    );
    assert!(!nodes.root.join("reinstalled").exists());
}

#[test]
fn node_leave_keeps_a_folder_whose_native_workloads_cannot_be_cleaned() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let elsewhere = nodes.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("link target");
    std::fs::create_dir_all(node_dir.join("runtime")).expect("Runtime state directory");
    std::os::unix::fs::symlink(&elsewhere, node_dir.join("runtime/native"))
        .expect("native state root link");

    let left = node_leave_command(&node_dir).output().expect("leave");

    assert_eq!(left.status.code(), Some(1));
    assert!(left.stdout.is_empty());
    assert_eq!(
        text(&left.stderr),
        format!(
            "cannot leave the site from {node}: its native workloads could not be listed safely, so nothing was stopped and the node folder was kept; resolve the cause below and retry: clean up the native workloads in {node}: native workload state root {node}/runtime/native resolves to {elsewhere}, outside the directory being removed; remove the link by hand\n",
            node = node_dir.display(),
            elsewhere = elsewhere.display()
        )
    );
    assert!(node_dir.join("worker.yaml").is_file());
}

#[test]
fn node_leave_refuses_a_manager_folder_an_empty_folder_and_a_missing_terminal() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();

    let manager = node_leave_command(fixture.site_path())
        .output()
        .expect("leave manager");
    assert_eq!(manager.status.code(), Some(1));
    assert!(manager.stdout.is_empty());
    assert_eq!(
        text(&manager.stderr),
        format!(
            "cannot leave the site from {site}: {site} holds a site (site.yaml), not a joined node; remove it by deleting the site with `dotc site delete`\n",
            site = fixture.site_path().display()
        )
    );
    assert!(fixture.site_path().join("site.yaml").is_file());

    let empty = node_leave_command(&nodes.root)
        .output()
        .expect("leave empty");
    assert_eq!(empty.status.code(), Some(1));
    assert_eq!(
        text(&empty.stderr),
        format!(
            "cannot leave the site from {root}: {root} is not a joined node folder (no worker.yaml)\n",
            root = nodes.root.display()
        )
    );

    let mut without_yes = dotc();
    without_yes
        .args(["node", "leave", "--site-dir"])
        .arg(&nodes.root)
        .clear_test_environment();
    detach_from_controlling_terminal(&mut without_yes);
    let output = without_yes.output().expect("leave without a terminal");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        "leaving the site needs a terminal; run in a terminal or pass --yes to stop the node's workloads and delete its folder without confirmation\n"
    );
}

#[test]
fn site_delete_refuses_a_node_folder_held_by_a_running_node() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    let custody = WorkerNodeCustody::try_acquire(&node_dir).expect("hold node custody");

    let mut command = dotc();
    command
        .args([
            "site",
            "delete",
            "--yes",
            "--skip-runtime-cleanup",
            "--site-dir",
        ])
        .arg(&node_dir)
        .clear_test_environment();
    let output = command.output().expect("run site delete");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        format!(
            "cannot delete the node folder at {node}; stop the node server and retry: node folder at {node} is held by a running node\n",
            node = node_dir.display()
        )
    );
    assert!(node_dir.join("worker.yaml").is_file());

    drop(custody);
    let deleted = {
        let mut command = dotc();
        command
            .args([
                "site",
                "delete",
                "--yes",
                "--skip-runtime-cleanup",
                "--site-dir",
            ])
            .arg(&node_dir)
            .clear_test_environment();
        command.output().expect("run site delete after stop")
    };
    assert_eq!(deleted.status.code(), Some(0), "{}", text(&deleted.stderr));
    assert!(!node_dir.exists());
}

#[test]
fn node_list_prints_one_record_block_per_node_with_and_without_the_site_lock() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");

    assert_eq!(node_list_stdout(fixture.site_path()), MANAGER_LIST_BLOCK);

    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");
    place_worker_application(fixture.site_path(), "worker-app");
    let expected = format!(
        "{MANAGER_LIST_BLOCK}\n{}",
        worker_list_block().replace(
            "DOTCORE_NODE_APPLICATION_DEPLOYMENTS=0",
            "DOTCORE_NODE_APPLICATION_DEPLOYMENTS=1"
        )
    );
    assert_eq!(node_list_stdout(fixture.site_path()), expected);

    // A running manager holds the site lock exclusively; listing does not need it.
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(format!("{}.lock", fixture.site_path().display()))
        .expect("Site lock");
    // SAFETY: flock is called with a valid owned file descriptor and released when the file closes.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    assert_eq!(node_list_stdout(fixture.site_path()), expected);

    let joined = node_list_command(&node_dir).output().expect("list joined");
    assert_eq!(joined.status.code(), Some(1));
    assert!(joined.stdout.is_empty());
    assert_eq!(
        text(&joined.stderr),
        format!(
            "cannot list the nodes of {dir}: {dir} is a joined node folder; the node list lives on the manager, run `dotc node list` there\n",
            dir = node_dir.display()
        )
    );
}

#[test]
fn fake_manager_serves_seeded_deployments_and_applies_lifecycle_requests() {
    use dotcore_proto::dotcore::{
        model::v1::{
            ListModelDeploymentsRequest, ModelDeploymentDesiredState as Desired,
            ModelDeploymentStatus as Status, PauseModelRequest, RemoveModelDeploymentRequest,
            ResumeModelRequest, model_deployment_service_client::ModelDeploymentServiceClient,
        },
        site::v1::{ListConfiguredExecutionTargetsRequest, site_service_client::SiteServiceClient},
    };

    use support::fake_manager::{
        Call, FakeManager, FakeState, NODE_NAME, Rpc, Scripted, local_model_deployment,
    };

    let fake = FakeManager::start(FakeState::with_models(vec![
        local_model_deployment("model-1", "Chat", Status::Ready, Desired::Present),
        local_model_deployment("model-2", "Coder", Status::Paused, Desired::Paused),
    ]));
    fake.state().scripted.insert(
        Rpc::RemoveModelDeployment,
        Scripted::Fail(tonic::Status::failed_precondition("not removable")),
    );
    fake.state()
        .scripted
        .insert(Rpc::ResumeModel, Scripted::Unchanged);
    assert_eq!(
        fake.model_deployment("model-1").desired_state,
        Desired::Present as i32
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("client runtime");
    let removal = runtime.block_on(async {
        let endpoint = format!("http://{}", fake.address());
        let mut models = ModelDeploymentServiceClient::connect(endpoint.clone())
            .await
            .expect("connect to the fake manager");
        let listed = models
            .list_model_deployments(ListModelDeploymentsRequest {})
            .await
            .expect("list")
            .into_inner()
            .deployments;
        assert_eq!(
            listed
                .iter()
                .map(|deployment| deployment.model_deployment_id.as_str())
                .collect::<Vec<_>>(),
            ["model-1", "model-2"]
        );
        let paused = models
            .pause_model(PauseModelRequest {
                model_deployment_id: "model-1".to_owned(),
            })
            .await
            .expect("pause")
            .into_inner()
            .deployment
            .expect("paused deployment");
        assert_eq!(paused.desired_state, Desired::Paused as i32);
        let unknown = models
            .resume_model(ResumeModelRequest {
                model_deployment_id: "model-9".to_owned(),
            })
            .await
            .expect_err("unknown deployment");
        assert_eq!(unknown.code(), tonic::Code::NotFound);
        let unchanged = models
            .resume_model(ResumeModelRequest {
                model_deployment_id: "model-2".to_owned(),
            })
            .await
            .expect("scripted unchanged answer")
            .into_inner()
            .deployment
            .expect("unchanged deployment");
        assert_eq!(unchanged.desired_state, Desired::Paused as i32);
        let names = SiteServiceClient::connect(endpoint)
            .await
            .expect("connect to the fake site service")
            .list_configured_execution_targets(ListConfiguredExecutionTargetsRequest {})
            .await
            .expect("execution targets")
            .into_inner()
            .execution_targets;
        assert_eq!(names[0].site_node_display_name, NODE_NAME);
        models
            .remove_model_deployment(RemoveModelDeploymentRequest {
                model_deployment_id: "model-2".to_owned(),
            })
            .await
            .expect_err("scripted refusal")
    });

    assert_eq!(removal.code(), tonic::Code::FailedPrecondition);
    assert_eq!(removal.message(), "not removable");
    let model_1 = fake.model_deployment("model-1");
    assert_eq!(model_1.desired_state, Desired::Paused as i32);
    assert_eq!(
        model_1.state.expect("observed state").status,
        Status::Ready as i32,
        "the fake does not reconcile an accepted pause"
    );
    assert_eq!(
        fake.model_deployment("model-2").desired_state,
        Desired::Paused as i32,
        "neither the unchanged resume nor the refused removal changes it"
    );
    let call = |rpc, id: Option<&str>| Call {
        rpc,
        id: id.map(str::to_owned),
    };
    assert_eq!(
        fake.calls(),
        [
            call(Rpc::ListModelDeployments, None),
            call(Rpc::PauseModel, Some("model-1")),
            call(Rpc::ResumeModel, Some("model-9")),
            call(Rpc::ResumeModel, Some("model-2")),
            call(Rpc::ListConfiguredExecutionTargets, None),
            call(Rpc::RemoveModelDeployment, Some("model-2")),
        ]
    );
}

#[test]
fn node_remove_asks_the_operator_to_review_and_confirm_in_a_terminal() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    join_node(
        fixture.site_path(),
        &nodes.root.join("worker"),
        "Worker two",
        "127.0.0.3",
    );
    place_worker_application(fixture.site_path(), "worker-app");
    let mut command = dotc();
    command
        .args([
            "node",
            "remove",
            "Worker two",
            "--forget-deployments",
            "--site-dir",
        ])
        .arg(fixture.site_path())
        .clear_test_environment()
        .stdin(Stdio::null());

    let Some(run) = run_in_pty(
        command,
        Session::new([
            Step::Wait("Remove this node from the site?"),
            Step::Send(b"yes\n"),
        ]),
    ) else {
        return;
    };

    assert_eq!(run.code(), Some(0), "screen:\n{}", run.screen());
    assert_eq!(run.stdout(), "");
    assert_eq!(run.stderr(), "");
    run.assert_terminal_restored();
    for expected in [
        "Remove Worker two from the site",
        "These deployments are dropped without cleaning up the node",
        "application  Worker app (present)",
        "Removed Worker two",
        "On that machine, stop its server, then run:",
        "dotc node leave",
    ] {
        assert!(
            run.screen().contains(expected),
            "{expected}\n{}",
            run.screen()
        );
    }
    assert_eq!(pin_files(fixture.site_path()), Vec::<String>::new());
}

#[test]
fn node_remove_refusal_reads_as_a_verdict_with_facts_and_next_steps_on_a_terminal() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    join_node(
        fixture.site_path(),
        &nodes.root.join("worker"),
        "Worker two",
        "127.0.0.3",
    );
    place_worker_application(fixture.site_path(), "worker-app");
    let mut command = dotc();
    command
        .args(["node", "remove", "Worker two", "--site-dir"])
        .arg(fixture.site_path())
        .clear_test_environment()
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null());

    let Some(run) = run_in_pty(command, Session::new([]).with_stderr_on_terminal()) else {
        return;
    };

    assert_eq!(run.code(), Some(1), "screen:\n{}", run.screen());
    assert_eq!(run.stderr(), "");
    let screen = run.screen();
    for expected in [
        "Can't remove Worker two: it still runs 1 deployment",
        "Placed on this node",
        "application  Worker app  present",
        "Drop them without cleaning up the node:",
        "dotc node remove 'Worker two' --forget-deployments",
    ] {
        assert!(screen.contains(expected), "{expected}\n{screen}");
    }
    assert!(!screen.contains("cannot remove node"), "{screen}");
}

#[test]
fn node_join_accepts_the_token_as_a_flag_and_rejects_it_with_a_token_file() {
    let nodes = TemporarySite::empty();
    let output = dotc()
        .args([
            "node",
            "join",
            "--yes",
            "--token",
            "dcjoin1.bad",
            "--site-dir",
        ])
        .arg(nodes.root.join("node"))
        .args(["--inference", "simulated", "--applications", "simulated"])
        .clear_test_environment()
        .output()
        .expect("run node join with a token argument");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("--token holds an invalid token"),
        "{stderr}"
    );

    let output = dotc()
        .args([
            "node",
            "join",
            "--yes",
            "--token",
            "dcjoin1.secret",
            "--token-file",
        ])
        .arg(nodes.root.join("unused.token"))
        .clear_test_environment()
        .output()
        .expect("run node join with both token sources");

    assert_eq!(output.status.code(), Some(2));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("the argument '--token <TOKEN>' cannot be used with '--token-file <PATH>'"),
        "{stderr}"
    );

    for arguments in [["node", "join", "--help"], ["node", "add", "--help"]] {
        let help = dotc().args(arguments).output().expect("help");
        assert_eq!(help.status.code(), Some(0));
        let help = text(&help.stdout);
        assert!(help.contains("--token-file"), "{help}");
        assert_eq!(
            help.contains("--token <TOKEN>"),
            arguments[1] == "join",
            "{help}"
        );
        assert!(!help.contains("DOTCORE_JOIN_TOKEN"), "{help}");
        assert!(!help.contains("DOTCORE_MANAGER_HOST"), "{help}");
    }
}

#[test]
fn node_join_names_node_add_for_an_unrecognized_or_invalid_token() {
    let nodes = TemporarySite::empty();
    let not_a_token = node_join_with_token_flag("hello", &nodes.root.join("not-a-token"))
        .output()
        .expect("run node join with text that is not a token");
    assert_eq!(not_a_token.status.code(), Some(1));
    assert_eq!(
        text(&not_a_token.stderr),
        "--token holds an invalid token: this is not a dotcore join token; copy the whole token that `dotc node add` printed\n"
    );

    let invalid = node_join_with_token_flag("dcjoin1.e30", &nodes.root.join("invalid"))
        .output()
        .expect("run node join with an invalid token payload");
    assert_eq!(invalid.status.code(), Some(1));
    let stderr = text(&invalid.stderr);
    assert!(
        stderr.starts_with("--token holds an invalid token: join token content is invalid (")
            && stderr.ends_with("); request a new token with `dotc node add`\n"),
        "{stderr}"
    );
}

#[test]
fn node_join_in_the_default_folder_prints_the_plain_start_command() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let state = nodes.root.join("state");
    let token_file = nodes.root.join("join.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);

    let joined = dotc()
        .args(["node", "join", "--yes", "--token-file"])
        .arg(&token_file)
        .args([
            "--name",
            "Default folder",
            "--host",
            "127.0.0.3",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ])
        .clear_test_environment()
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &nodes.root)
        .output()
        .expect("run node join");

    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(add.finish().status.code(), Some(0));
    assert_eq!(
        record(&text(&joined.stdout), "DOTCORE_START_COMMAND="),
        "dotcore"
    );
    assert!(state.join("dotcore/site/trust/node-key.pk8").is_file());
}

#[test]
fn node_join_in_a_folder_from_the_environment_prints_the_plain_start_command() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("env-node");
    let token_file = nodes.root.join("join.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);

    let joined = dotc()
        .args(["node", "join", "--yes", "--token-file"])
        .arg(&token_file)
        .args([
            "--name",
            "Env folder",
            "--host",
            "127.0.0.3",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ])
        .clear_test_environment()
        .env("DOTCORE_SITE_DIR", &node_dir)
        .env("HOME", &nodes.root)
        .output()
        .expect("run node join");

    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(add.finish().status.code(), Some(0));
    assert_eq!(
        record(&text(&joined.stdout), "DOTCORE_START_COMMAND="),
        "dotcore"
    );
    assert!(node_dir.join("trust/node-key.pk8").is_file());
}

#[test]
fn server_start_site_dir_flag_beats_the_environment_variable() {
    let fixture = TemporarySite::new();
    std::fs::write(
        fixture.site_path().join("node.yaml"),
        "nodeId: missing-node\n",
    )
    .expect("invalid node configuration");
    let other = TemporarySite::empty();

    for environment_site_dir in [None, Some(other.root.as_path())] {
        let mut command = dotc();
        command
            .args(["server", "start", "--site-dir"])
            .arg(fixture.site_path())
            .clear_test_environment()
            .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
            .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
            .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
            .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
            .env("DOTCORE_CACHE_DIR", fixture.cache_path())
            .env("RUST_LOG", "off");
        if let Some(directory) = environment_site_dir {
            command.env("DOTCORE_SITE_DIR", directory);
        }
        let output = command.output().expect("run dotc");

        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            text(&output.stderr),
            "server start failed: Backend composition failed: node.yaml nodeId \"missing-node\" does not reference a declared node\n"
        );
    }
}

#[test]
fn the_printed_start_command_starts_the_joined_node() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("joined-node");
    let token_file = nodes.root.join("join.token");
    let private_port = dotcore::testing::reserve_port();
    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let mut join = node_join(&token_file, &node_dir, "Started node", "127.0.0.3");
    join.args(["--private-port", &private_port.port().to_string()]);
    let joined = join.output().expect("run node join");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(add.finish().status.code(), Some(0));
    let printed = record(&text(&joined.stdout), "DOTCORE_START_COMMAND=");
    assert_eq!(
        printed,
        format!("dotcore --site-dir {}", node_dir.display())
    );

    // `dotcore` forwards its path flags to the bare server process; run that with no site
    // directory variable.
    let arguments = printed.strip_prefix("dotcore ").expect("dotcore command");
    let mut server = dotc();
    server
        .args(arguments.split(' '))
        .args(["server", "start"])
        .clear_test_environment()
        .env("DOTCORE_CACHE_DIR", nodes.root.join("cache"))
        .env("RUST_LOG", "off")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = server.spawn().expect("start the joined node");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut lines = String::new();
    while !lines.contains("DOTCORE_PRIVATE_ADDRESS=") {
        let mut line = String::new();
        let read = stdout.read_line(&mut line).expect("read startup line");
        if read == 0 {
            let _ = child.kill();
            let output = child.wait_with_output().expect("server output");
            panic!("the node stopped early: {lines:?} {}", text(&output.stderr));
        }
        lines.push_str(&line);
    }
    child.kill().expect("stop the node");
    let _ = child.wait();

    assert_eq!(
        lines,
        format!(
            "Dotcore server starting\nDOTCORE_PRIVATE_ADDRESS=https://127.0.0.3:{}\n",
            private_port.port()
        )
    );
}

#[test]
fn worker_start_failures_name_the_node_add_and_join_commands() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let node_dir = nodes.root.join("worker");
    join_node(fixture.site_path(), &node_dir, "Worker two", "127.0.0.3");

    std::fs::write(
        fixture.site_path().join("node.yaml"),
        format!("nodeId: {WORKER_NODE_ID}\n"),
    )
    .expect("bind the full site to the worker");
    let full_site = failing_server_start(&fixture);
    assert_eq!(full_site.status.code(), Some(1));
    assert!(full_site.stdout.is_empty());
    assert_eq!(
        text(&full_site.stderr),
        format!(
            "server start failed: Backend composition failed: node {WORKER_NODE_ID} is not the site manager and cannot start from a full site; start it from its node folder after joining it to the site with `dotc node add` on the manager and `dotc node join` on this machine\n"
        )
    );

    std::fs::write(node_dir.join("state.pb"), b"worker state").expect("root node state");
    let node_folder = dotc()
        .args(["server", "start", "--site-dir"])
        .arg(&node_dir)
        .clear_test_environment()
        .env("DOTCORE_CACHE_DIR", nodes.root.join("cache"))
        .env("RUST_LOG", "off")
        .output()
        .expect("start the node folder");
    assert_eq!(node_folder.status.code(), Some(1));
    assert!(node_folder.stdout.is_empty());
    assert_eq!(
        text(&node_folder.stderr),
        format!(
            "server start failed: Backend composition failed: node folder at {} keeps node state in state.pb instead of its node store (node/), a state layout this backend does not read; remove the node folder and join the site again with `dotc node add` on the manager and `dotc node join` on this machine\n",
            node_dir.display()
        )
    );
}

#[test]
fn node_join_ignores_the_removed_join_environment_variables() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let token_file = nodes.root.join("join.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let token = add.token();
    let node_dir = nodes.root.join("ignored-env-node");

    // Without a token flag or file, the environment token is not read; the missing-token error
    // is reported.
    let mut join = node_join_without_token(&node_dir, "Ignored", "127.0.0.3");
    join.env("DOTCORE_JOIN_TOKEN", &token)
        .env("DOTCORE_MANAGER_HOST", "127.0.0.9");
    let output = join.output().expect("run node join");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "no join token; pass --token or --token-file, or run in a terminal to enter it\n"
    );
    assert!(!node_dir.exists());

    // The manager host variable does not redirect the connection either: the join reaches the
    // token address and succeeds.
    let mut join = node_join_with_token_flag(&token, &node_dir);
    join.env("DOTCORE_MANAGER_HOST", "127.0.0.9");
    let joined = join.output().expect("run node join");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(add.finish().status.code(), Some(0));
}

#[test]
fn node_join_proposes_the_machine_host_variable_unless_the_host_flag_is_given() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();

    // The variable alone names the proposed host.
    let token_file = nodes.root.join("variable.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let joined = dotc()
        .args([
            "node",
            "join",
            "--yes",
            "--name",
            "Variable node",
            "--site-dir",
        ])
        .arg(nodes.root.join("variable"))
        .arg("--token-file")
        .arg(&token_file)
        .args(["--inference", "simulated", "--applications", "simulated"])
        .clear_test_environment()
        .env("DOTCORE_MACHINE_HOST", "node.example")
        .output()
        .expect("join with the host variable");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(
        record(&text(&joined.stdout), "DOTCORE_NODE_HOST="),
        "node.example"
    );
    assert_eq!(add.finish().status.code(), Some(0));

    // The flag wins over the variable.
    let token_file = nodes.root.join("flag.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let mut join = node_join(
        &token_file,
        &nodes.root.join("flag"),
        "Flag node",
        "127.0.0.3",
    );
    join.env("DOTCORE_MACHINE_HOST", "node.example");
    let joined = join.output().expect("join with the host flag and variable");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    assert_eq!(
        record(&text(&joined.stdout), "DOTCORE_NODE_HOST="),
        "127.0.0.3"
    );
    assert_eq!(add.finish().status.code(), Some(0));

    // The retired DOTCORE_DEV_HOST name is not read: without a host the join falls back to the
    // observed loopback address, which the manager refuses to record. The observed address is the
    // source address the host picks toward the 127.0.0.2 listener: 127.0.0.1 on Linux, 127.0.0.2
    // on macOS.
    let token_file = nodes.root.join("retired.token");
    let add = NodeAdd::start(fixture.site_path(), &token_file);
    let refused = dotc()
        .args([
            "node",
            "join",
            "--yes",
            "--name",
            "Retired node",
            "--site-dir",
        ])
        .arg(nodes.root.join("retired"))
        .arg("--token-file")
        .arg(&token_file)
        .args(["--inference", "simulated", "--applications", "simulated"])
        .clear_test_environment()
        .env("DOTCORE_DEV_HOST", "retired.example")
        .output()
        .expect("join with the retired host variable");
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        text(&refused.stderr),
        format!(
            "node join failed; set this machine's address with --host: the manager observed this node at {}, which it cannot record as the node address; provide the address the manager reaches this node at\n",
            loopback_peer_address_toward(std::net::Ipv4Addr::new(127, 0, 0, 2))
        )
    );
    drop(add);

    let site = std::fs::read_to_string(fixture.site_path().join("site.yaml")).expect("Site");
    assert!(site.contains("host: node.example"), "{site}");
    assert!(site.contains("host: 127.0.0.3"), "{site}");
    assert!(!site.contains("retired.example"), "{site}");
}

#[test]
fn node_join_without_a_token_or_terminal_explains_how_to_supply_it() {
    let nodes = TemporarySite::empty();
    let output = dotc()
        .args([
            "node",
            "join",
            "--yes",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--site-dir",
        ])
        .arg(nodes.root.join("node"))
        .clear_test_environment()
        .output()
        .expect("run node join without a token");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        "no join token; pass --token or --token-file, or run in a terminal to enter it\n"
    );
}

#[test]
fn node_join_refuses_an_existing_directory_before_contacting_the_manager() {
    let nodes = TemporarySite::empty();
    let existing = nodes.root.join("existing");
    std::fs::create_dir(&existing).expect("existing directory");

    let output = dotc()
        .args(["node", "join", "--yes", "--token-file"])
        .arg(nodes.root.join("unused.token"))
        .arg("--site-dir")
        .arg(&existing)
        .clear_test_environment()
        .output()
        .expect("run node join into an existing directory");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        format!(
            "{} already exists; a joined node needs a new directory, pass --site-dir with a path that does not exist\n",
            existing.display()
        )
    );
}

#[test]
fn node_join_rejects_device_selectors_without_nvidia_inference() {
    let output = dotc()
        .args([
            "node",
            "join",
            "--inference",
            "simulated",
            "--nvidia-device",
            "nvidia.com/gpu=gpu0",
        ])
        .clear_test_environment()
        .output()
        .expect("run node join with a contradictory selector");

    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("--nvidia-device requires --inference nvidia"));
}

#[test]
fn node_add_asks_the_operator_and_node_join_enters_the_token_hidden() {
    let fixture = TemporarySite::new();
    let nodes = TemporarySite::empty();
    let token_file = nodes.root.join("join.token");

    // The manager operator allows the node on the terminal.
    let mut add = node_add_command(fixture.site_path(), Some(&token_file));
    add.stdin(Stdio::null());
    let add_run = thread::spawn(move || {
        run_in_pty(
            add,
            Session::new([Step::Wait("Allow this node to join?"), Step::Send(b"yes\n")]),
        )
    });
    wait_for_file(&token_file);
    let token = std::fs::read_to_string(&token_file)
        .expect("token")
        .trim()
        .to_owned();

    // The joining operator pastes the token at a hidden prompt, and a taken name is asked again.
    let node_dir = nodes.root.join("joined-node");
    let mut join = dotc();
    join.args(["node", "join", "--site-dir"])
        .arg(&node_dir)
        .args([
            "--name",
            "CLI test node",
            "--host",
            "127.0.0.3",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ])
        .clear_test_environment();
    let pasted = format!("{token}\n").into_bytes().leak();
    let session = Session::new([
        Step::Wait("Join token (hidden)"),
        Step::Send(pasted),
        Step::Wait("Send this proposal to the manager?"),
        Step::Send(b"yes\n"),
        Step::Wait("Waiting for approval on the manager..."),
        Step::Wait("Node name"),
        Step::Send(b"Interactive node\n"),
    ]);
    let Some(joined) = run_in_pty(join, session) else {
        return;
    };
    let added = add_run
        .join()
        .expect("node add session")
        .expect("pty available");

    assert_eq!(joined.code(), Some(0), "screen:\n{}", joined.screen());
    assert_eq!(joined.stderr(), "");
    assert_eq!(joined.stdout(), "");
    assert_eq!(
        joined.screen().matches("Waiting for approval").count(),
        1,
        "{}",
        joined.screen()
    );
    assert!(
        joined.screen().contains("Start the node:"),
        "{}",
        joined.screen()
    );
    assert!(
        !joined.screen().contains(&token[..16]),
        "the token was echoed:\n{}",
        joined.screen()
    );
    joined.assert_terminal_restored();
    assert!(
        joined.screen().contains("already exists in this site"),
        "{}",
        joined.screen()
    );
    assert_eq!(added.code(), Some(0), "screen:\n{}", added.screen());
    assert!(
        added.screen().contains("A node asks to join"),
        "{}",
        added.screen()
    );
    assert_eq!(added.stdout(), "");
    assert!(
        added.screen().contains("Added Interactive node at "),
        "{}",
        added.screen()
    );
    let site = std::fs::read_to_string(fixture.site_path().join("site.yaml")).expect("Site");
    assert!(site.contains("displayName: Interactive node"), "{site}");
    assert!(node_dir.join("trust/node-key.pk8").is_file());
}

#[test]
fn site_validate_success_prints_only_the_exact_machine_readable_records() {
    let fixture = TemporarySite::new();
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(fixture.site_path())
        .clear_test_environment()
        .output()
        .expect("run dotc site validate");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Site valid\nDOTCORE_SITE_DIR={}\n",
            fixture.site_path().display()
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn site_commands_read_a_site_that_holds_deployments() {
    let fixture = TemporarySite::new();
    let valid = "siteId: cli-test-site\nmodelDeploymentIdentityAllocator: { namespace: 0123456789abcdef, nextSequence: '1' }\nnodes:\n- nodeId: cli-test-node\n  host: 127.0.0.2\n  displayName: CLI test node\n  siteManager: {}\nmodelDeployments:\n- modelDeploymentId: upstream-one\n  snapshot:\n    upstream:\n      upstreamModelTemplate:\n        upstreamModelTemplateId: openai-compatible\n        displayName: OpenAI-compatible\n        wireProtocol: UPSTREAM_WIRE_PROTOCOL_OPENAI_COMPATIBLE\n  desiredState: MODEL_DEPLOYMENT_DESIRED_STATE_PRESENT\n  lifecycleGeneration: '1'\n  lifecycleTransition: MODEL_DEPLOYMENT_LIFECYCLE_TRANSITION_CREATE\n  upstreamDesiredConfiguration:\n    upstreamModelId: hosted-private-model\n    apiKey: test-upstream-key\n    inputCapabilities:\n      supportsText: true\n      supportsImage: true\n    endpoint:\n      baseUrl: https://endpoint.example/v1\n      allowInsecureHttp: false\n    boundary:\n      locality: UPSTREAM_MODEL_LOCALITY_PUBLIC_EXTERNAL\n      trustDomain: endpoint.example\n      dataEgressClass: UPSTREAM_MODEL_DATA_EGRESS_CLASS_PUBLIC_INTERNET\n";
    let site_yaml = fixture.site_path().join("site.yaml");
    std::fs::write(&site_yaml, valid).expect("Upstream site input");
    std::fs::set_permissions(
        &site_yaml,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("owner-only upstream site input");

    let validated = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(fixture.site_path())
        .clear_test_environment()
        .output()
        .expect("validate a site with deployments");
    assert_eq!(
        validated.status.code(),
        Some(0),
        "{}",
        text(&validated.stderr)
    );
    assert_eq!(
        text(&validated.stdout),
        format!(
            "Site valid\nDOTCORE_SITE_DIR={}\n",
            fixture.site_path().display()
        )
    );

    let configuration = dotc()
        .args(["config", "show", "--format", "json", "--site-dir"])
        .arg(fixture.site_path())
        .clear_test_environment()
        .env("HOME", fixture.root.join("home"))
        .output()
        .expect("show the configuration of a site with deployments");
    assert_eq!(
        configuration.status.code(),
        Some(0),
        "{}",
        text(&configuration.stderr)
    );
    let json = serde_json::from_slice::<serde_json::Value>(&configuration.stdout)
        .expect("configuration JSON");
    assert_eq!(json["nodeHost"], "127.0.0.2");

    let replaced = dotc()
        .args(["site", "init", "--replace", "--site-dir"])
        .arg(fixture.site_path())
        .args([
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--non-interactive",
        ])
        .clear_test_environment()
        .output()
        .expect("replace a site with deployments");
    assert_eq!(replaced.status.code(), Some(1));
    assert!(
        text(&replaced.stderr).contains("--replace --force"),
        "{}",
        text(&replaced.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&site_yaml).expect("preserved site input"),
        valid
    );
}

#[test]
fn site_validate_platform_catalog_composition_is_optional_and_read_only() {
    let fixture = TemporarySite::new();
    let site_yaml = std::fs::read(fixture.site_path().join("site.yaml")).expect("Site input");
    let node_yaml = std::fs::read(fixture.site_path().join("node.yaml")).expect("node input");
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(fixture.site_path())
        .args(["--catalog-dir"])
        .arg(fixture.catalog_path())
        .clear_test_environment()
        .output()
        .expect("run dotc site validate with platform catalog");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Site valid\nDOTCORE_SITE_DIR={}\n",
            fixture.site_path().display()
        )
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read(fixture.site_path().join("site.yaml")).expect("Site input"),
        site_yaml
    );
    assert_eq!(
        std::fs::read(fixture.site_path().join("node.yaml")).expect("node input"),
        node_yaml
    );
}

#[test]
fn site_validate_failures_name_the_failing_source_and_do_not_mutate_inputs() {
    let site_failure = TemporarySite::new();
    let invalid_site = b"siteId: [\n";
    std::fs::write(site_failure.site_path().join("site.yaml"), invalid_site)
        .expect("invalid site input");
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(site_failure.site_path())
        .clear_test_environment()
        .output()
        .expect("run invalid site validation");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("site.yaml"));
    assert_eq!(
        std::fs::read(site_failure.site_path().join("site.yaml")).expect("Site input"),
        invalid_site
    );

    let node_failure = TemporarySite::new();
    std::fs::write(
        node_failure.site_path().join("node.yaml"),
        "nodeId: missing-node\n",
    )
    .expect("invalid node input");
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(node_failure.site_path())
        .clear_test_environment()
        .output()
        .expect("run invalid node validation");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        text(&output.stderr).contains("node.yaml"),
        "stderr: {}",
        text(&output.stderr)
    );

    let catalog_failure = TemporarySite::new();
    std::fs::create_dir_all(catalog_failure.site_path().join("catalog/models"))
        .expect("Site catalog models");
    std::fs::write(
        catalog_failure
            .site_path()
            .join("catalog/models/broken.yaml"),
        "modelId: [\n",
    )
    .expect("invalid site catalog");
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(catalog_failure.site_path())
        .clear_test_environment()
        .output()
        .expect("run invalid site catalog validation");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("Site catalog"));
    assert!(text(&output.stderr).contains("models/broken.yaml"));
}

#[test]
fn site_validate_platform_catalog_failure_identifies_cross_source_composition() {
    let fixture = TemporarySite::new();
    std::fs::write(
        fixture.catalog_path().join("models/broken.yaml"),
        "modelId: [\n",
    )
    .expect("invalid platform catalog");
    let output = dotc()
        .args(["site", "validate", "--site-dir"])
        .arg(fixture.site_path())
        .args(["--catalog-dir"])
        .arg(fixture.catalog_path())
        .clear_test_environment()
        .output()
        .expect("run invalid platform catalog validation");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("platform and site catalog sources"));
    assert!(text(&output.stderr).contains("master catalog models/broken.yaml"));
}

#[test]
fn server_start_with_missing_environment_reports_the_full_backend_cause_chain() {
    let output = dotc()
        .args(["server", "start"])
        .clear_test_environment()
        .output()
        .expect("run dotc");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains(
        "server start failed: Invalid site path: cannot resolve DOTCORE_SITE_DIR: XDG_STATE_HOME and HOME are unset or empty"
    ));
    assert!(stderr.contains("backend process configuration failed"));
}

#[test]
fn server_start_with_invalid_node_configuration_reports_the_complete_composition_cause_chain() {
    let fixture = TemporarySite::new();
    std::fs::write(
        fixture.site_path().join("node.yaml"),
        "nodeId: missing-node\n",
    )
    .expect("invalid node configuration");
    let output = dotc()
        .args(["server", "start"])
        .clear_test_environment()
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", fixture.site_path())
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .env("RUST_LOG", "off")
        .output()
        .expect("run dotc");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        text(&output.stderr),
        "server start failed: Backend composition failed: node.yaml nodeId \"missing-node\" does not reference a declared node\n"
    );
}

#[test]
fn server_start_names_site_reset_for_an_unreadable_site_schema_or_state_layout() {
    let fixture = TemporarySite::new();
    let site_path = fixture.site_path().join("site.yaml");
    let site = std::fs::read_to_string(&site_path).expect("desired site");
    std::fs::write(
        &site_path,
        site.replace(
            "applicationRuntime: { simulatedRuntime: {} }",
            "applicationRuntime: { simulatedRuntime: { unknownSetting: 1 } }",
        ),
    )
    .expect("Site from another schema");
    let schema = failing_server_start(&fixture);
    assert_eq!(schema.status.code(), Some(1));
    assert_eq!(
        text(&schema.stderr),
        "server start failed: Backend composition desired site failure: site.yaml does not match the site schema of this build (unknown field `unknownSetting`, there are no fields); correct the named value or reset the site with `dotc site reset`\n"
    );

    std::fs::write(&site_path, site).expect("restore desired site");
    std::fs::write(fixture.site_path().join("state.pb"), b"owner state").expect("owner state");
    let layout = failing_server_start(&fixture);
    assert_eq!(layout.status.code(), Some(1));
    assert_eq!(
        text(&layout.stderr),
        format!(
            "server start failed: Backend composition failed: site at {} has an owner state.pb without a node store (node/), a state layout this backend does not read; stop the backend and reset the site with `dotc site reset`\n",
            fixture.site_path().display()
        )
    );
}

#[test]
fn server_start_with_unavailable_logging_directory_has_no_startup_stdout() {
    let fixture = TemporarySite::new();
    std::fs::write(fixture.site_path().join("logs"), "not a directory").expect("log obstruction");
    let output = dotc()
        .args(["server", "start"])
        .env_clear()
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", fixture.site_path())
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .env("RUST_LOG", "off")
        .output()
        .expect("run dotc");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("Backend composition logging failure"));
    assert!(stderr.contains("could not create backend log directory"));
}

#[test]
fn server_start_with_an_occupied_inference_listener_has_no_startup_stdout() {
    let fixture = TemporarySite::new();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupied inference listener");
    let output = dotc()
        .args(["server", "start"])
        .clear_test_environment()
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env(
            "DOTCORE_INFERENCE_ADDR",
            occupied.local_addr().expect("occupied address").to_string(),
        )
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", fixture.site_path())
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .env("RUST_LOG", "info")
        .output()
        .expect("run dotc");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("failed to bind inference router"));
    assert!(
        stderr.contains("server_listener_bind_failed"),
        "stderr: {stderr}"
    );
}

#[test]
fn server_start_with_an_occupied_management_listener_has_no_startup_stdout() {
    let fixture = TemporarySite::new();
    let occupied =
        std::net::TcpListener::bind("127.0.0.1:0").expect("occupied management listener");
    let output = dotc()
        .args(["server", "start"])
        .clear_test_environment()
        .env(
            "DOTCORE_MANAGEMENT_ADDR",
            occupied.local_addr().expect("occupied address").to_string(),
        )
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", fixture.site_path())
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .output()
        .expect("run dotc");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("failed to bind development backend"));
}

#[test]
fn site_reset_rejects_a_live_server_without_mutation_then_succeeds_after_shutdown() {
    let fixture = TemporarySite::new();
    let site_dir = fixture.site_path();
    let mut command = dotc();
    command
        .args(["server", "start"])
        .clear_test_environment()
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", site_dir)
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .env("RUST_LOG", "off");
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start dotc server");
    let mut server = ServerProcess { child };
    let stdout = server.child.stdout.take().expect("server stdout");
    let stderr = server.child.stderr.take().expect("server stderr");
    let (lines, stdout_reader) = read_lines(stdout);
    let (stderr, stderr_reader) = read_output(stderr);
    wait_for_startup(&mut server.child, &lines, &stderr);
    let site_before = std::fs::read(site_dir.join("site.yaml")).expect("Site input");
    let node_before = std::fs::read(site_dir.join("node.yaml")).expect("node input");

    let busy = reset_command_with_choices(site_dir, "nvidia", "docker")
        .output()
        .expect("run reset while server is live");

    assert_eq!(busy.status.code(), Some(1));
    assert!(busy.stdout.is_empty());
    let busy_stderr = text(&busy.stderr);
    assert!(busy_stderr.contains(&site_dir.to_string_lossy().into_owned()));
    assert!(busy_stderr.contains("stop the backend and retry"));
    assert_eq!(
        std::fs::read(site_dir.join("site.yaml")).expect("Site input after rejection"),
        site_before
    );
    assert_eq!(
        std::fs::read(site_dir.join("node.yaml")).expect("node input after rejection"),
        node_before
    );

    send_signal(&server.child, libc::SIGTERM);
    assert_eq!(wait_for_exit(&mut server.child).code(), Some(0));
    stdout_reader.join().expect("stdout reader");
    stderr_reader.join().expect("stderr reader");

    let docker = FakeDockerDaemon::start();
    let reset = reset_command(site_dir)
        .forward_test_docker_host(&docker.host())
        .output()
        .expect("retry reset after server shutdown");

    assert_eq!(reset.status.code(), Some(0), "{}", text(&reset.stderr));
    assert_eq!(text(&reset.stderr), LOOPBACK_FLAG_STDERR);
    assert_generated_simulated_bundle(site_dir, "reset-site", "reset-node");
    docker.assert_empty_site_reset_requests(&["cli-test-site"]);
}

#[test]
fn server_start_emits_startup_protocol_and_exits_cleanly_after_signals() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        assert_lifecycle(signal);
    }
}

fn assert_lifecycle(signal: libc::c_int) {
    let fixture = TemporarySite::new();
    let mut command = dotc();
    command
        .args(["server", "start"])
        .clear_test_environment()
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", fixture.site_path())
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .env("RUST_LOG", "info");
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start dotc server");
    let mut server = ServerProcess { child };
    let stdout = server.child.stdout.take().expect("server stdout");
    let stderr = server.child.stderr.take().expect("server stderr");
    let (lines, reader) = read_lines(stdout);
    let (stderr, stderr_reader) = read_output(stderr);

    let mut output = wait_for_startup(&mut server.child, &lines, &stderr);
    let addresses = startup_addresses(&output);
    assert_management_responds(addresses.management);
    assert_inference_responds(addresses.inference);
    send_signal(&server.child, signal);
    let status = wait_for_exit(&mut server.child);
    reader.join().expect("stdout reader");
    stderr_reader.join().expect("stderr reader");
    output.extend(lines.try_iter());
    let stderr_output = text(
        &stderr
            .recv_timeout(Duration::from_secs(1))
            .expect("server stderr"),
    );

    assert_eq!(
        status.code(),
        Some(0),
        "signal {signal}; startup output: {output:?}; stderr: {stderr_output}"
    );
    assert_eq!(
        output,
        [
            "Dotcore server starting",
            &format!("DOTCORE_MANAGEMENT_ADDRESS=http://{}", addresses.management),
            &format!("DOTCORE_INFERENCE_ADDRESS=http://{}", addresses.inference),
        ],
        "the server process must emit exactly one complete startup block"
    );
    assert!(
        stderr_output.contains("backend_dispatch"),
        "tracing diagnostics must use stderr: {stderr_output}"
    );
    for event_kind in [
        "cli_process_start_requested",
        "cli_startup_protocol_written",
        "cli_shutdown_completed",
    ] {
        assert!(
            stderr_output.contains(event_kind),
            "missing {event_kind} in stderr diagnostics: {stderr_output}"
        );
    }
}

fn wait_for_workload_health(launch: &Output, timeout: Duration) -> Result<(), String> {
    let response: serde_json::Value = serde_json::from_slice(&launch.stdout)
        .map_err(|error| format!("decode launch JSON for health polling: {error}"))?;
    let endpoint = response["payload"]["loopback_endpoints"]
        .as_array()
        .and_then(|endpoints| endpoints.first())
        .ok_or_else(|| "launch has no loopback endpoint for health polling".to_owned())?;
    let host = endpoint["host"]
        .as_str()
        .ok_or_else(|| "launch health endpoint has no host".to_owned())?;
    let port = endpoint["host_port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .ok_or_else(|| "launch health endpoint has no valid port".to_owned())?;
    let address = format!("{host}:{port}")
        .parse::<std::net::SocketAddr>()
        .map_err(|error| format!("parse launch health endpoint: {error}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_result = "health endpoint was not reachable".to_owned();
    while Instant::now() < deadline {
        match std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let request = format!(
                    "GET /health HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
                );
                if let Err(error) = stream.write_all(request.as_bytes()) {
                    last_result = format!("write health request: {error}");
                } else {
                    let mut response = Vec::new();
                    match stream.take(64 * 1024).read_to_end(&mut response) {
                        Ok(_) if healthy_llama_response(&response) => return Ok(()),
                        Ok(_) => last_result = "llama.cpp health is not ready".to_owned(),
                        Err(error) => last_result = format!("read health response: {error}"),
                    }
                }
            }
            Err(error) => last_result = format!("connect to health endpoint: {error}"),
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "llama.cpp did not become ready within {timeout:?}: {last_result}"
    ))
}

fn healthy_llama_response(response: &[u8]) -> bool {
    let Some((headers, body)) = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|boundary| response.split_at(boundary + 4))
    else {
        return false;
    };
    headers.starts_with(b"HTTP/1.1 200 ")
        && serde_json::from_slice::<serde_json::Value>(body)
            .is_ok_and(|body| body["status"] == "ok")
}

/// Runs `dotc server start` on the fixture site for a start expected to fail during preparation.
fn failing_server_start(fixture: &TemporarySite) -> Output {
    dotc()
        .args(["server", "start"])
        .clear_test_environment()
        .env("DOTCORE_MANAGEMENT_ADDR", "127.0.0.1:0")
        .env("DOTCORE_INFERENCE_ADDR", "127.0.0.1:0")
        .env("DOTCORE_FRONTEND_ORIGIN", "http://127.0.0.1:5173")
        .env("DOTCORE_SITE_DIR", fixture.site_path())
        .env("DOTCORE_CATALOG_DIR", fixture.catalog_path())
        .env("DOTCORE_CACHE_DIR", fixture.cache_path())
        .env("RUST_LOG", "off")
        .output()
        .expect("run dotc server start")
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn bounded process: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("collect bounded process output: {error}"));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("collect timed-out process output: {error}"))?;
                return Err(format!(
                    "process exceeded {timeout:?}: stdout={} stderr={}",
                    text(&output.stdout),
                    text(&output.stderr)
                ));
            }
            Err(error) => return Err(format!("inspect bounded process: {error}")),
        }
    }
}

trait TestCommandEnvironment {
    fn clear_test_environment(&mut self) -> &mut Self;

    fn forward_test_docker_host(&mut self, docker_host: &str) -> &mut Self;
}

impl TestCommandEnvironment for Command {
    fn clear_test_environment(&mut self) -> &mut Self {
        self.env_clear();
        if let Some(certificates) = std::env::var_os("SSL_CERT_FILE") {
            self.env("SSL_CERT_FILE", certificates);
        }
        self
    }

    fn forward_test_docker_host(&mut self, docker_host: &str) -> &mut Self {
        self.env("DOCKER_HOST", docker_host)
    }
}

struct FakeDockerDaemon {
    socket_path: std::path::PathBuf,
    requests: Arc<Mutex<Vec<DockerRequest>>>,
    failures: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeDockerDaemon {
    fn start() -> Self {
        let socket_path = crate::support::canonical_temp_dir().join(format!(
            "dotc-fake-docker-{}-{}.sock",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = UnixListener::bind(&socket_path).expect("bind fake Docker socket");
        listener
            .set_nonblocking(true)
            .expect("make fake Docker listener nonblocking");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let failures = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_requests = Arc::clone(&requests);
        let server_failures = Arc::clone(&failures);
        let server_shutdown = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            while !server_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // BSD-derived kernels (macOS) let accepted sockets inherit the listener's
                        // non-blocking mode; the request read below needs blocking reads.
                        stream
                            .set_nonblocking(false)
                            .expect("make fake Docker connection blocking");
                        let request = read_docker_request(&mut stream);
                        let response = match request {
                            Ok(request) => {
                                let result = fake_docker_response(&request);
                                server_requests
                                    .lock()
                                    .expect("fake Docker request log")
                                    .push(request);
                                if let Err(error) = &result {
                                    server_failures
                                        .lock()
                                        .expect("fake Docker request failure log")
                                        .push(error.clone());
                                }
                                result
                            }
                            Err(error) => {
                                server_failures
                                    .lock()
                                    .expect("fake Docker request failure log")
                                    .push(error.clone());
                                Err(error)
                            }
                        };
                        let body = response.unwrap_or_else(|error| {
                            format!(
                                r#"{{"message":{}}}"#,
                                serde_json::to_string(&error).expect("error JSON")
                            )
                        });
                        let status = if body == "[]" || body.starts_with("{\"ApiVersion\"") {
                            "200 OK"
                        } else {
                            "500 Internal Server Error"
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => {
                        server_failures
                            .lock()
                            .expect("fake Docker request failure log")
                            .push(format!("accept fake Docker request: {error}"));
                        break;
                    }
                }
            }
        });
        Self {
            socket_path,
            requests,
            failures,
            shutdown,
            thread: Some(thread),
        }
    }

    fn host(&self) -> String {
        format!(
            "unix://{}",
            self.socket_path.to_str().expect("fake Docker socket UTF-8")
        )
    }

    fn assert_empty_site_reset_requests(&self, site_ids: &[&str]) {
        let failures = self
            .failures
            .lock()
            .expect("fake Docker request failure log");
        assert!(
            failures.is_empty(),
            "unexpected fake Docker request: {failures:?}"
        );
        let requests = self.requests.lock().expect("fake Docker request log");
        let expected = empty_site_reset_requests(site_ids);
        assert_eq!(*requests, expected);
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join fake Docker server");
        }
        std::fs::remove_file(&self.socket_path).expect("remove fake Docker socket");
    }
}

impl Drop for FakeDockerDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DockerRequest {
    method: String,
    target: String,
}

fn read_docker_request(
    stream: &mut std::os::unix::net::UnixStream,
) -> Result<DockerRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| format!("set fake Docker read timeout: {error}"))?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("read fake Docker request: {error}"))?;
        if read == 0 {
            return Err("fake Docker client closed without a request".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let request = std::str::from_utf8(&bytes)
        .map_err(|error| format!("decode fake Docker request: {error}"))?;
    let mut parts = request
        .lines()
        .next()
        .ok_or_else(|| "fake Docker request has no request line".to_owned())?
        .split_whitespace();
    Ok(DockerRequest {
        method: parts.next().unwrap_or_default().to_owned(),
        target: parts.next().unwrap_or_default().to_owned(),
    })
}

fn fake_docker_response(request: &DockerRequest) -> Result<String, String> {
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/version") => Ok(r#"{"ApiVersion":"1.48","Version":"fake"}"#.to_owned()),
        ("GET", target) if docker_list_request(target, "/containers/json") => Ok("[]".to_owned()),
        ("GET", target) if docker_list_request(target, "/networks") => Ok("[]".to_owned()),
        _ => Err(format!("unexpected Docker request {request:?}")),
    }
}

fn docker_list_request(target: &str, endpoint: &str) -> bool {
    let Some(query) = target.strip_prefix(endpoint) else {
        return false;
    };
    let parameters = query.strip_prefix('?').map(|query| {
        query
            .split('&')
            .filter_map(|parameter| parameter.split_once('='))
            .collect::<std::collections::BTreeMap<_, _>>()
    });
    let Some(parameters) = parameters else {
        return false;
    };
    if endpoint == "/containers/json"
        && (parameters.get("all") != Some(&"true") || parameters.get("size") != Some(&"false"))
    {
        return false;
    }
    let Some(filters) = parameters.get("filters") else {
        return false;
    };
    percent_decode(filters).as_deref().is_some_and(|filters| {
        serde_json::from_str::<serde_json::Value>(filters).is_ok_and(|filters| {
            filters["label"].as_array().is_some_and(|labels| {
                labels.len() == 1
                    && labels[0]
                        .as_str()
                        .is_some_and(|label| label.starts_with("com.dotcore.site="))
            })
        })
    })
}

fn percent_decode(value: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16)?;
                let low = (bytes[index + 2] as char).to_digit(16)?;
                decoded.push((high * 16 + low) as u8);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn empty_site_reset_requests(site_ids: &[&str]) -> Vec<DockerRequest> {
    let mut requests = Vec::with_capacity(site_ids.len() * 11);
    for site_id in site_ids {
        requests.push(DockerRequest {
            method: "GET".to_owned(),
            target: "/version".to_owned(),
        });
        // One inventory listing before confirmation, then prevalidation and three cleanup passes.
        for _ in 0..5 {
            for endpoint in ["/containers/json", "/networks"] {
                requests.push(DockerRequest {
                    method: "GET".to_owned(),
                    target: format!(
                        "{endpoint}?{}filters=%7B%22label%22%3A%5B%22com.dotcore.site%3D{site_id}%22%5D%7D",
                        if endpoint == "/containers/json" {
                            "all=true&size=false&"
                        } else {
                            ""
                        }
                    ),
                });
            }
        }
    }
    requests
}

const KEY_UP: &[u8] = b"\x1b[A";
const KEY_DOWN: &[u8] = b"\x1b[B";
const KEY_END: &[u8] = b"\x1b[F";
const KEY_SPACE: &[u8] = b" ";
const KEY_ENTER: &[u8] = b"\r";
const KEY_ESC: &[u8] = b"\x1b";
const KEY_CTRL_C: &[u8] = b"\x03";

/// Builds a guided `site init` for a rich (`xterm-256color`) terminal with fixed identities.
/// Without a named host, the recommended host is the one of the machine running the tests.
fn rich_guided_init(site_dir: &Path, extra: &[&str]) -> Command {
    let mut command = dotc();
    command
        .args(["site", "init", "--site-dir"])
        .arg(site_dir)
        .args([
            "--site-id",
            "site-test",
            "--node-id",
            "node-test",
            "--interactive",
        ])
        .args(extra)
        .clear_test_environment()
        // The Docker probe would otherwise reach the host's default socket, so screens would
        // depend on whether the developer runs Docker.
        .forward_test_docker_host(ABSENT_DOCKER_HOST)
        // A development workspace offers simulated inference, so the inference selector shows
        // on a host without accelerators.
        .env(
            dotcore::composition::DEV_SOURCE_FLAKE_ENV,
            "git+file:///dev/null",
        )
        .env("TERM", "xterm-256color");
    command
}

/// The warning for a host that is no address of this machine, as stderr prints it.
fn non_local_host_warning(host: &str) -> String {
    format!(
        "Warning: node host {host} is not an address of this machine: the console and inference \
         cannot start on this host, and the private listener cannot once nodes join; use an \
         address of this machine, or start the manager with DOTCORE_INFERENCE_ADDR=0.0.0.0:50052 \
         and DOTCORE_PRIVATE_ADDR=0.0.0.0:50053 (or dotcore --private-addr 0.0.0.0:50053)\n"
    )
}

/// A short Docker host whose socket never exists, so guided Docker probes report it unavailable
/// with a reason that fits the test terminal.
const ABSENT_DOCKER_HOST: &str = "unix:///nonexistent/docker.sock";

/// Non-interactive simulated init pinned to the loopback host, so its output does not depend on
/// the addresses of the machine running the tests.
fn simulated_init(site_dir: &Path) -> Command {
    let mut command = unhosted_simulated_init(site_dir);
    command.args(["--host", "127.0.0.1"]);
    command
}

/// Non-interactive simulated init without a host flag.
fn unhosted_simulated_init(site_dir: &Path) -> Command {
    let mut command = dotc();
    command
        .args(["site", "init", "--site-dir"])
        .arg(site_dir)
        .args([
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--non-interactive",
        ])
        .clear_test_environment();
    command
}

/// What non-interactive init and reset print on stderr for the loopback host flag: only the
/// loopback warning, since the operator named the host.
const LOOPBACK_FLAG_STDERR: &str = "Warning: only this machine can reach node host 127.0.0.1; \
    workers and remote apps need another host\n";

/// A `dotc node add --yes` process waiting for one node, with the token file it wrote.
struct NodeAdd {
    child: Option<Child>,
    token_file: std::path::PathBuf,
}

impl NodeAdd {
    /// Starts node add on the fixture site and waits until its listener published the token.
    fn start(site_dir: &Path, token_file: &Path) -> Self {
        let mut command = node_add_command(site_dir, Some(token_file));
        command
            .arg("--yes")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::start_command(command, token_file)
    }

    /// Starts a prepared node add command and waits until it published the token.
    fn start_command(mut command: Command, token_file: &Path) -> Self {
        let mut child = command.spawn().expect("spawn node add");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !token_file.exists() {
            if let Some(status) = child.try_wait().expect("poll node add") {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("node add exited early with {status}: {stderr}");
            }
            assert!(
                Instant::now() < deadline,
                "node add never published its token"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Self {
            child: Some(child),
            token_file: token_file.to_owned(),
        }
    }

    fn token(&self) -> String {
        std::fs::read_to_string(&self.token_file)
            .expect("token file")
            .trim()
            .to_owned()
    }

    /// Returns a syntactically valid token whose secret is not the real one.
    fn token_with_wrong_secret(&self) -> String {
        let mut token = JoinToken::parse(&self.token()).expect("published token");
        token.secret = JoinSecret::from_bytes([7; 32]);
        token.encode()
    }

    /// Waits for node add to end and returns what it printed.
    fn finish(mut self) -> Output {
        self.child
            .take()
            .expect("running node add")
            .wait_with_output()
            .expect("wait for node add")
    }
}

impl Drop for NodeAdd {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Builds `dotc node add` for the fixture site on loopback, optionally with a token file.
fn node_add_command(site_dir: &Path, token_file: Option<&Path>) -> Command {
    let mut command = node_add_command_listening(site_dir, token_file, "127.0.0.2:0");
    command.args(["--manager-private-port", "9443"]);
    command
}

fn node_add_command_listening(site_dir: &Path, token_file: Option<&Path>, listen: &str) -> Command {
    let mut command = dotc();
    command
        .args(["node", "add", "--site-dir"])
        .arg(site_dir)
        .args(["--listen", listen, "--timeout", "120s"])
        .clear_test_environment();
    if let Some(token_file) = token_file {
        command.arg("--token-file").arg(token_file);
    }
    command
}

/// Builds a non-interactive `dotc node join` with simulated capabilities.
fn node_join(token_file: &Path, node_dir: &Path, name: &str, host: &str) -> Command {
    let mut command = node_join_without_token(node_dir, name, host);
    command.arg("--token-file").arg(token_file);
    command
}

fn node_join_with_token_flag(token: &str, node_dir: &Path) -> Command {
    let mut command = node_join_without_token(node_dir, "Worker flag", "127.0.0.3");
    command.args(["--token", token]);
    command
}

fn node_join_without_token(node_dir: &Path, name: &str, host: &str) -> Command {
    let mut command = dotc();
    command
        .args(["node", "join", "--yes", "--site-dir"])
        .arg(node_dir)
        .args([
            "--name",
            name,
            "--host",
            host,
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ])
        .clear_test_environment();
    command
}

/// Adds one simulated node to the fixture site through the two real commands.
fn join_node(site_dir: &Path, node_dir: &Path, name: &str, host: &str) {
    let token_file = node_dir.with_extension("token");
    let add = NodeAdd::start(site_dir, &token_file);
    let joined = node_join(&token_file, node_dir, name, host)
        .output()
        .expect("run node join");
    assert_eq!(joined.status.code(), Some(0), "{}", text(&joined.stderr));
    let added = add.finish();
    assert_eq!(added.status.code(), Some(0), "{}", text(&added.stderr));
}

const MANAGER_LIST_BLOCK: &str = "Node\nDOTCORE_NODE_ID=cli-test-node\nDOTCORE_NODE_NAME=CLI test node\nDOTCORE_NODE_ROLE=manager\nDOTCORE_NODE_HOST=127.0.0.2\nDOTCORE_NODE_MANAGER_HOST=default\nDOTCORE_NODE_PIN=own identity\nDOTCORE_NODE_EXECUTION_TARGET=cli-test-target model linux/amd64\nDOTCORE_NODE_EXECUTION_TARGET=cli-test-application-target application linux/amd64\nDOTCORE_NODE_MODEL_DEPLOYMENTS=0\nDOTCORE_NODE_APPLICATION_DEPLOYMENTS=0\n";

/// The joined worker's `node list` block: its simulated models run on the default simulated GPU,
/// which the declared machine puts on a Linux host of this machine's architecture, and its
/// simulated applications take that architecture too.
fn worker_list_block() -> String {
    let platform = match std::env::consts::ARCH {
        "aarch64" => "linux/arm64",
        _ => "linux/amd64",
    };
    format!(
        "Node\nDOTCORE_NODE_ID=node-7b836d678d251736\nDOTCORE_NODE_NAME=Worker two\nDOTCORE_NODE_ROLE=worker\nDOTCORE_NODE_HOST=127.0.0.3\nDOTCORE_NODE_MANAGER_HOST=default\nDOTCORE_NODE_PIN=yes\nDOTCORE_NODE_EXECUTION_TARGET=execution-target-7182af3d809190c5-simulated-4d747ac0 model {platform}\nDOTCORE_NODE_EXECUTION_TARGET=application-execution-target-7182af3d809190c5-simulated application {platform}\nDOTCORE_NODE_MODEL_DEPLOYMENTS=0\nDOTCORE_NODE_APPLICATION_DEPLOYMENTS=0\n"
    )
}

/// Builds a non-interactive `dotc node remove` of `node` on the fixture site.
fn node_remove_command(site_dir: &Path, node: &str) -> Command {
    let mut command = dotc();
    command
        .args(["node", "remove", node, "--yes", "--site-dir"])
        .arg(site_dir)
        .clear_test_environment();
    command
}

/// Builds a non-interactive `dotc node leave` on the node folder.
fn node_leave_command(node_dir: &Path) -> Command {
    let mut command = dotc();
    command
        .args(["node", "leave", "--yes", "--site-dir"])
        .arg(node_dir)
        .clear_test_environment();
    command
}

fn node_list_command(site_dir: &Path) -> Command {
    let mut command = dotc();
    command
        .args(["node", "list", "--site-dir"])
        .arg(site_dir)
        .clear_test_environment();
    detach_from_controlling_terminal(&mut command);
    command
}

/// Runs `dotc node list` without a terminal and returns its records.
fn node_list_stdout(site_dir: &Path) -> String {
    let output = node_list_command(site_dir).output().expect("run node list");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    text(&output.stdout)
}

/// Declares an application deployment placed on the joined worker's application target.
fn place_worker_application(site_dir: &Path, deployment_id: &str) {
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
            ResolvedApplicationDeploymentSnapshot,
        },
        deployment::v1::ExactPlacement,
        site::v1::Site,
    };

    let path = site_dir.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&path).expect("read site");
    let platform = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == WORKER_APPLICATION_TARGET)
        .and_then(|target| target.platform.clone());
    let placement = ExactPlacement {
        site_node_id: WORKER_NODE_ID.to_owned(),
        execution_target_id: WORKER_APPLICATION_TARGET.to_owned(),
    };
    site.application_deployments
        .push(ApplicationDeploymentDeclaration {
            application_deployment_id: deployment_id.to_owned(),
            application_deployment_name: "Worker app".to_owned(),
            placement: Some(placement.clone()),
            snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                placement: Some(placement),
                platform,
                ..Default::default()
            }),
            desired_state: ApplicationDeploymentDesiredState::Present as i32,
            lifecycle_generation: 1,
            ..Default::default()
        });
    std::fs::write(
        &path,
        dotcore_proto::yaml::to_string(&site).expect("serialize site"),
    )
    .expect("write site");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("owner-only site");
}

/// Lists the node pin files the site holds, sorted.
fn pin_files(site_dir: &Path) -> Vec<String> {
    let mut pins = std::fs::read_dir(site_dir.join("trust/worker-pins"))
        .map(|entries| {
            entries
                .map(|entry| {
                    entry
                        .expect("pin entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    pins.sort();
    pins
}

/// Returns the peer address a listener on `destination` observes for a local connection to it,
/// which is the source address the host selects toward that destination.
fn loopback_peer_address_toward(destination: std::net::Ipv4Addr) -> std::net::IpAddr {
    let listener = std::net::TcpListener::bind((destination, 0)).expect("bind loopback probe");
    let _client = std::net::TcpStream::connect(listener.local_addr().expect("probe address"))
        .expect("connect loopback probe");
    let (_, peer) = listener.accept().expect("accept loopback probe");
    peer.ip()
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Returns the lines right before and after the first line of `text` starting with `start`,
/// without trailing spaces.
fn neighbor_lines<'a>(text: &'a str, start: &str) -> (Option<&'a str>, Option<&'a str>) {
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let index = lines
        .iter()
        .position(|line| line.starts_with(start))
        .unwrap_or_else(|| panic!("missing {start:?} in:\n{text}"));
    (
        index.checked_sub(1).map(|before| lines[before]),
        lines.get(index + 1).copied(),
    )
}

fn record(stdout: &str, prefix: &str) -> String {
    let mut records = stdout.lines().filter_map(|line| line.strip_prefix(prefix));
    let value = records
        .next()
        .unwrap_or_else(|| panic!("missing {prefix} in {stdout:?}"));
    assert!(records.next().is_none(), "duplicate {prefix} in {stdout:?}");
    value.to_owned()
}

fn assert_generated_simulated_bundle(site_dir: &Path, site_id: &str, node_id: &str) {
    assert_generated_simulated_bundle_on(site_dir, site_id, node_id, "127.0.0.1");
}

fn assert_generated_simulated_bundle_on(site_dir: &Path, site_id: &str, node_id: &str, host: &str) {
    let site = std::fs::read_to_string(site_dir.join("site.yaml")).expect("Site input");
    assert!(site.contains(&format!("siteId: {site_id}\n")));
    assert!(site.contains(&format!("nodeId: {node_id}\n")));
    assert!(site.contains(&format!("host: {host}\n")), "{site}");
    assert!(site.contains("executionTargets:"));
    assert_eq!(site.matches("applicationRuntime:").count(), 1);
    assert_eq!(site.matches("modelRuntime:").count(), 1);
    assert!(site.contains("  simulatedRuntime:\n"));
    assert!(site.contains("executionTargetId: execution-target-"));
    assert!(site.contains(&format!("siteNodeId: {node_id}\n")));
    assert!(!site.contains("modelDeployments:"));
    assert!(!site.contains("applicationDeployments:"));
    // Simulated models run on the default simulated hardware, holding its one GPU.
    assert!(
        site.contains("- GPU-00000000-0000-0000-0000-000000005090\n"),
        "{site}"
    );
    let node: dotcore_proto::dotcore::node::v1::NodeConfiguration =
        dotcore_proto::yaml::from_file(&site_dir.join("node.yaml")).expect("node input");
    assert_eq!(
        node,
        dotcore_proto::dotcore::node::v1::NodeConfiguration {
            node_id: node_id.to_owned(),
            logging: Some(dotcore_proto::dotcore::node::v1::LoggingConfig { retention_days: 7 }),
            declared_machine: Some(dotcore_proto::dotcore::node::v1::DeclaredMachine {
                choices: vec!["rtx-5090".to_owned()],
                description: Some(
                    dotcore_node_hardware::MachineDescription::simulated(
                        &["rtx-5090".to_owned()],
                        std::env::consts::ARCH,
                    )
                    .expect("simulated machine")
                    .to_stored(),
                ),
            }),
        }
    );
}

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn reset_command(site_dir: &Path) -> Command {
    reset_command_with_choices(site_dir, "simulated", "simulated")
}

fn reset_command_with_choices(site_dir: &Path, inference: &str, applications: &str) -> Command {
    let mut command = dotc();
    command
        .args(["site", "reset", "--site-dir"])
        .arg(site_dir)
        .args([
            "--inference",
            inference,
            "--applications",
            applications,
            "--site-id",
            "reset-site",
            "--node-id",
            "reset-node",
            "--host",
            "127.0.0.1",
            "--non-interactive",
        ])
        .clear_test_environment();
    command
}

/// Retained execution target bound to the simulated runtime, as a `site.yaml` list item.
const SIMULATED_MODEL_TARGET: &str = "- executionTargetId: model-target\n  siteNodeId: retained-node\n  displayName: model-target\n  modelRuntime: { simulatedRuntime: {} }\n";

/// Retained execution target bound to the Docker runtime, as a `site.yaml` list item.
const DOCKER_APPLICATION_TARGET: &str = "- executionTargetId: application-target\n  siteNodeId: retained-node\n  displayName: application-target\n  applicationRuntime: { dockerRuntime: {} }\n";

fn text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

struct TemporarySite {
    root: std::path::PathBuf,
}

impl TemporarySite {
    fn empty() -> Self {
        let root = crate::support::canonical_temp_dir().join(format!(
            "dotc-site-init-test-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("fixture root");
        Self { root }
    }

    fn new() -> Self {
        let root = crate::support::canonical_temp_dir().join(format!(
            "dotc-server-test-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let initialized = dotc()
            .args([
                "site",
                "init",
                "--site-dir",
                root.to_str().expect("fixture path"),
                "--inference",
                "simulated",
                "--applications",
                "simulated",
                "--site-id",
                "cli-test-site",
                "--node-id",
                "cli-test-node",
                "--host",
                "127.0.0.2",
                "--non-interactive",
            ])
            .clear_test_environment()
            .output()
            .expect("initialize site fixture trust");
        assert_eq!(
            initialized.status.code(),
            Some(0),
            "{}",
            text(&initialized.stderr)
        );
        // `site init` leaves its advisory flock file behind; drop it so the fixture starts unlocked.
        let _ = std::fs::remove_file(format!("{}.lock", root.display()));
        std::fs::create_dir_all(root.join("master-catalog/models")).expect("Model directory");
        std::fs::create_dir_all(root.join("master-catalog/engines")).expect("Engine directory");
        std::fs::create_dir_all(root.join("master-catalog/applications"))
            .expect("Application directory");
        std::fs::create_dir_all(root.join("cache")).expect("cache directory");
        std::fs::write(
            root.join("site.yaml"),
            "siteId: cli-test-site\nmodelDeploymentIdentityAllocator: { namespace: 0123456789abcdef, nextSequence: '1' }\nnodes:\n- nodeId: cli-test-node\n  host: 127.0.0.2\n  displayName: CLI test node\n  siteManager: {}\nexecutionTargets:\n- executionTargetId: cli-test-target\n  siteNodeId: cli-test-node\n  displayName: cli-test-target\n  platform: { operatingSystem: linux, architecture: amd64 }\n  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]\n  modelRuntime:\n    simulatedRuntime:\n      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }\n- executionTargetId: cli-test-application-target\n  siteNodeId: cli-test-node\n  displayName: cli-test-application-target\n  platform: { operatingSystem: linux, architecture: amd64 }\n  applicationRuntime: { simulatedRuntime: {} }\nmodelDeployments: []\ninferenceRoutes: []\napplicationDeployments: []\n",
        )
        .expect("desired site");
        std::fs::write(
            root.join("node.yaml"),
            format!(
                "nodeId: cli-test-node\n{}",
                std::fs::read_to_string(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../fixtures/simulated-site/node.yaml")
                )
                .expect("fixture node configuration")
                .split_once('\n')
                .expect("fixture node id line")
                .1
            ),
        )
        .expect("node configuration");
        Self { root }
    }

    fn site_path(&self) -> &std::path::Path {
        &self.root
    }

    fn catalog_path(&self) -> std::path::PathBuf {
        self.root.join("master-catalog")
    }

    fn cache_path(&self) -> std::path::PathBuf {
        self.root.join("cache")
    }
}

impl Drop for TemporarySite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn read_lines(
    stdout: impl std::io::Read + Send + 'static,
) -> (mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line.expect("server stdout line")).is_err() {
                break;
            }
        }
    });
    (receiver, reader)
}

fn read_output(
    mut input: impl Read + Send + 'static,
) -> (mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        input.read_to_end(&mut output).expect("server stderr");
        let _ = sender.send(output);
    });
    (receiver, reader)
}

fn wait_for_startup(
    child: &mut Child,
    lines: &mpsc::Receiver<String>,
    stderr: &mpsc::Receiver<Vec<u8>>,
) -> Vec<String> {
    let timeout = Duration::from_secs(5);
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("server process status") {
            let error = stderr
                .recv_timeout(Duration::from_secs(1))
                .map(|output| text(&output))
                .unwrap_or_else(|error| format!("unavailable stderr: {error}"));
            panic!(
                "server exited before startup with {status}; stdout: {output:?}; stderr: {error}"
            );
        }
        if let Ok(line) = lines.recv_timeout(Duration::from_millis(25)) {
            output.push(line);
            if has_startup_protocol(&output) {
                return output;
            }
        }
    }
    panic!("server did not emit startup protocol within {timeout:?}; stdout: {output:?}");
}

fn has_startup_protocol(output: &[String]) -> bool {
    output.len() == 3
        && output[0] == "Dotcore server starting"
        && valid_address_marker(&output[1], "DOTCORE_MANAGEMENT_ADDRESS=")
        && valid_address_marker(&output[2], "DOTCORE_INFERENCE_ADDRESS=")
}

struct StartupAddresses {
    management: std::net::SocketAddr,
    inference: std::net::SocketAddr,
}

fn startup_addresses(output: &[String]) -> StartupAddresses {
    StartupAddresses {
        management: marker_address(&output[1], "DOTCORE_MANAGEMENT_ADDRESS="),
        inference: marker_address(&output[2], "DOTCORE_INFERENCE_ADDRESS="),
    }
}

fn valid_address_marker(line: &str, marker: &str) -> bool {
    let address = marker_address(line, marker);
    address.ip().is_loopback() && address.port() != 0
}

fn marker_address(line: &str, marker: &str) -> std::net::SocketAddr {
    let Some(address) = line.strip_prefix(marker) else {
        panic!("missing {marker} marker: {line:?}");
    };
    let Some(address) = address.strip_prefix("http://") else {
        panic!("missing http scheme: {line:?}");
    };
    address
        .parse::<std::net::SocketAddr>()
        .expect("listener address")
}

fn assert_management_responds(address: std::net::SocketAddr) {
    let response = request("Management", address, list_machines_request());
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "Management response: {response:?}"
    );
    assert!(
        response
            .windows(b"cli-test-node".len())
            .any(|value| value == b"cli-test-node"),
        "Management response must include the configured node: {response:?}"
    );
}

fn assert_inference_responds(address: std::net::SocketAddr) {
    let response = request(
        "inference",
        address,
        b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "Inference response: {response:?}"
    );
}

fn request(label: &str, address: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = None;
    while Instant::now() < deadline {
        match std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(mut stream) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("response read timeout");
                if let Err(error) = std::io::Write::write_all(&mut stream, request) {
                    last_error = Some(error);
                } else {
                    let mut response = Vec::new();
                    match std::io::Read::read_to_end(&mut stream, &mut response) {
                        Ok(_) if !response.is_empty() => return response,
                        Ok(_) => last_error = Some(std::io::Error::other("empty response")),
                        Err(error) => last_error = Some(error),
                    }
                }
            }
            Err(error) => last_error = Some(error),
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("{label} listener {address} did not respond: {last_error:?}");
}

#[test]
fn hardware_recommend_prints_exact_tables_for_a_described_machine() {
    let catalog = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/recommendation-catalog");
    let fixture = TemporarySite::empty();
    let roomy = fixture.root.join("roomy.yaml");
    let tight = fixture.root.join("tight.yaml");
    for (file, memory) in [(&roomy, 34359738368_u64), (&tight, 8589934592)] {
        std::fs::write(
            file,
            format!(
                "platform: linux/amd64\ncpuCount: 8\nhostMemory: {{ totalBytes: 34359738368, idleUsedBytes: 4294967296 }}\ngpus:\n  - id: GPU-fixture\n    vendor: nvidia\n    model: Fixture GPU X\n    driverVersion: \"580.173.02\"\n    memory: {{ architecture: dedicated, totalBytes: {memory}, reservedBytes: 536870912 }}\n"
            ),
        )
        .expect("write machine description");
    }
    let recommend = |machine: &Path, extra: &[&str]| {
        let output = isolated_dotc()
            .args(["hardware", "recommend", "--machine"])
            .arg(machine)
            .arg("--catalog-dir")
            .arg(&catalog)
            .args(extra)
            .output()
            .expect("run dotc hardware recommend");
        assert!(output.status.success(), "{}", text(&output.stderr));
        assert_eq!(text(&output.stderr), "");
        text(&output.stdout)
    };

    // The model that needs 24 GB per device is listed first on the roomy GPU and dropped on the tight one.
    assert_eq!(
        recommend(&roomy, &[]),
        "Coding and agents\n  MODEL          PROFILE     TARGET         STATUS    CONTEXT TOKENS\n  Fixture Big    Fixture Q4  Fixture GPU X  fits now  174080\n  Fixture Small  Fixture Q4  Fixture GPU X  fits now  314368\n\nGeneral assistant\nNo recommendation for this machine\n\nTranslation\nNo recommendation for this machine\n\nDecision making\nNo recommendation for this machine\n"
    );
    assert_eq!(
        recommend(&tight, &["--use-case", "coding"]),
        "Coding and agents\n  MODEL          PROFILE     TARGET         STATUS    CONTEXT TOKENS\n  Fixture Small  Fixture Q4  Fixture GPU X  fits now  59392\n"
    );
}

fn list_machines_request() -> &'static [u8] {
    b"POST /dotcore.site.v1.SiteService/ListMachines HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0"
}

fn send_signal(child: &Child, signal: libc::c_int) {
    // The child process is owned by this test and its PID remains valid until wait_for_exit.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    assert_eq!(
        result,
        0,
        "send signal {signal}: {}",
        std::io::Error::last_os_error()
    );
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let timeout = Duration::from_secs(5);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("server process status") {
            return status;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("server did not exit within {timeout:?}");
}

static FIXTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
