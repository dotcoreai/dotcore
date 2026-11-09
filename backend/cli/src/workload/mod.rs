//! Lifecycle commands for a transient development workload, exposed as `dotc dev workload`.
//!
//! It launches, inspects, logs, stops, and profiles a workload owned through an
//! ownership handle, resolving launch configuration from the catalog and model
//! profile. Every action reports its result through a versioned JSON envelope so
//! the command output stays machine-readable.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use dotcore::{
    development::{
        DevelopmentAcceleratorTransport, DevelopmentLaunchProgress, DevelopmentRuntimeChoice,
        DevelopmentWorkloadProcessIdentity, DirectWorkloadFacade, DirectWorkloadFacadeConfig,
        DirectWorkloadLaunchRequest, measurement_record,
    },
    telemetry::FileTelemetrySink,
};
use dotcore_catalog::CatalogModelId;
use dotcore_domain::SiteNodeId;
use dotcore_domain::real_time;
use dotcore_node_hardware::vram::{
    VramAccounting, VramAvailability, VramCancellation, VramGpuMemory, VramObservation,
    sample_process,
};
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterValue, EngineParameterValues, ModelOptionParameterValue, ModelOptionValues,
        engine_parameter_value, model_option_parameter_value,
    },
    telemetry::v1::WorkloadResourceRecord,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::RootPathArgs;

mod teardown_all;

const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct DevCommand {
    #[command(subcommand)]
    command: Option<DevSubcommand>,
}

#[derive(Debug, Subcommand)]
enum DevSubcommand {
    /// Manage one transient development workload.
    Workload(WorkloadCommand),

    /// Work with recorded workload resource telemetry.
    Telemetry(crate::telemetry::TelemetryCommand),
}

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
struct WorkloadCommand {
    #[command(subcommand)]
    command: Option<WorkloadAction>,
}

#[derive(Debug, Subcommand)]
enum WorkloadAction {
    /// Resolve and launch a workload.
    Launch(LaunchArgs),

    /// Inspect an ownership handle and its runtime namespace.
    Inspect(InspectArgs),

    /// Read workload diagnostics.
    Logs(LogsArgs),

    /// Observe exact-process NVIDIA VRAM without mutating the workload.
    Vram(VramArgs),

    /// Stop and remove the owned workload.
    Teardown(TeardownArgs),
}

#[derive(Debug, Args, Clone)]
struct CommonArgs {
    #[arg(long, env = "DOTCORE_DEV_CATALOG_DIR", value_parser = absolute_path)]
    development_catalog: Option<PathBuf>,
    /// Flake that `self` Nix engine artifacts are evaluated and built from, for example the
    /// workspace checkout.
    #[arg(long, env = "DOTCORE_DEV_SOURCE_FLAKE")]
    development_source: Option<String>,
    #[arg(long, default_value = "local")]
    runtime_scope: String,
    /// Runtime the workload runs through; defaults to native on macOS and Docker on Linux.
    #[arg(long, value_enum)]
    runtime: Option<Runtime>,
    #[arg(long, default_value = "cdi", value_enum)]
    nvidia_transport: Transport,
}

impl CommonArgs {
    /// The selected runtime, or this platform's default.
    fn runtime(&self) -> Runtime {
        self.runtime.unwrap_or(if cfg!(target_os = "macos") {
            Runtime::Native
        } else {
            Runtime::Docker
        })
    }
}

#[derive(Debug, Args)]
struct LaunchArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    handle: HandleArgs,
    #[arg(long)]
    model_id: String,
    #[arg(long)]
    profile_id: String,
    #[arg(long)]
    model_artifact_binding_id: Option<String>,
    #[arg(long, default_value = "local-node")]
    node_id: String,
    #[arg(long, action = clap::ArgAction::Append)]
    device: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append, value_name = "ID=TYPE:VALUE")]
    engine_parameter: Vec<String>,
    #[arg(
        long,
        action = clap::ArgAction::Append,
        value_name = "OPTION_ID.PARAMETER_ID=TYPE:VALUE"
    )]
    model_option: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    custom_argument_token: Vec<String>,
}

#[derive(Debug, Args, Clone)]
struct HandleArgs {
    #[arg(long, value_parser = absolute_path)]
    handle: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct InspectArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    handle: HandleArgs,
}

#[derive(Debug, Args)]
struct TeardownArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    handle: HandleArgs,

    /// Tear down every live workload handle in the workspace, after confirmation.
    #[arg(long, conflicts_with = "handle")]
    all: bool,

    /// With --all, tear down without asking for confirmation. Required when no prompt is
    /// possible (no terminal or --non-interactive).
    #[arg(long, requires = "all")]
    yes: bool,

    /// With --all, never prompt; live workloads then require --yes.
    #[arg(long, requires = "all")]
    non_interactive: bool,
}

#[derive(Debug, Args)]
struct LogsArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    handle: HandleArgs,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=200))]
    maximum_entries: u16,
    /// Stream records until Ctrl-C, then emit one cancelled terminal event.
    #[arg(long)]
    follow: bool,
}

#[derive(Debug, Args)]
struct VramArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    handle: HandleArgs,
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=3_600_000))]
    duration_ms: u64,
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u64).range(20..=50))]
    interval_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Runtime {
    /// Docker containers with NVIDIA devices.
    Docker,

    /// Native host processes, such as Nix packages on macOS.
    Native,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Transport {
    Cdi,

    LegacyNvidia,
}

#[derive(Debug, thiserror::Error)]
#[error("workload command completed with a JSON error response")]
pub(crate) struct WorkloadCommandError {
    exit_code: ExitCode,
}

impl WorkloadCommandError {
    pub(crate) fn exit_code(&self) -> ExitCode {
        self.exit_code
    }
}

impl DevCommand {
    pub async fn dispatch(self, paths: &RootPathArgs) -> Result<()> {
        match self.command {
            Some(DevSubcommand::Workload(command)) => command.dispatch(paths).await,
            Some(DevSubcommand::Telemetry(command)) => command.dispatch(paths).await,
            None => bail!("dev command requires an action"),
        }
    }
}

impl WorkloadCommand {
    async fn dispatch(self, paths: &RootPathArgs) -> Result<()> {
        // Folder problems are argument errors of every action, reported before it starts.
        if self.command.is_some()
            && let Err(error) = paths.master_catalog().and_then(|_| paths.cache_root())
        {
            let operation = match &self.command {
                Some(WorkloadAction::Launch(_)) => "launch",
                Some(WorkloadAction::Inspect(_)) => "inspect",
                Some(WorkloadAction::Teardown(_)) => "teardown",
                Some(WorkloadAction::Logs(_)) => "logs",
                Some(WorkloadAction::Vram(_)) => "vram",
                None => "workload",
            };
            return Err(workload_error(
                operation,
                anyhow::Error::new(InvalidArguments(error)),
            ));
        }
        let (operation, result): (&str, Result<()>) = match self.command {
            Some(WorkloadAction::Launch(args)) => ("launch", launch(paths, args).await),
            Some(WorkloadAction::Inspect(args)) => ("inspect", inspect(paths, args).await),
            Some(WorkloadAction::Teardown(args)) => ("teardown", teardown(paths, args).await),
            Some(WorkloadAction::Logs(args)) => ("logs", logs(paths, args).await),
            Some(WorkloadAction::Vram(args)) => ("vram", vram(paths, args).await),
            None => (
                "workload",
                Err(anyhow::anyhow!("workload command requires an action")),
            ),
        };
        result.map_err(|error| workload_error(operation, error))
    }
}

async fn launch(paths: &RootPathArgs, args: LaunchArgs) -> Result<()> {
    let option_overrides = parse_option_overrides(args.model_option)?;
    let facade = connect(paths, &args.common).await?;
    let handle_path = resolve_handle(paths, &args.common, &args.handle)?;
    let model_id = CatalogModelId(args.model_id);
    let profile_id = args.profile_id;
    let mut configuration = facade
        .local_launch_defaults(&model_id, &profile_id)
        .context("resolve workload launch defaults")?;
    if let Some(binding_id) = args.model_artifact_binding_id {
        configuration.model_artifact_binding_id = binding_id;
    }
    merge_parameter_overrides(
        configuration
            .engine_parameter_values
            .get_or_insert_with(EngineParameterValues::default),
        args.engine_parameter
            .into_iter()
            .map(parse_parameter)
            .collect::<Result<Vec<_>>>()?,
    );
    merge_option_overrides(&mut configuration.option_overrides, option_overrides);
    configuration.custom_argument_tokens = args.custom_argument_token;
    let execution_target = dotcore_runtime::ExecutionTarget {
        managed_node_id: args
            .node_id
            .parse::<SiteNodeId>()
            .context("parse development node identity")?,
        accelerator_device_ids: accelerator_device_ids(
            args.device,
            args.common.runtime(),
            args.common.nvidia_transport,
        ),
    };
    facade
        .validate_local_launch_configuration(
            &model_id,
            &profile_id,
            &configuration,
            &execution_target,
        )
        .await
        .context("validate workload launch configuration")?;
    let request = DirectWorkloadLaunchRequest {
        handle_path: handle_path.clone(),
        owner_id: "dotc-dev-workload".to_owned(),
        model_id,
        profile_id,
        configuration,
        execution_target,
    };
    let cancellation = CancellationToken::new();
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal_cancellation = cancellation.clone();
    let signal_interrupted = interrupted.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
            signal_cancellation.cancel();
        }
    });
    let progress = DevelopmentLaunchProgress::default();
    let result = facade.launch(request, cancellation, progress.clone()).await;
    signal.abort();
    if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
        if progress.ownership_established() {
            facade
                .teardown(&handle_path)
                .await
                .context("workload launch interruption recovery failed")?;
        }
        return Err(Interrupted.into());
    }
    let facts = result.context("workload launch failed")?;
    print_success(
        "launch",
        json!({
            "handle": handle_path,
            "model_id": facts.model_id.0,
            "profile_id": facts.profile_id,
            "configuration_id": facts.configuration_id,
            "model_artifact_id": facts.model_artifact_id,
            "auxiliary_artifact_ids": facts.auxiliary_artifact_ids,
            "resolved_options": facts.resolved_options,
            "engine_parameter_values": facts.engine_parameter_values,
            "engine_release_id": facts.engine_release_id,
            "engine_artifact_id": facts.engine_artifact_id,
            "engine_version": facts.engine_version,
            "runtime_backend_id": facts.runtime_backend_id.0,
            "workload_id": facts.workload_id.0,
            "owner_id": facts.correlation.owner_id,
            "generation": facts.correlation.reconcile_generation,
            "runtime_process_id": facts.runtime_process_id,
            "process_id": facts.process_id,
            "materialized_argv": facts.materialized_argv,
            "materialized_mounts": facts.materialized_mounts.iter().map(|mount| json!({"name": mount.name, "destination": mount.destination, "read_only": mount.read_only})).collect::<Vec<_>>(),
            "loopback_endpoints": facts.loopback_endpoints.iter().map(|endpoint| json!({"port_name": endpoint.port_name, "host": endpoint.host, "host_port": endpoint.host_port})).collect::<Vec<_>>(),
            "desired_launch_plan": facts.desired_launch_plan,
        }),
    );
    Ok(())
}

async fn inspect(paths: &RootPathArgs, args: InspectArgs) -> Result<()> {
    let facade = connect(paths, &args.common).await?;
    let handle_path = resolve_handle(paths, &args.common, &args.handle)?;
    let inspection = facade
        .inspect_handle(&handle_path)
        .await
        .context("workload inspect failed")?;
    let handle = inspection.handle;
    print_success(
        "inspect",
        json!({
            "handle": handle_path,
            "handle_format_version": handle.format_version,
            "state": inspection.state,
            "runtime_scope": handle.runtime_scope,
            "runtime_backend_id": handle.runtime_backend_id,
            "owner_id": handle.owner_id,
            "generation": handle.generation,
            "launch_fingerprint": handle.launch_fingerprint,
            "workload_id": inspection.workload_id.map(|id| id.0),
            "runtime_process_id": inspection.runtime_process_id,
            "process_id": inspection.process_id,
            "dirty_reason": handle.dirty_reason,
        }),
    );
    Ok(())
}

async fn vram(paths: &RootPathArgs, args: VramArgs) -> Result<()> {
    let facade = connect(paths, &args.common).await?;
    let handle_path = resolve_handle(paths, &args.common, &args.handle)?;
    let duration = Duration::from_millis(args.duration_ms);
    let interval = Duration::from_millis(args.interval_ms);
    let cancellation = VramCancellation::default();
    let signal_cancellation = cancellation.clone();
    let custody_cancellation = CancellationToken::new();
    let signal_custody_cancellation = custody_cancellation.clone();
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal_interrupted = interrupted.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
            signal_custody_cancellation.cancel();
            signal_cancellation.cancel();
        }
    });
    let sampling_cancellation = cancellation.clone();
    let measurement = facade
        .with_live_process(
            &handle_path,
            custody_cancellation,
            move |identity| async move {
                tokio::task::spawn_blocking(move || {
                    sample_process(
                        identity.process_id,
                        duration,
                        interval,
                        &sampling_cancellation,
                    )
                })
                .await
                .context("join workload VRAM sampling task")?
            },
        )
        .await;
    signal.abort();
    if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(VramInterrupted.into());
    }
    let (identity, observation) = measurement.context("workload VRAM custody failed")?;
    let observation = observation.context("workload VRAM sampling failed")?;
    if observation.process_id != identity.process_id {
        bail!("VRAM observation process identity differs from workload custody");
    }
    let (engine_facts, engine_facts_status) =
        match facade.read_engine_startup_facts(&handle_path).await {
            Ok(Some(facts)) => (Some(facts), json!("extracted")),
            Ok(None) => (None, json!("unsupported")),
            Err(error) => (
                None,
                json!({"error": format!("{:#}", anyhow::Error::new(error))}),
            ),
        };
    let record = measurement_record(&identity, &observation, engine_facts, real_time::wall_now());
    let telemetry = record_measurement(paths.telemetry_dir(), &record, engine_facts_status);
    print_success(
        "vram",
        vram_payload(&handle_path, identity, observation, telemetry),
    );
    Ok(())
}

/// Appends the measurement record and reports where it landed, or why it was not recorded.
fn record_measurement(
    directory: Result<Option<PathBuf>>,
    record: &WorkloadResourceRecord,
    engine_facts: Value,
) -> Value {
    let reason = match directory {
        Ok(Some(directory)) => {
            match FileTelemetrySink::new(directory).append_workload_resource(record) {
                Ok(path) => {
                    return json!({
                        "status": "recorded",
                        "path": path,
                        "record_id": record.record_id,
                        "engine_facts": engine_facts,
                    });
                }
                Err(error) => format!("{:#}", anyhow::Error::new(error)),
            }
        }
        Ok(None) => format!(
            "no telemetry directory: set {} or pass --site-dir or set DOTCORE_SITE_DIR",
            dotcore::telemetry::TELEMETRY_DIR
        ),
        Err(error) => format!("{error:#}"),
    };
    json!({
        "status": "not_recorded",
        "reason": reason,
        "engine_facts": engine_facts,
    })
}

async fn teardown(paths: &RootPathArgs, args: TeardownArgs) -> Result<()> {
    if args.all {
        return teardown_all::teardown_all(paths, &args.common, args.yes, args.non_interactive)
            .await;
    }
    let facade = connect(paths, &args.common).await?;
    let handle_path = resolve_handle(paths, &args.common, &args.handle)?;
    let handle = facade
        .teardown(&handle_path)
        .await
        .context("workload teardown failed")?;
    print_success(
        "teardown",
        json!({
            "handle": handle_path,
            "handle_format_version": handle.format_version,
            "state": handle.state,
            "runtime_scope": handle.runtime_scope,
            "runtime_backend_id": handle.runtime_backend_id,
            "owner_id": handle.owner_id,
            "generation": handle.generation,
            "launch_fingerprint": handle.launch_fingerprint,
            "workload_id": handle.workload_id,
            "runtime_process_id": handle.runtime_process_id,
            "process_id": handle.process_id,
            "dirty_reason": handle.dirty_reason,
        }),
    );
    Ok(())
}

async fn logs(paths: &RootPathArgs, args: LogsArgs) -> Result<()> {
    let facade = connect(paths, &args.common).await?;
    let handle_path = resolve_handle(paths, &args.common, &args.handle)?;
    if args.follow {
        return follow_logs(facade, args, handle_path).await;
    }
    let history = facade
        .read_logs(
            &handle_path,
            std::num::NonZeroU16::new(args.maximum_entries).expect("clap enforces nonzero"),
        )
        .await
        .context("workload logs failed")?;
    write_log_entries(history.entries).context("write workload logs")?;
    if history.has_older_output || history.candidate_budget_truncated {
        eprintln!("workload logs are a bounded tail; older or budget-truncated output was omitted");
    }
    Ok(())
}

async fn follow_logs(
    facade: DirectWorkloadFacade,
    args: LogsArgs,
    handle_path: PathBuf,
) -> Result<()> {
    let cancellation = CancellationToken::new();
    let signal_cancellation = cancellation.clone();
    let signal = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal_cancellation.cancel();
    });
    let mut stream = facade
        .follow_logs(&handle_path, cancellation.clone())
        .await
        .context("workload log follow failed")?;
    let mut emitted_entries = 0_u16;
    while let Some(event) = stream.next().await {
        match event.context("workload log follow failed")? {
            dotcore_runtime::RuntimeLogEvent::Entry(entry) => {
                emitted_entries += 1;
                write_log_content(&entry.content).context("write workload logs")?;
                if emitted_entries == args.maximum_entries {
                    cancellation.cancel();
                    break;
                }
            }
            dotcore_runtime::RuntimeLogEvent::SourceUnavailable(source) => {
                eprintln!(
                    "workload log source unavailable: {} ({})",
                    source.name, source.label
                );
            }
        }
    }
    signal.abort();
    Ok(())
}

fn write_log_entries(
    entries: impl IntoIterator<Item = dotcore_runtime::RuntimeLogEntry>,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    write_log_entries_to(&mut stdout, entries)
}

fn write_log_content(content: &[u8]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    write_log_content_to(&mut stdout, content)
}

fn write_log_entries_to(
    output: &mut impl Write,
    entries: impl IntoIterator<Item = dotcore_runtime::RuntimeLogEntry>,
) -> Result<()> {
    for entry in entries {
        write_log_content_to(output, &entry.content)?;
    }
    output.flush().context("flush workload logs")
}

fn write_log_content_to(output: &mut impl Write, content: &[u8]) -> Result<()> {
    output
        .write_all(content)
        .context("write workload log bytes")
}

fn resolve_handle(paths: &RootPathArgs, args: &CommonArgs, handle: &HandleArgs) -> Result<PathBuf> {
    if let Some(handle) = &handle.handle {
        return Ok(handle.clone());
    }
    let handle_root = handle_root(&paths.master_catalog()?)?;
    std::fs::create_dir_all(&handle_root).context("create workspace workload handle directory")?;
    Ok(handle_root.join(format!("{}.json", args.runtime_scope)))
}

/// Directory holding the workspace's development workload handles, next to the master catalog.
fn handle_root(master_catalog: &Path) -> Result<PathBuf> {
    let workspace_root = master_catalog
        .parent()
        .context("the master catalog path must identify a workspace catalog directory")?;
    Ok(workspace_root.join(".scratch/development-workloads"))
}

async fn connect(paths: &RootPathArgs, args: &CommonArgs) -> Result<DirectWorkloadFacade> {
    DirectWorkloadFacade::connect(DirectWorkloadFacadeConfig {
        master_catalog_root: paths.master_catalog()?,
        development_catalog_root: args.development_catalog.clone(),
        development_source: args.development_source.clone(),
        cache_root: paths.cache_root()?,
        runtime_scope: args.runtime_scope.clone(),
        runtime: match args.runtime() {
            Runtime::Docker => DevelopmentRuntimeChoice::Docker {
                accelerator_transport: match args.nvidia_transport {
                    Transport::Cdi => DevelopmentAcceleratorTransport::Cdi,
                    Transport::LegacyNvidia => DevelopmentAcceleratorTransport::LegacyNvidia,
                },
            },
            Runtime::Native => DevelopmentRuntimeChoice::Native { launcher: None },
        },
    })
    .await
    .map_err(|error| anyhow::anyhow!(error))
}

fn vram_payload(
    handle_path: &Path,
    identity: DevelopmentWorkloadProcessIdentity,
    observation: VramObservation,
    telemetry: Value,
) -> Value {
    let processes = observation
        .processes
        .into_iter()
        .map(|process| {
            json!({
                "process_id": process.process_id,
                "start_token": process.start_token,
                "accounting": match process.accounting {
                    VramAvailability::Available { value } => accounting_payload(value),
                    VramAvailability::Unavailable { reason } => {
                        json!({"status": "unavailable", "reason": reason})
                    }
                },
            })
        })
        .collect::<Vec<_>>();
    let other_processes = observation
        .other_processes
        .into_iter()
        .map(|process| {
            json!({
                "process_id": process.process_id,
                "memory_known": process.memory_known,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "handle": handle_path,
        "runtime_scope": identity.runtime_scope,
        "runtime_backend_id": identity.runtime_backend_id,
        "owner_id": identity.owner_id,
        "generation": identity.generation,
        "launch_fingerprint": identity.launch_fingerprint,
        "workload_id": identity.workload_id.0,
        "runtime_process_id": identity.runtime_process_id,
        "process_id": identity.process_id,
        "process_start_token": observation.process_start_token,
        "started_unix_millis": observation.started_unix_millis,
        "duration_millis": observation.duration_millis,
        "interval_millis": observation.interval_millis,
        "gpu": {
            "index": observation.gpu.index,
            "uuid": observation.gpu.uuid,
            "name": observation.gpu.name,
            "driver_version": observation.gpu.driver_version,
            "cuda_driver_version": observation.gpu.cuda_driver_version,
            "memory": gpu_memory_payload(observation.gpu.memory),
        },
        "isolated": observation.isolated,
        "samples": observation.samples.into_iter().map(|sample| json!({
            "elapsed_millis": sample.elapsed_millis,
            "used_bytes": sample.used_bytes,
            "unattributed_bytes": sample.unattributed_bytes,
        })).collect::<Vec<_>>(),
        "observed_max_bytes": observation.observed_max_bytes,
        "unattributed_max_bytes": observation.unattributed_max_bytes,
        "processes": processes,
        "other_processes": other_processes,
        "telemetry": telemetry,
    })
}

fn gpu_memory_payload(memory: VramGpuMemory) -> Value {
    match memory {
        VramGpuMemory::Dedicated { total_bytes } => json!({
            "architecture": "dedicated",
            "total_bytes": total_bytes,
        }),
        VramGpuMemory::Unified {
            host_total_bytes,
            host_available_bytes,
        } => json!({
            "architecture": "unified",
            "host_total_bytes": host_total_bytes,
            "host_available_bytes": host_available_bytes,
        }),
    }
}

fn accounting_payload(accounting: VramAccounting) -> Value {
    json!({
        "status": "available",
        "value": {
            "max_memory_usage_bytes": accounting.max_memory_usage_bytes,
            "process_start_unix_micros": accounting.process_start_unix_micros,
            "is_running": accounting.is_running,
        }
    })
}

fn print_success(operation: &str, payload: Value) {
    println!(
        "{}",
        json!({"format_version": FORMAT_VERSION, "operation": operation, "result": "success", "payload": payload})
    );
}

fn workload_error(operation: &str, error: anyhow::Error) -> anyhow::Error {
    let interrupted = error.downcast_ref::<Interrupted>().is_some()
        || error.downcast_ref::<VramInterrupted>().is_some()
        || error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<crate::terminal::PromptError>(),
                Some(crate::terminal::PromptError::Cancelled)
            )
        });
    let invalid_arguments = error.downcast_ref::<InvalidArguments>().is_some();
    eprintln!("{error:#}");
    if operation != "logs" || invalid_arguments {
        let code = if invalid_arguments {
            "invalid_arguments"
        } else if interrupted {
            "interrupted"
        } else {
            "operation_failed"
        };
        println!(
            "{}",
            json!({"format_version": FORMAT_VERSION, "operation": operation, "result": "error", "error": {"code": code, "message": error.to_string()}})
        );
    }
    WorkloadCommandError {
        exit_code: ExitCode::from(if invalid_arguments {
            2
        } else if interrupted {
            130
        } else {
            1
        }),
    }
    .into()
}

/// A folder option or variable that is missing or unusable for a workload action.
#[derive(Debug, thiserror::Error)]
#[error("{0:#}")]
struct InvalidArguments(anyhow::Error);

#[derive(Debug, thiserror::Error)]
#[error("workload launch interrupted")]
struct Interrupted;

#[derive(Debug, thiserror::Error)]
#[error("workload VRAM observation interrupted")]
struct VramInterrupted;

fn accelerator_device_ids(
    explicit: Vec<String>,
    runtime: Runtime,
    transport: Transport,
) -> Vec<String> {
    if !explicit.is_empty() {
        return explicit;
    }

    vec![
        match (runtime, transport) {
            (Runtime::Native, _) => dotcore_domain::APPLE_GPU_DEVICE_ID,
            (Runtime::Docker, Transport::Cdi) => "nvidia.com/gpu=0",
            (Runtime::Docker, Transport::LegacyNvidia) => "0",
        }
        .to_owned(),
    ]
}

fn merge_parameter_overrides(
    defaults: &mut EngineParameterValues,
    overrides: Vec<EngineParameterValue>,
) {
    for parameter in overrides {
        match defaults
            .values
            .iter_mut()
            .find(|value| value.parameter_id == parameter.parameter_id)
        {
            Some(default) => *default = parameter,
            None => defaults.values.push(parameter),
        }
    }
}

fn parse_option_overrides(values: Vec<String>) -> Result<Vec<ModelOptionValues>> {
    let mut options = BTreeMap::<String, BTreeMap<String, ModelOptionParameterValue>>::new();
    for value in values {
        let (option_id, parameter) = parse_option_parameter(value)?;
        let parameter_id = parameter.parameter_id.clone();
        if options
            .entry(option_id.clone())
            .or_default()
            .insert(parameter_id.clone(), parameter)
            .is_some()
        {
            bail!("duplicate model option parameter: {option_id}.{parameter_id}");
        }
    }
    Ok(options
        .into_iter()
        .map(|(option_id, values)| ModelOptionValues {
            option_id,
            values: values.into_values().collect(),
        })
        .collect())
}

fn parse_option_parameter(value: String) -> Result<(String, ModelOptionParameterValue)> {
    let (path, typed) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("model option must be OPTION_ID.PARAMETER_ID=TYPE:VALUE"))?;
    let (option_id, parameter_id) = path
        .split_once('.')
        .filter(|(option_id, parameter_id)| !option_id.is_empty() && !parameter_id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("model option must be OPTION_ID.PARAMETER_ID=TYPE:VALUE"))?;
    let (kind, raw) = typed
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("model option must be OPTION_ID.PARAMETER_ID=TYPE:VALUE"))?;
    let value = match kind {
        "boolean" => model_option_parameter_value::Value::Boolean(
            raw.parse().context("invalid boolean model option")?,
        ),
        "unsigned" => model_option_parameter_value::Value::UnsignedInteger(
            raw.parse().context("invalid unsigned model option")?,
        ),
        "string" => model_option_parameter_value::Value::String(raw.to_owned()),
        _ => bail!("unsupported model option type: {kind}"),
    };
    Ok((
        option_id.to_owned(),
        ModelOptionParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(value),
        },
    ))
}

fn merge_option_overrides(
    defaults: &mut Vec<ModelOptionValues>,
    overrides: Vec<ModelOptionValues>,
) {
    for mut option in overrides {
        match defaults
            .iter_mut()
            .find(|default| default.option_id == option.option_id)
        {
            Some(default) => {
                for parameter in option.values.drain(..) {
                    match default
                        .values
                        .iter_mut()
                        .find(|value| value.parameter_id == parameter.parameter_id)
                    {
                        Some(value) => *value = parameter,
                        None => default.values.push(parameter),
                    }
                }
            }
            None => defaults.push(option),
        }
    }
}

fn parse_parameter(value: String) -> Result<EngineParameterValue> {
    let (id, typed) = value
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("engine parameter must be ID=TYPE:VALUE"))?;
    let (kind, raw) = typed
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("engine parameter must be ID=TYPE:VALUE"))?;
    let value = match kind {
        "unsigned" => engine_parameter_value::Value::UnsignedInteger(
            raw.parse().context("invalid unsigned parameter")?,
        ),
        _ => bail!("unsupported engine parameter type: {kind}"),
    };
    Ok(EngineParameterValue {
        parameter_id: id.to_owned(),
        value: Some(value),
    })
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("path must be absolute, normalized, and name a file".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use dotcore::development::DevelopmentWorkloadProcessIdentity;
    use dotcore_node_hardware::vram::{
        VramAccounting, VramAvailability, VramGpuIdentity, VramGpuMemory, VramObservation,
        VramOtherProcess, VramProcess, VramSample,
    };
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineParameterValue, EngineParameterValues, ModelOptionParameterValue,
            ModelOptionValues, engine_parameter_value, model_option_parameter_value,
        },
        telemetry::v1::WorkloadResourceRecord,
    };
    use dotcore_runtime::WorkloadId;

    use super::{
        CommonArgs, DevSubcommand, HandleArgs, Runtime, Transport, WorkloadAction,
        accelerator_device_ids, accounting_payload, gpu_memory_payload, handle_root,
        merge_option_overrides, merge_parameter_overrides, parse_option_overrides,
        record_measurement, resolve_handle, vram_payload, write_log_content_to,
    };
    use crate::RootPathArgs;

    #[test]
    fn unified_gpu_memory_payload_reports_shared_host_memory() {
        assert_eq!(
            gpu_memory_payload(VramGpuMemory::Unified {
                host_total_bytes: 130_000_000_000,
                host_available_bytes: 90_000_000_000,
            }),
            serde_json::json!({
                "architecture": "unified",
                "host_total_bytes": 130_000_000_000_u64,
                "host_available_bytes": 90_000_000_000_u64,
            })
        );
    }

    #[test]
    fn minimal_launch_uses_profile_defaults_and_internal_lifecycle_values() {
        let cli = crate::Cli::try_parse_from([
            "dotc",
            "dev",
            "workload",
            "launch",
            "--catalog-dir",
            "/catalog",
            "--cache-dir",
            "/cache",
            "--model-id",
            "model-one",
            "--profile-id",
            "profile-one",
        ])
        .expect("minimal direct workload launch");
        let crate::Command::Dev(command) = cli.command else {
            panic!("expected development command");
        };
        let Some(DevSubcommand::Workload(command)) = command.command else {
            panic!("expected workload command");
        };
        let Some(WorkloadAction::Launch(args)) = command.command else {
            panic!("expected launch action");
        };

        assert_eq!(args.model_id, "model-one");
        assert_eq!(args.profile_id, "profile-one");
        assert_eq!(args.model_artifact_binding_id, None);
        assert!(args.device.is_empty());
        assert!(args.model_option.is_empty());
        assert_eq!(args.handle.handle, None);
        assert_eq!(args.common.development_catalog, None);
    }

    #[test]
    fn model_option_arguments_preserve_typed_values_and_reject_duplicates() {
        let parsed = parse_option_overrides(vec![
            "vision.enabled=boolean:false".to_owned(),
            "vision.token-budget=unsigned:0".to_owned(),
            "vision.projector=string:bf16".to_owned(),
        ])
        .expect("typed option overrides");
        assert_eq!(
            parsed,
            vec![ModelOptionValues {
                option_id: "vision".to_owned(),
                values: vec![
                    ModelOptionParameterValue {
                        parameter_id: "enabled".to_owned(),
                        value: Some(model_option_parameter_value::Value::Boolean(false)),
                    },
                    ModelOptionParameterValue {
                        parameter_id: "projector".to_owned(),
                        value: Some(model_option_parameter_value::Value::String(
                            "bf16".to_owned()
                        )),
                    },
                    ModelOptionParameterValue {
                        parameter_id: "token-budget".to_owned(),
                        value: Some(model_option_parameter_value::Value::UnsignedInteger(0)),
                    },
                ],
            }]
        );
        assert_eq!(
            parse_option_overrides(vec![
                "vision.enabled=boolean:true".to_owned(),
                "vision.enabled=boolean:false".to_owned(),
            ])
            .expect_err("duplicate parameter")
            .to_string(),
            "duplicate model option parameter: vision.enabled"
        );
        assert!(
            parse_option_overrides(vec!["vision.enabled=boolean:not-a-boolean".to_owned()])
                .expect_err("wrong value type")
                .to_string()
                .contains("invalid boolean model option")
        );
        assert_eq!(
            parse_option_overrides(vec!["vision.enabled=object:true".to_owned()])
                .expect_err("unknown value type")
                .to_string(),
            "unsupported model option type: object"
        );
    }

    #[test]
    fn model_option_overrides_replace_profile_values_without_dropping_siblings() {
        let mut defaults = vec![ModelOptionValues {
            option_id: "vision".to_owned(),
            values: vec![
                ModelOptionParameterValue {
                    parameter_id: "enabled".to_owned(),
                    value: Some(model_option_parameter_value::Value::Boolean(true)),
                },
                ModelOptionParameterValue {
                    parameter_id: "token-budget".to_owned(),
                    value: Some(model_option_parameter_value::Value::UnsignedInteger(1_024)),
                },
            ],
        }];
        merge_option_overrides(
            &mut defaults,
            parse_option_overrides(vec!["vision.enabled=boolean:false".to_owned()])
                .expect("override"),
        );

        assert_eq!(
            defaults,
            vec![ModelOptionValues {
                option_id: "vision".to_owned(),
                values: vec![
                    ModelOptionParameterValue {
                        parameter_id: "enabled".to_owned(),
                        value: Some(model_option_parameter_value::Value::Boolean(false)),
                    },
                    ModelOptionParameterValue {
                        parameter_id: "token-budget".to_owned(),
                        value: Some(model_option_parameter_value::Value::UnsignedInteger(1_024)),
                    },
                ],
            }]
        );
    }

    #[test]
    fn vram_uses_bounded_defaults_and_the_normal_handle() {
        let cli = crate::Cli::try_parse_from([
            "dotc",
            "dev",
            "workload",
            "vram",
            "--catalog-dir",
            "/catalog",
            "--cache-dir",
            "/cache",
        ])
        .expect("default VRAM observation");
        let crate::Command::Dev(command) = cli.command else {
            panic!("expected development command");
        };
        let Some(DevSubcommand::Workload(command)) = command.command else {
            panic!("expected workload command");
        };
        let Some(WorkloadAction::Vram(args)) = command.command else {
            panic!("expected VRAM action");
        };

        assert_eq!(args.handle.handle, None);
        assert_eq!(args.duration_ms, 10_000);
        assert_eq!(args.interval_ms, 25);
    }

    #[test]
    fn vram_accepts_explicit_handle_and_sampling_window() {
        let cli = crate::Cli::try_parse_from([
            "dotc",
            "dev",
            "workload",
            "vram",
            "--catalog-dir",
            "/catalog",
            "--cache-dir",
            "/cache",
            "--handle",
            "/tmp/workload.json",
            "--duration-ms",
            "1234",
            "--interval-ms",
            "30",
        ])
        .expect("explicit VRAM observation");
        let crate::Command::Dev(command) = cli.command else {
            panic!("expected development command");
        };
        let Some(DevSubcommand::Workload(command)) = command.command else {
            panic!("expected workload command");
        };
        let Some(WorkloadAction::Vram(args)) = command.command else {
            panic!("expected VRAM action");
        };

        assert_eq!(
            args.handle.handle,
            Some(PathBuf::from("/tmp/workload.json"))
        );
        assert_eq!(args.duration_ms, 1_234);
        assert_eq!(args.interval_ms, 30);
    }

    #[test]
    fn vram_payload_serializes_exact_custody_and_observation() {
        let payload = vram_payload(
            std::path::Path::new("/workspace/.scratch/development-workloads/local.json"),
            DevelopmentWorkloadProcessIdentity {
                runtime_scope: "development-scope".to_owned(),
                runtime_backend_id: "runtime/docker".to_owned(),
                owner_id: "runner-owner".to_owned(),
                generation: 7,
                launch_fingerprint: "a".repeat(64),
                workload_id: WorkloadId("dotcore_workload".to_owned()),
                runtime_process_id: "sha256:container".to_owned(),
                process_id: 4242,
                launch_inputs: Default::default(),
            },
            VramObservation {
                process_id: 4242,
                process_start_token: 700,
                started_unix_millis: 1_750_000_000_000,
                duration_millis: 10_015,
                interval_millis: 25,
                gpu: VramGpuIdentity {
                    index: 0,
                    uuid: "GPU-fixture".to_owned(),
                    name: "Fixture GPU".to_owned(),
                    driver_version: "595.71.05".to_owned(),
                    cuda_driver_version: 13_020,
                    memory: VramGpuMemory::Dedicated {
                        total_bytes: 34_179_219_456,
                    },
                },
                isolated: false,
                samples: vec![
                    VramSample {
                        elapsed_millis: 0,
                        used_bytes: 3_000,
                        unattributed_bytes: 900,
                    },
                    VramSample {
                        elapsed_millis: 25,
                        used_bytes: 3_500,
                        unattributed_bytes: 1_200,
                    },
                ],
                observed_max_bytes: 3_500,
                unattributed_max_bytes: 1_200,
                other_processes: vec![
                    VramOtherProcess {
                        process_id: 98,
                        memory_known: false,
                    },
                    VramOtherProcess {
                        process_id: 99,
                        memory_known: true,
                    },
                ],
                processes: vec![
                    VramProcess {
                        process_id: 4242,
                        start_token: 700,
                        accounting: VramAvailability::Unavailable {
                            reason: "NVML accounting is disabled".to_owned(),
                        },
                    },
                    VramProcess {
                        process_id: 4300,
                        start_token: 760,
                        accounting: VramAvailability::Available {
                            value: VramAccounting {
                                max_memory_usage_bytes: Some(3_072),
                                process_start_unix_micros: 1_750_000_000_600_000,
                                is_running: true,
                            },
                        },
                    },
                ],
            },
            serde_json::json!({"status": "not_recorded"}),
        );

        assert_eq!(
            payload,
            serde_json::json!({
                "handle": "/workspace/.scratch/development-workloads/local.json",
                "runtime_scope": "development-scope",
                "runtime_backend_id": "runtime/docker",
                "owner_id": "runner-owner",
                "generation": 7,
                "launch_fingerprint": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "workload_id": "dotcore_workload",
                "runtime_process_id": "sha256:container",
                "process_id": 4242,
                "process_start_token": 700,
                "started_unix_millis": 1_750_000_000_000_u64,
                "duration_millis": 10_015,
                "interval_millis": 25,
                "gpu": {
                    "index": 0,
                    "uuid": "GPU-fixture",
                    "name": "Fixture GPU",
                    "driver_version": "595.71.05",
                    "cuda_driver_version": 13_020,
                    "memory": {
                        "architecture": "dedicated",
                        "total_bytes": 34_179_219_456_u64
                    }
                },
                "isolated": false,
                "samples": [
                    {"elapsed_millis": 0, "used_bytes": 3_000, "unattributed_bytes": 900},
                    {"elapsed_millis": 25, "used_bytes": 3_500, "unattributed_bytes": 1_200}
                ],
                "observed_max_bytes": 3_500,
                "unattributed_max_bytes": 1_200,
                "processes": [
                    {
                        "process_id": 4242,
                        "start_token": 700,
                        "accounting": {
                            "status": "unavailable",
                            "reason": "NVML accounting is disabled"
                        }
                    },
                    {
                        "process_id": 4300,
                        "start_token": 760,
                        "accounting": {
                            "status": "available",
                            "value": {
                                "max_memory_usage_bytes": 3_072,
                                "process_start_unix_micros": 1_750_000_000_600_000_u64,
                                "is_running": true
                            }
                        }
                    }
                ],
                "other_processes": [
                    {"process_id": 98, "memory_known": false},
                    {"process_id": 99, "memory_known": true}
                ],
                "telemetry": {"status": "not_recorded"}
            })
        );
    }

    #[test]
    fn vram_telemetry_reports_recorded_path_or_the_reason_it_was_not_recorded() {
        let directory = crate::test_support::canonical_tempdir().expect("telemetry directory");
        let record = WorkloadResourceRecord {
            record_id: "record-1".to_owned(),
            recorded_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 1_790_298_000,
                nanos: 0,
            }),
            ..WorkloadResourceRecord::default()
        };

        let recorded = record_measurement(
            Ok(Some(directory.path().to_path_buf())),
            &record,
            serde_json::json!("extracted"),
        );
        let unconfigured = record_measurement(Ok(None), &record, serde_json::json!("unsupported"));

        let path = directory
            .path()
            .join("workload-resources")
            .join("2026-09-25.jsonl");
        assert_eq!(
            recorded,
            serde_json::json!({
                "status": "recorded",
                "path": path,
                "record_id": "record-1",
                "engine_facts": "extracted"
            })
        );
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("record file")
                .lines()
                .count(),
            1
        );
        assert_eq!(
            unconfigured,
            serde_json::json!({
                "status": "not_recorded",
                "reason": "no telemetry directory: set DOTCORE_TELEMETRY_DIR or pass --site-dir or set DOTCORE_SITE_DIR",
                "engine_facts": "unsupported"
            })
        );
    }

    #[test]
    fn vram_payload_preserves_available_accounting_evidence() {
        assert_eq!(
            accounting_payload(VramAccounting {
                max_memory_usage_bytes: Some(4_096),
                process_start_unix_micros: 1_750_000_000_000_000,
                is_running: true,
            }),
            serde_json::json!({
                "status": "available",
                "value": {
                    "max_memory_usage_bytes": 4_096,
                    "process_start_unix_micros": 1_750_000_000_000_000_u64,
                    "is_running": true
                }
            })
        );
    }

    #[test]
    fn raw_log_output_preserves_bytes_without_text_conversion() {
        let mut output = Vec::new();

        write_log_content_to(&mut output, &[0, b'\n', 255]).expect("write raw log bytes");

        assert_eq!(output, [0, b'\n', 255]);
    }

    #[test]
    fn default_handle_is_workspace_local_and_separate_from_model_sizing_output() {
        let workspace_root = crate::test_support::canonical_temp_dir().join(format!(
            "dotcore-workload-handle-test-{}",
            std::process::id()
        ));
        let catalog_root = workspace_root.join("catalog");
        std::fs::create_dir_all(&catalog_root).expect("catalog root");
        let paths = RootPathArgs {
            catalog_dir: Some(catalog_root),
            cache_dir: Some(workspace_root.join("cache")),
            ..RootPathArgs::default()
        };
        let args = common_args();

        let handle = resolve_handle(&paths, &args, &HandleArgs { handle: None })
            .expect("workspace-local default handle");

        assert_eq!(
            handle,
            workspace_root.join(".scratch/development-workloads/local.json")
        );
        assert!(!handle.starts_with(workspace_root.join(".scratch/model-sizing")));
        std::fs::remove_dir_all(workspace_root).expect("test workspace cleanup");
    }

    #[test]
    fn explicit_handle_path_is_preserved() {
        let explicit = PathBuf::from("/tmp/explicit-workload-handle.json");
        let paths = RootPathArgs {
            catalog_dir: Some(PathBuf::from("/workspace/catalog")),
            ..RootPathArgs::default()
        };
        let args = common_args();

        assert_eq!(
            resolve_handle(
                &paths,
                &args,
                &HandleArgs {
                    handle: Some(explicit.clone()),
                }
            )
            .expect("explicit handle"),
            explicit
        );
    }

    #[test]
    fn implicit_accelerator_device_matches_transport() {
        assert_eq!(
            accelerator_device_ids(Vec::new(), Runtime::Docker, Transport::Cdi),
            ["nvidia.com/gpu=0"]
        );
        assert_eq!(
            accelerator_device_ids(Vec::new(), Runtime::Docker, Transport::LegacyNvidia),
            ["0"]
        );
    }

    #[test]
    fn explicit_accelerator_devices_are_preserved() {
        let explicit = vec!["nvidia.com/gpu=gpu1".to_owned()];

        assert_eq!(
            accelerator_device_ids(explicit.clone(), Runtime::Docker, Transport::Cdi),
            explicit
        );
    }

    #[test]
    fn native_launches_default_to_the_apple_gpu_and_the_platform_picks_the_runtime() {
        assert_eq!(
            accelerator_device_ids(Vec::new(), Runtime::Native, Transport::Cdi),
            ["apple-gpu-0"]
        );
        let expected = if cfg!(target_os = "macos") {
            Runtime::Native
        } else {
            Runtime::Docker
        };
        assert_eq!(common_args().runtime(), expected);
        let explicit = CommonArgs {
            runtime: Some(Runtime::Native),
            ..common_args()
        };
        assert_eq!(explicit.runtime(), Runtime::Native);
    }

    #[test]
    fn explicit_parameter_values_override_profile_defaults() {
        let mut values = EngineParameterValues {
            values: vec![unsigned_parameter("total-context", 30_000)],
        };

        merge_parameter_overrides(
            &mut values,
            vec![
                unsigned_parameter("total-context", 8_192),
                unsigned_parameter("new-parameter", 7),
            ],
        );

        assert_eq!(
            values.values,
            vec![
                unsigned_parameter("total-context", 8_192),
                unsigned_parameter("new-parameter", 7),
            ]
        );
    }

    #[test]
    fn a_missing_master_catalog_names_the_option_and_variable() {
        let paths = RootPathArgs::default();

        let error = resolve_handle(&paths, &common_args(), &HandleArgs { handle: None })
            .expect_err("no catalog");

        assert_eq!(
            error.to_string(),
            "no master catalog: pass --catalog-dir or set DOTCORE_CATALOG_DIR to its folder"
        );
    }

    #[test]
    fn the_handle_directory_sits_beside_the_master_catalog() {
        assert_eq!(
            handle_root(Path::new("/workspace/catalog")).expect("handle root"),
            PathBuf::from("/workspace/.scratch/development-workloads")
        );
        assert_eq!(
            handle_root(Path::new("/"))
                .expect_err("no parent")
                .to_string(),
            "the master catalog path must identify a workspace catalog directory"
        );
    }

    fn common_args() -> CommonArgs {
        CommonArgs {
            development_catalog: None,
            development_source: None,
            runtime_scope: "local".to_owned(),
            runtime: None,
            nvidia_transport: Transport::Cdi,
        }
    }

    fn unsigned_parameter(id: &str, value: u64) -> EngineParameterValue {
        EngineParameterValue {
            parameter_id: id.to_owned(),
            value: Some(engine_parameter_value::Value::UnsignedInteger(value)),
        }
    }
}
