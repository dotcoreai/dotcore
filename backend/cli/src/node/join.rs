//! `dotc node join`: turns this machine into a node of an existing site by reaching the
//! manager's join listener with the one-time token from `dotc node add`.
//!
//! The command probes this machine, proposes a name, address, and runtime capabilities, and
//! asks the operator to confirm before anything is sent. The node's private key is created
//! here and never leaves; the finished node folder is what `dotcore` runs, and on a terminal the
//! command then starts it, after a countdown the operator can skip. A node whose
//! intact folder of the same site already exists joins again instead, with no probe or prompt,
//! keeping its key, identity, and deployments and changing only its addresses. Asked for YAML,
//! the command never asks, so it needs `--yes`, reports its progress on stderr, prints one
//! document once the folder is installed, and never starts the node.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fmt,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::Context;
use clap::Args;
use dotcore::composition::DEFAULT_PRIVATE_PORT;
use dotcore_domain::{
    SiteNodeHost, SystemHostResolver,
    timing::{JoinTiming, Timing},
};
use dotcore_site::{
    initial::{
        inputs::NodeTargetChoices,
        probe::InitialSiteProber,
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
            derive_site_node_id,
        },
    },
    membership::join::{
        JoinConnection, JoinProposal, JoinToken, JoinedNodeFolder, NodeJoinError, NodeKey,
        RejoinProposal, RejoiningNodeFolder,
    },
    node::{
        folder::{CertificateFingerprint, WORKER_MANIFEST_FILE},
        hostname::local_node_name,
    },
};

use serde::Serialize;

use super::{Notices, boxed, fingerprint_facts, fingerprint_grid, lend, progress, write_stdout};
use crate::{
    RootPathArgs,
    failure::shell_word,
    guided::{
        CapabilityRecommendations, InferencePick, collect_applications, collect_inference,
        in_dev_env,
    },
    output::{OutputFormat, plain_text, require_consent, write_yaml},
    paths::find_executable,
    site::{
        ApplicationsChoice, InferenceArgument, InferenceChoice, NativeWorkloadLifetimeChoice,
        inference_long_help, loopback_host_warning, parse_absolute_path, parse_display_name,
        parse_host, parse_inference, system_initial_site_prober,
    },
    terminal::{ConfirmPrompt, CountdownPrompt, InputPrompt, Message, Terminal, open_terminal},
};

/// Shown after the proposal is sent, while the manager operator decides.
const APPROVAL_NOTICE: &str = "Waiting for approval on the manager...";

/// The launcher that starts a joined node, as found on the search path and shown to the operator.
const LAUNCHER: &str = "dotcore";

/// Hint shown when the manager cannot be reached at the address tried.
const MANAGER_HOST_HINT: &str =
    "pass --manager-host <address or host name this machine reaches the manager at>";

/// Hint shown when the manager reached closes the connection before admitting the machine.
const CONNECTION_CLOSED_HINT: &str = "the token may have expired or already been used; get a new \
     command from the console's Add machine or `dotc node add`, and keep it open on the manager \
     until this machine joins";

/// Arguments of `dotc node join`. The token comes from `--token`, `--token-file`, or a hidden
/// prompt.
#[derive(Debug, Args)]
pub struct NodeJoinArgs {
    /// Join token printed by `dotc node add`; otherwise --token-file, otherwise a hidden
    /// prompt. It is visible to other users on this machine while the command runs.
    #[arg(long, value_name = "TOKEN", conflicts_with = "token_file")]
    pub token: Option<String>,

    /// File holding the join token; otherwise --token, otherwise a hidden prompt.
    #[arg(long, value_name = "PATH", value_parser = parse_absolute_path)]
    pub token_file: Option<PathBuf>,

    /// Node display name; defaults to this machine's hostname without a trailing `.local`. When
    /// joining again, it must be the folder's node.
    #[arg(long, value_name = "NAME", value_parser = parse_display_name)]
    pub name: Option<String>,

    /// IPv4 address or DNS host name of the manager as seen from this machine; replaces the
    /// host the token names, for a machine on another network than the manager's declared host.
    #[arg(long, value_name = "HOST", value_parser = parse_host)]
    pub manager_host: Option<SiteNodeHost>,

    /// IPv4 address or DNS host name the manager reaches this node at; defaults to
    /// DOTCORE_MACHINE_HOST, else the address the manager observes. The manager connects back to
    /// it on the private port while the node joins, so forward that port first when behind NAT.
    #[arg(long, env = "DOTCORE_MACHINE_HOST", value_parser = parse_host)]
    pub host: Option<SiteNodeHost>,

    /// Port of this node's private listener; the manager connects back to it while the node
    /// joins.
    #[arg(long, value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub private_port: Option<u16>,

    /// Inference capability the node offers: simulated, nvidia, apple or none; repeat to offer
    /// several. `simulated:<name>` simulates hardware in place of the real GPUs; repeat it to put
    /// several GPUs on one simulated host.
    #[arg(
        long,
        value_name = "INFERENCE",
        value_parser = parse_inference,
        long_help = inference_long_help(),
        action = clap::ArgAction::Append
    )]
    pub inference: Vec<InferenceArgument>,

    /// Applications capability the node offers; repeat to offer several, or pass none for a node
    /// that runs no applications.
    #[arg(long, value_enum, action = clap::ArgAction::Append)]
    pub applications: Vec<ApplicationsChoice>,

    /// Advanced NVIDIA CDI selector override, such as nvidia.com/gpu=gpu1; declares one model
    /// target per selector, each naming one GPU. Repeat once per selector.
    #[arg(long, value_name = "CDI_SELECTOR", action = clap::ArgAction::Append)]
    pub nvidia_device: Vec<String>,

    /// Whether native workloads outlive the server process; only with --inference apple.
    /// Defaults to persistent.
    #[arg(long, value_enum)]
    pub native_workload_lifetime: Option<NativeWorkloadLifetimeChoice>,

    /// Join without asking for confirmation; needs --inference and --applications.
    #[arg(long)]
    pub yes: bool,
}

impl NodeJoinArgs {
    pub(super) fn argument_contract_error(&self) -> Option<&'static str> {
        if !self.nvidia_device.is_empty()
            && !self.inference.is_empty()
            && !InferenceArgument::offers(&self.inference, InferenceChoice::Nvidia)
        {
            return Some("--nvidia-device requires --inference nvidia");
        }
        if self.native_workload_lifetime.is_some()
            && !self.inference.is_empty()
            && !InferenceArgument::offers(&self.inference, InferenceChoice::Apple)
        {
            return Some("--native-workload-lifetime requires --inference apple");
        }
        if InferenceArgument::offers(&self.inference, InferenceChoice::None)
            && self
                .inference
                .iter()
                .any(|argument| argument.choice != InferenceChoice::None)
        {
            return Some("--inference none cannot be combined with another --inference value");
        }
        if self.applications.contains(&ApplicationsChoice::None)
            && self
                .applications
                .iter()
                .any(|choice| *choice != ApplicationsChoice::None)
        {
            return Some(
                "--applications none cannot be combined with another --applications value",
            );
        }
        None
    }
}

/// Joins this machine to the site named by the token and installs its node folder.
pub(crate) async fn node_join(
    paths: &RootPathArgs,
    arguments: NodeJoinArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    require_consent(
        arguments.yes,
        format,
        "joining asks for confirmation and needs a terminal; run in a terminal or pass --yes",
        "joining asks for confirmation, which --yaml never gives; pass --yes",
    )?;
    let mut terminal = (!arguments.yes).then(open_terminal);
    let prober = system_initial_site_prober();
    let timing = Timing::for_process().join;
    let launcher = find_executable(LAUNCHER);
    let start = join_site(
        paths,
        arguments,
        boxed(&mut terminal),
        &prober,
        timing,
        launcher.as_deref(),
        format,
    )
    .await?;

    // The terminal is restored before the node takes it over.
    drop(terminal);
    match start {
        Some(start) => Err(start.exec()),
        None => Ok(()),
    }
}

/// Joins as a new node, or joins again when `--site-dir` holds the intact folder of a node of the
/// token's site, within the real-time bounds of `timing`. On a terminal, it then offers to start
/// the node with `launcher` and returns the start the operator let go ahead.
async fn join_site(
    paths: &RootPathArgs,
    arguments: NodeJoinArgs,
    mut terminal: Option<&mut dyn Terminal>,
    prober: &InitialSiteProber,
    timing: JoinTiming,
    launcher: Option<&Path>,
    format: OutputFormat,
) -> anyhow::Result<Option<NodeStart>> {
    let site_dir = paths.site_dir()?;
    let command = StartCommand::new(
        paths.command_line_site_dir(),
        paths.command_line_cache_dir(),
    );
    anyhow::ensure!(
        std::fs::symlink_metadata(&site_dir).is_err() || holds_node_folder(&site_dir),
        "{} already exists; a joined node needs a new directory, pass --site-dir with a path that does not exist",
        site_dir.display()
    );

    let run = JoinRun { timing, format };
    let token = read_token(&arguments, lend(&mut terminal))?;
    if std::fs::symlink_metadata(&site_dir).is_ok() {
        let folder = open_rejoining_folder(&site_dir, &token, arguments.name.as_deref())?;
        rejoin_site(arguments, token, folder, &command, lend(&mut terminal), run).await?;
    } else {
        join_new_site(
            arguments,
            token,
            site_dir,
            &command,
            lend(&mut terminal),
            prober,
            run,
        )
        .await?;
    }

    match terminal {
        Some(terminal) => offer_start(terminal, command, launcher, timing.start_countdown),
        None => Ok(None),
    }
}

/// How one join runs: its real-time bounds and how it reports.
#[derive(Clone, Copy)]
struct JoinRun {
    timing: JoinTiming,
    format: OutputFormat,
}

async fn join_new_site(
    arguments: NodeJoinArgs,
    token: JoinToken,
    site_dir: PathBuf,
    command: &StartCommand,
    mut terminal: Option<&mut dyn Terminal>,
    prober: &InitialSiteProber,
    run: JoinRun,
) -> anyhow::Result<()> {
    let JoinRun { timing, format } = run;
    let mut connection = connect_to_manager(&token, arguments.manager_host.clone(), timing).await?;
    announce_connection(&connection, lend(&mut terminal), format)?;

    let (inference, applications) =
        choose_runtimes(&arguments, lend(&mut terminal), prober).await?;
    let targets = probe_targets(&arguments, inference, applications, prober).await?;
    let name = choose_name(&arguments, lend(&mut terminal))?;

    let key = NodeKey::generate().context("create the node key")?;
    let mut proposal = JoinProposal {
        display_name: name,
        host: arguments.host.clone(),
        private_port: arguments.private_port.unwrap_or(DEFAULT_PRIVATE_PORT),
        targets,
    };
    if let Some(terminal) = lend(&mut terminal)
        && !confirm_proposal(&proposal, &key, &site_dir, terminal)?
    {
        anyhow::bail!("join cancelled; nothing was sent");
    }
    if let Some(terminal) = lend(&mut terminal) {
        show_fingerprint(terminal, &proposal.display_name, &key.fingerprint())?;
        terminal.prompt_context(&Message::Info(APPROVAL_NOTICE.to_owned()))?;
    } else if format == OutputFormat::Yaml {
        progress(APPROVAL_NOTICE);
    }

    let folder = loop {
        match connection.join(&proposal, &key, &site_dir).await {
            Ok(folder) => break folder,
            Err(NodeJoinError::DuplicateName(status)) if terminal.is_some() => {
                let terminal = lend(&mut terminal).expect("checked by the guard");
                terminal.prompt_context(&Message::Error(status.message().to_owned()))?;
                terminal
                    .prompt_context(&Message::Hint(replaced_node_hint(&proposal.display_name)))?;
                proposal.display_name = prompt_name(terminal, None)?;
            }
            Err(error) => return Err(join_failure(error)),
        }
    };

    warn_about_addresses(&folder, &connection, lend(&mut terminal)).await?;
    if format == OutputFormat::Yaml {
        return write_yaml(&JoinedDocument::of(
            "joined",
            &folder,
            &proposal.display_name,
            command,
        ));
    }
    if let Some(text) = report_joined(lend(&mut terminal), &proposal, &folder, command)? {
        write_stdout(&text)?;
    }
    Ok(())
}

/// Joins the node of the intact folder to the site again, probing and asking nothing: the folder
/// already holds the node's identity, targets, and private port.
async fn rejoin_site(
    arguments: NodeJoinArgs,
    token: JoinToken,
    folder: RejoiningNodeFolder,
    command: &StartCommand,
    mut terminal: Option<&mut dyn Terminal>,
    run: JoinRun,
) -> anyhow::Result<()> {
    let JoinRun { timing, format } = run;
    let mut connection = connect_to_manager(&token, arguments.manager_host.clone(), timing).await?;
    announce_connection(&connection, lend(&mut terminal), format)?;
    if let Some(ignored) = ignored_rejoin_flags(&arguments) {
        Notices::new(lend(&mut terminal)).warn(&format!(
            "{ignored} ignored: a node that joins again keeps its capabilities and execution targets"
        ))?;
    }

    let proposal = RejoinProposal {
        host: arguments.host.clone(),
        private_port: arguments.private_port.unwrap_or(folder.private_port()),
    };
    if let Some(terminal) = lend(&mut terminal) {
        terminal.prompt_context(&Message::Info(format!(
            "Joining again from {}",
            folder.root().display()
        )))?;
        // The folder holds no display name; the manager shows the one the site declares.
        let name = arguments.name.as_deref().unwrap_or("this node");
        show_fingerprint(terminal, name, &folder.fingerprint())?;
        terminal.prompt_context(&Message::Info(APPROVAL_NOTICE.to_owned()))?;
    } else if format == OutputFormat::Yaml {
        progress(APPROVAL_NOTICE);
    }
    let joined = connection
        .rejoin(&proposal, &folder)
        .await
        .map_err(join_failure)?;
    drop(folder);

    warn_about_addresses(&joined, &connection, lend(&mut terminal)).await?;
    if format == OutputFormat::Yaml {
        return write_yaml(&JoinedDocument::of(
            "rejoined",
            &joined,
            &joined.display_name,
            command,
        ));
    }
    if let Some(text) = report_rejoined(lend(&mut terminal), &joined, command)? {
        write_stdout(&text)?;
    }
    Ok(())
}

/// Reports whether `directory` holds a node folder manifest, which is what a join again reopens.
fn holds_node_folder(directory: &Path) -> bool {
    std::fs::symlink_metadata(directory.join(WORKER_MANIFEST_FILE)).is_ok()
}

/// Opens the node folder at `site_dir` to join `token`'s site again, refusing a `--name` that is
/// not the folder's node before anything is sent.
fn open_rejoining_folder(
    site_dir: &Path,
    token: &JoinToken,
    name: Option<&str>,
) -> anyhow::Result<RejoiningNodeFolder> {
    let folder = RejoiningNodeFolder::open(site_dir, token).map_err(|error| {
        anyhow::Error::new(error).context(format!("cannot join again from {}", site_dir.display()))
    })?;
    if let Some(name) = name
        && derive_site_node_id(&token.site_id, name) != folder.node_id()
    {
        return Err(anyhow::Error::new(RejoinNameMismatch {
            name: name.to_owned(),
            root: site_dir.to_owned(),
        }));
    }
    Ok(folder)
}

/// Names the flags a join again does not use, when the operator passed any.
fn ignored_rejoin_flags(arguments: &NodeJoinArgs) -> Option<String> {
    let flags: Vec<&str> = [
        ("--inference", !arguments.inference.is_empty()),
        ("--applications", !arguments.applications.is_empty()),
        ("--nvidia-device", !arguments.nvidia_device.is_empty()),
        (
            "--native-workload-lifetime",
            arguments.native_workload_lifetime.is_some(),
        ),
    ]
    .into_iter()
    .filter_map(|(flag, given)| given.then_some(flag))
    .collect();
    (!flags.is_empty()).then(|| flags.join(", "))
}

/// Shows which site and manager the token reached: next to the prompts on the terminal, and on
/// stderr when the command prints YAML.
fn announce_connection(
    connection: &JoinConnection,
    terminal: Option<&mut dyn Terminal>,
    format: OutputFormat,
) -> anyhow::Result<()> {
    if format == OutputFormat::Yaml {
        progress(&format!("Connected to site {}", connection.site_id()));
        return Ok(());
    }
    let mut notices = Notices::new(terminal);
    notices.info(Message::Heading(format!(
        "Connected to site {}",
        connection.site_id()
    )))?;
    notices.info(Message::KeyValues(vec![
        (
            "manager".to_owned(),
            connection.manager_node_id().to_owned(),
        ),
        (
            "CA fingerprint".to_owned(),
            connection.site_ca_sha256().as_str().to_owned(),
        ),
    ]))
}

/// Warns about loopback addresses of the joined node or the manager, which other machines cannot
/// reach.
async fn warn_about_addresses(
    folder: &JoinedNodeFolder,
    connection: &JoinConnection,
    terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    let mut notices = Notices::new(terminal);
    let resolver = SystemHostResolver;
    for host in [&folder.host, connection.manager_host()] {
        if let Some(warning) = loopback_host_warning(host, &resolver).await {
            notices.warn(&warning)?;
        }
    }
    Ok(())
}

/// The YAML document of a node folder installed by joining.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JoinedDocument {
    outcome: &'static str,
    site_id: String,
    node_id: String,
    display_name: String,
    host: String,
    private_address: String,

    /// The command that starts the node.
    start_command: String,
}

impl JoinedDocument {
    fn of(
        outcome: &'static str,
        folder: &JoinedNodeFolder,
        display_name: &str,
        command: &StartCommand,
    ) -> Self {
        Self {
            outcome,
            site_id: folder.site_id.clone(),
            node_id: folder.node_id.clone(),
            display_name: display_name.to_owned(),
            host: folder.host.to_string(),
            private_address: folder.private_bind_address.to_string(),
            start_command: command.to_string(),
        }
    }
}

/// Reports the installed node folder, on the terminal when there is one, else as plain text with
/// the command that starts it. Returns the text to write when there is no terminal.
fn report_joined(
    terminal: Option<&mut dyn Terminal>,
    proposal: &JoinProposal,
    folder: &JoinedNodeFolder,
    command: &StartCommand,
) -> anyhow::Result<Option<String>> {
    let Some(terminal) = terminal else {
        return Ok(Some(plain_text(&[
            Message::Success(format!(
                "Joined {} as {} at {}",
                folder.site_id, proposal.display_name, folder.host
            )),
            Message::Info("Start the node with:".to_owned()),
            Message::Command(command.to_string()),
        ])));
    };
    terminal.message(&Message::Success(format!(
        "Joined {} as {} at {}",
        folder.site_id, proposal.display_name, folder.host
    )))?;
    Ok(None)
}

/// Reports the rewritten node folder, on the terminal when there is one, else as plain text with
/// the command that starts it. Returns the text to write when there is no terminal.
fn report_rejoined(
    terminal: Option<&mut dyn Terminal>,
    folder: &JoinedNodeFolder,
    command: &StartCommand,
) -> anyhow::Result<Option<String>> {
    let Some(terminal) = terminal else {
        return Ok(Some(plain_text(&[
            Message::Success(format!(
                "Rejoined {} at {}",
                folder.display_name, folder.host
            )),
            Message::Info("Start the node with:".to_owned()),
            Message::Command(command.to_string()),
        ])));
    };
    terminal.message(&Message::Success(format!(
        "Rejoined {} at {}",
        folder.display_name, folder.host
    )))?;
    Ok(None)
}

/// Starts the joined node with `launcher` once a countdown of `countdown` runs out, unless the
/// operator skips it; without a launcher, or after a skip, says the node is not started.
/// Returns the start to carry out.
fn offer_start(
    terminal: &mut dyn Terminal,
    command: StartCommand,
    launcher: Option<&Path>,
    countdown: Duration,
) -> anyhow::Result<Option<NodeStart>> {
    if let Some(launcher) = launcher
        && terminal.countdown(&CountdownPrompt::new("Start the node", countdown))?
    {
        return Ok(Some(NodeStart {
            launcher: launcher.to_owned(),
            command,
        }));
    }
    terminal.message(&Message::Info("The node is not started".to_owned()))?;
    Ok(None)
}

/// Reads the token from `--token`, the token file, or a hidden prompt, in that order.
fn read_token(
    arguments: &NodeJoinArgs,
    terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<JoinToken> {
    if let Some(text) = &arguments.token {
        return JoinToken::parse(text.trim()).context("--token holds an invalid token");
    }
    if let Some(path) = &arguments.token_file {
        warn_when_readable_by_others(path, &mut Notices::new(terminal))?;
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the join token file {}", path.display()))?;
        return JoinToken::parse(text.trim()).context("the join token file holds an invalid token");
    }
    let Some(terminal) = terminal else {
        anyhow::bail!(
            "no join token; pass --token or --token-file, or run in a terminal to enter it"
        );
    };
    loop {
        let text = terminal.secret(&InputPrompt::new("Join token (hidden)"))?;
        match JoinToken::parse(text.trim()) {
            Ok(token) => return Ok(token),
            Err(error) => terminal.prompt_context(&Message::Error(format!(
                "that is not a valid join token: {}",
                crate::hint::describe(&error)
            )))?,
        }
    }
}

fn warn_when_readable_by_others(path: &Path, notices: &mut Notices<'_>) -> anyhow::Result<()> {
    let mode = std::fs::metadata(path)
        .with_context(|| format!("inspect the join token file {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        notices.warn(&format!(
            "the join token file {} is readable by other users (mode {:o}); restrict it with chmod 600",
            path.display(),
            mode & 0o777
        ))?;
    }
    Ok(())
}

/// Returns the flag values, or asks for the capabilities the flags left open using what the
/// probe found on this machine.
async fn choose_runtimes(
    arguments: &NodeJoinArgs,
    terminal: Option<&mut dyn Terminal>,
    prober: &InitialSiteProber,
) -> anyhow::Result<(InferencePick, BTreeSet<InitialApplicationRuntime>)> {
    let inference =
        InferenceChoice::capabilities(&InferenceArgument::choices(&arguments.inference))?;
    let applications = ApplicationsChoice::capabilities(&arguments.applications)?;
    if let (Some(inference), Some(applications)) = (&inference, &applications) {
        return Ok((
            InferencePick {
                inference: inference.clone(),
                nvidia_cdi_selectors: None,
                simulated_hardware: InferenceArgument::simulated_hardware(&arguments.inference),
            },
            applications.clone(),
        ));
    }
    let Some(terminal) = terminal else {
        anyhow::bail!(
            "missing workload choices; pass --inference and --applications or run in a terminal without --yes"
        );
    };
    let recommendations =
        CapabilityRecommendations::collect(prober, inference.is_none(), applications.is_none())
            .await;
    Ok((
        collect_inference(inference, in_dev_env(), terminal, &recommendations)?,
        collect_applications(applications, terminal, &recommendations)?,
    ))
}

/// Probes the chosen capabilities and resolves them into the node's targets the same way site
/// initialization does: Docker-backed targets take the Docker platform, simulated models the
/// declared machine's GPUs and platform, simulated applications this machine's architecture,
/// NVIDIA one target per selected GPU, and Apple the observed GPU.
async fn probe_targets(
    arguments: &NodeJoinArgs,
    inference: InferencePick,
    applications: BTreeSet<InitialApplicationRuntime>,
    prober: &InitialSiteProber,
) -> anyhow::Result<NodeTargetChoices> {
    let offers = |capability| inference.inference.contains(&capability);
    let mut request = InitialSiteRequest::new(inference.inference.clone(), applications);
    let selectors = (!arguments.nvidia_device.is_empty())
        .then(|| arguments.nvidia_device.clone())
        .or(inference.nvidia_cdi_selectors.clone());
    if let Some(selectors) = selectors {
        anyhow::ensure!(
            offers(InitialInferenceRuntime::Nvidia),
            "--nvidia-device requires the nvidia inference capability"
        );
        request = request.with_nvidia_cdi_selectors(selectors);
    }
    if let Some(lifetime) = arguments.native_workload_lifetime {
        anyhow::ensure!(
            offers(InitialInferenceRuntime::Apple),
            "--native-workload-lifetime requires the apple inference capability"
        );
        request = request.with_native_workload_lifetime(lifetime.to_lifetime());
    }
    if let Some(hardware) = inference.simulated_hardware.clone() {
        request = request.with_simulated_hardware(hardware);
    }
    let probe = prober
        .probe(&request)
        .await
        .context("probe this machine's capabilities")?;
    NodeTargetChoices::resolve(&request, &probe, std::env::consts::ARCH)
        .context("select this machine's execution targets")
}

/// Connects once to the join host at the token's host, or at `manager_host` when given, within
/// the bounds of `timing`. A failure names the address tried, plus the flag to use when the
/// manager was not reachable.
async fn connect_to_manager(
    token: &JoinToken,
    manager_host: Option<SiteNodeHost>,
    timing: JoinTiming,
) -> anyhow::Result<JoinConnection> {
    let mut tried = token.endpoint.clone();
    if let Some(host) = &manager_host {
        let _ = tried.set_host(Some(&host.to_string()));
    }
    JoinConnection::connect(token.clone(), manager_host, timing)
        .await
        .map_err(|error| {
            join_failure(error).context(format!("cannot reach the manager at {tried}"))
        })
}

/// Reports whether a connect failure means the manager could not be reached (timeout or
/// refusal), as opposed to being reached and failing authentication, where another address
/// would not help.
fn is_unreachable(error: &NodeJoinError) -> bool {
    let NodeJoinError::Connect(source) = error else {
        return false;
    };
    let mut reached_transport_failure = false;
    for cause in source.chain() {
        if cause.downcast_ref::<rustls::Error>().is_some() {
            return false;
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            reached_transport_failure |= matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::HostUnreachable
                    | std::io::ErrorKind::NetworkUnreachable
            );
        }
        reached_transport_failure |= cause.to_string().contains("timed out");
    }
    reached_transport_failure
}

/// Returns the flag name, else the hostname-derived default, which an interactive operator
/// may override.
fn choose_name(
    arguments: &NodeJoinArgs,
    terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<String> {
    if let Some(name) = &arguments.name {
        return Ok(name.clone());
    }
    let default = local_node_name();
    match terminal {
        Some(terminal) => prompt_name(terminal, default.as_deref()),
        None => default.context("this machine has no usable hostname; pass --name"),
    }
}

/// Asks for a node name until it is valid; an empty answer takes the default, if any.
fn prompt_name(terminal: &mut dyn Terminal, default: Option<&str>) -> anyhow::Result<String> {
    let prompt = match default {
        Some(default) => InputPrompt::new("Node name").with_default(default),
        None => InputPrompt::new("Node name"),
    };
    loop {
        match parse_display_name(terminal.input(&prompt)?.trim()) {
            Ok(name) => return Ok(name),
            Err(message) => terminal.prompt_context(&Message::Error(message))?,
        }
    }
}

fn confirm_proposal(
    proposal: &JoinProposal,
    key: &NodeKey,
    site_dir: &Path,
    terminal: &mut dyn Terminal,
) -> anyhow::Result<bool> {
    let mut facts = vec![
        ("name".to_owned(), proposal.display_name.clone()),
        (
            "host".to_owned(),
            proposal
                .host
                .as_ref()
                .map_or_else(|| "observed by the manager".to_owned(), ToString::to_string),
        ),
        ("models".to_owned(), proposal.inference_summary()),
        ("applications".to_owned(), proposal.applications_summary()),
        ("node folder".to_owned(), site_dir.display().to_string()),
    ];
    facts.extend(fingerprint_facts(&key.fingerprint()));
    if proposal.private_port != DEFAULT_PRIVATE_PORT {
        facts.insert(
            2,
            ("private port".to_owned(), proposal.private_port.to_string()),
        );
    }
    terminal.prompt_context(&Message::Heading("Proposal".to_owned()))?;
    terminal.prompt_context(&Message::KeyValues(facts))?;
    Ok(terminal.confirm(&ConfirmPrompt::new(
        "Send this proposal to the manager?",
        true,
    ))?)
}

/// Asks the operator to match this machine's key fingerprint and its pattern with the ones shown
/// where the node named `name` is approved.
fn show_fingerprint(
    terminal: &mut dyn Terminal,
    name: &str,
    fingerprint: &CertificateFingerprint,
) -> anyhow::Result<()> {
    terminal.prompt_context(&Message::Info(format!(
        "Confirm this fingerprint where you approve {name}:"
    )))?;
    terminal.prompt_context(&fingerprint_grid(fingerprint))?;
    terminal.prompt_context(&Message::KeyValues(fingerprint_facts(fingerprint).to_vec()))?;
    Ok(())
}

/// Adds the command the operator should run next to a join failure.
fn join_failure(error: NodeJoinError) -> anyhow::Error {
    let hint = match &error {
        NodeJoinError::CommitOutcomeUnknown { .. } => {
            "check what `dotc node add` printed on the manager before retrying"
        }
        NodeJoinError::Connect(_) if is_unreachable(&error) => MANAGER_HOST_HINT,
        // The manager closes the connection without a reason for a token it doesn't hold, so an
        // expired or used token looks like any other refused connection here.
        NodeJoinError::Connect(_) => CONNECTION_CLOSED_HINT,
        NodeJoinError::DuplicateName(_) => {
            "choose another name with --name, or if this machine replaces that node, run \
             `dotc node remove <node name>` on the manager and join again"
        }
        NodeJoinError::DuplicateHost(_) | NodeJoinError::UnusableHost(_) => {
            "set this machine's address with --host"
        }
        NodeJoinError::UnreachableNode { .. } => {
            "make sure the manager's machine reaches this node at that host and port, then join \
             again; repeated failures revoke the token, so you may need a new one from \
             `dotc node add`"
        }
        NodeJoinError::PrivateListener { .. } => {
            "free that port, or choose another with --private-port"
        }
        _ => "",
    };
    let context = if hint.is_empty() {
        "node join failed".to_owned()
    } else {
        format!("node join failed; {hint}")
    };
    anyhow::Error::new(error).context(context)
}

/// Tells the operator how to reuse a name that a node of the site already holds.
fn replaced_node_hint(name: &str) -> String {
    format!(
        "If this machine replaces {name}, run `dotc node remove {}` on the manager and join again",
        shell_word(name)
    )
}

/// The launcher arguments that start a joined node from its folder: `--site-dir` and
/// `--cache-dir` for the folders typed on the command line, since folders from the environment
/// reach the launcher through it. It shows as the command an operator would type.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StartCommand {
    arguments: Vec<OsString>,
}

impl StartCommand {
    fn new(site_dir: Option<&Path>, cache_dir: Option<&Path>) -> Self {
        let mut arguments = Vec::new();
        for (flag, folder) in [("--site-dir", site_dir), ("--cache-dir", cache_dir)] {
            if let Some(folder) = folder {
                arguments.extend([OsString::from(flag), folder.as_os_str().to_owned()]);
            }
        }
        Self { arguments }
    }
}

impl fmt::Display for StartCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(LAUNCHER)?;
        for argument in &self.arguments {
            write!(formatter, " {}", shell_word(&argument.to_string_lossy()))?;
        }
        Ok(())
    }
}

/// A joined node to start: the launcher found on the search path, run with the start command's
/// arguments in place of this process.
#[derive(Debug)]
struct NodeStart {
    launcher: PathBuf,
    command: StartCommand,
}

impl NodeStart {
    /// Replaces this process with the launcher, so the node keeps its terminal and Ctrl-C stops
    /// it. Returns only when the launcher could not run, with the command to start it by hand.
    fn exec(self) -> anyhow::Error {
        let error = Command::new(&self.launcher)
            .args(&self.command.arguments)
            .exec();
        anyhow::Error::new(error).context(format!(
            "cannot start the node with {}; start it with: {}",
            self.launcher.display(),
            self.command
        ))
    }
}

/// `--name` names another node than the one in the folder that joins again.
#[derive(Debug, thiserror::Error)]
#[error("--name {name:?} is not the name of the node in {}", root.display())]
pub(crate) struct RejoinNameMismatch {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
}

#[cfg(test)]
mod tests {
    use dotcore_site::node::folder::CertificateFingerprint;
    use url::Url;

    use dotcore_domain::{
        ExecutionPlatform,
        timing::{JoinTiming, Timing},
    };
    use dotcore_proto::dotcore::{node::v1::DeclaredMachine, site::v1::NativeWorkloadLifetime};
    use dotcore_site::initial::inputs::{ApplicationTargetChoice, ModelTargetChoice};

    use super::*;
    use crate::terminal::{GridColor, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};

    #[test]
    fn the_token_is_asked_for_hidden_and_asked_again_when_malformed() {
        let token = token();
        let arguments = arguments();
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Secret("not a token".to_owned()),
            ScriptedAnswer::Secret(format!("  {}\n", token.encode())),
        ]);

        let read = read_token_without_environment(&arguments, &mut terminal);

        assert_eq!(read.encode(), token.encode());
        assert!(
            terminal
                .prompts()
                .iter()
                .all(|prompt| matches!(prompt, ScriptedPrompt::Secret(_)))
        );
        assert_eq!(terminal.prompts().len(), 2);
        assert!(terminal.lines()[0].starts_with("✘ that is not a valid join token"));
    }

    #[test]
    fn the_token_flag_is_used_without_prompting() {
        let token = token();
        let mut arguments = arguments();
        arguments.token = Some(format!(" {} ", token.encode()));
        let mut terminal = ScriptedTerminal::interactive([]);

        let read = read_token(&arguments, Some(&mut terminal)).expect("token");

        assert_eq!(read.encode(), token.encode());
        assert!(terminal.prompts().is_empty());
    }

    #[test]
    fn a_missing_token_without_a_terminal_names_both_flags() {
        let error = read_token(&arguments(), None).expect_err("no token");

        assert_eq!(
            error.to_string(),
            "no join token; pass --token or --token-file, or run in a terminal to enter it"
        );
    }

    #[test]
    fn token_and_token_file_conflict() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            join: NodeJoinArgs,
        }
        use clap::Parser;

        let error =
            Cli::try_parse_from(["cli", "--token", "dcjoin1.x", "--token-file", "/tmp/token"])
                .err()
                .expect("conflict");

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn a_duplicate_or_invalid_name_is_asked_again() {
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Input("   ".to_owned()),
            ScriptedAnswer::Input("gpu-box".to_owned()),
        ]);

        let name = prompt_name(&mut terminal, None).expect("name");

        assert_eq!(name, "gpu-box");
        assert_eq!(
            terminal.lines(),
            ["✘ display name must contain 1 to 128 non-control characters"]
        );
    }

    #[test]
    fn an_empty_name_answer_takes_the_default() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Input(String::new())]);

        assert_eq!(
            prompt_name(&mut terminal, Some("studio")).expect("name"),
            "studio"
        );
    }

    #[test]
    fn an_unreachable_token_address_fails_without_prompting_and_names_the_flag() {
        let closed = dotcore::testing::reserve_port();
        let port = closed.port();
        let token = token_at(port);

        let error = block_on(connect_to_manager(&token, None, Timing::test().join))
            .err()
            .expect("unreachable manager");

        let text = format!("{error:#}");
        assert!(
            text.contains(&format!(
                "cannot reach the manager at https://127.0.0.1:{port}/"
            )),
            "{text}"
        );
        assert!(text.contains(MANAGER_HOST_HINT), "{text}");
        assert!(!text.contains("DOTCORE_MANAGER_HOST"), "{text}");
    }

    #[test]
    fn a_connection_the_manager_closes_suggests_a_new_command() {
        let error = join_failure(NodeJoinError::Connect(anyhow::anyhow!(
            "the manager closed the connection"
        )));

        assert_eq!(
            error.to_string(),
            "node join failed; the token may have expired or already been used; get a new \
             command from the console's Add machine or `dotc node add`, and keep it open on the \
             manager until this machine joins"
        );
    }

    #[test]
    fn an_unreachable_manager_host_flag_is_the_address_reported() {
        let closed = dotcore::testing::reserve_port();
        let port = closed.port();
        let token = token_at(port);

        let error = block_on(connect_to_manager(
            &token,
            Some(SiteNodeHost::parse("127.0.0.2").expect("manager host")),
            Timing::test().join,
        ))
        .err()
        .expect("unreachable manager");

        let text = format!("{error:#}");
        assert!(
            text.contains(&format!(
                "cannot reach the manager at https://127.0.0.2:{port}/"
            )),
            "{text}"
        );
    }

    #[test]
    fn an_interactive_join_report_announces_the_node_and_writes_nothing() {
        let mut terminal = ScriptedTerminal::interactive([]);

        let records = report_joined(
            Some(&mut terminal),
            &proposal(),
            &joined_folder(),
            &StartCommand::new(None, None),
        )
        .expect("report");

        assert_eq!(records, None);
        assert_eq!(terminal.lines(), ["✔ Joined site-1 as gpu-box at 10.0.0.7"]);
    }

    #[test]
    fn a_rejoin_report_names_the_node_or_prints_its_start_command() {
        let mut terminal = ScriptedTerminal::interactive([]);

        let records = report_rejoined(
            Some(&mut terminal),
            &joined_folder(),
            &StartCommand::new(None, None),
        )
        .expect("report");

        assert_eq!(records, None);
        assert_eq!(terminal.lines(), ["✔ Rejoined gpu-box at 10.0.0.7"]);
        assert_eq!(
            report_rejoined(
                None,
                &joined_folder(),
                &StartCommand::new(Some(Path::new("/srv/node")), None)
            )
            .expect("records")
            .as_deref(),
            Some(
                "✔ Rejoined gpu-box at 10.0.0.7\nStart the node with:\n  dotcore --site-dir /srv/node\n"
            )
        );
    }

    #[test]
    fn flags_a_rejoin_does_not_use_are_named() {
        let mut arguments = arguments();
        assert_eq!(ignored_rejoin_flags(&arguments), None);

        arguments.inference = vec![InferenceChoice::Simulated.into()];
        arguments.applications = vec![ApplicationsChoice::Docker];
        assert_eq!(
            ignored_rejoin_flags(&arguments).as_deref(),
            Some("--inference, --applications")
        );
    }

    #[test]
    fn an_unattended_join_report_prints_the_start_command() {
        let records = report_joined(
            None,
            &proposal(),
            &joined_folder(),
            &StartCommand::new(Some(Path::new("/srv/node")), None),
        )
        .expect("report");

        assert_eq!(
            records.as_deref(),
            Some(
                "✔ Joined site-1 as gpu-box at 10.0.0.7\nStart the node with:\n  dotcore --site-dir /srv/node\n"
            )
        );
    }

    #[test]
    fn a_connect_timeout_counts_as_unreachable() {
        // With the accept queue full, further connection attempts are left unanswered.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().unwrap();
        let _queued: Vec<_> = (0..200)
            .filter_map(|_| {
                std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(20))
                    .ok()
            })
            .collect();
        let token = token_at(address.port());

        let error = block_on(JoinConnection::connect(
            token,
            None,
            JoinTiming {
                connect_timeout: std::time::Duration::from_millis(300),
                ..Timing::test().join
            },
        ))
        .err()
        .expect("connect timeout");

        assert!(is_unreachable(&error), "{error:?}");
    }

    #[test]
    fn a_tls_failure_names_no_address_hint() {
        // A peer that answers with something other than TLS was reached, so another address
        // would not help.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let token = token_at(listener.local_addr().unwrap().port());
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0; 512];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        });

        let error = block_on(connect_to_manager(&token, None, Timing::test().join))
            .err()
            .expect("tls failure");
        server.join().expect("server");

        assert!(
            !format!("{error:#}").contains(MANAGER_HOST_HINT),
            "{error:#}"
        );
    }

    #[test]
    fn a_native_workload_lifetime_needs_apple_inference() {
        let mut arguments = arguments();
        arguments.native_workload_lifetime = Some(NativeWorkloadLifetimeChoice::Ephemeral);
        assert_eq!(arguments.argument_contract_error(), None);

        arguments.inference = vec![InferenceChoice::Apple.into()];
        assert_eq!(arguments.argument_contract_error(), None);

        arguments.inference = vec![
            InferenceChoice::Simulated.into(),
            InferenceChoice::Apple.into(),
        ];
        assert_eq!(arguments.argument_contract_error(), None);

        arguments.inference = vec![InferenceChoice::Nvidia.into()];
        assert_eq!(
            arguments.argument_contract_error(),
            Some("--native-workload-lifetime requires --inference apple")
        );
    }

    #[test]
    fn none_stands_alone_among_repeated_choices() {
        let mut arguments = arguments();
        arguments.inference = vec![InferenceChoice::None.into(), InferenceChoice::None.into()];
        arguments.applications = vec![ApplicationsChoice::None];
        assert_eq!(arguments.argument_contract_error(), None);

        arguments.inference = vec![InferenceChoice::None.into(), InferenceChoice::Nvidia.into()];
        assert_eq!(
            arguments.argument_contract_error(),
            Some("--inference none cannot be combined with another --inference value")
        );

        arguments.inference = vec![InferenceChoice::None.into()];
        arguments.applications = vec![ApplicationsChoice::Docker, ApplicationsChoice::None];
        assert_eq!(
            arguments.argument_contract_error(),
            Some("--applications none cannot be combined with another --applications value")
        );
    }

    #[test]
    fn apple_inference_proposes_the_darwin_platform_observed_device_and_lifetime() {
        use dotcore_node_hardware::hardware::{AcceleratorUnavailable, AcceleratorVendorAdapter};
        use dotcore_proto::dotcore::observation::v1::{AcceleratorFacts, AcceleratorVendor};
        use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};

        struct NoDocker;

        impl DockerCapabilityProbe for NoDocker {
            fn probe(
                &self,
            ) -> std::pin::Pin<
                Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>,
            > {
                Box::pin(async { anyhow::bail!("Docker is not probed for Apple inference") })
            }

            fn nvidia_cdi_selectors(
                &self,
            ) -> std::pin::Pin<
                Box<
                    dyn Future<Output = anyhow::Result<std::collections::BTreeSet<String>>>
                        + Send
                        + '_,
                >,
            > {
                Box::pin(async { anyhow::bail!("Docker is not probed for Apple inference") })
            }
        }

        struct Adapter(AcceleratorVendor);

        impl AcceleratorVendorAdapter for Adapter {
            fn vendor(&self) -> AcceleratorVendor {
                self.0
            }

            fn collect_accelerator_facts(
                &self,
            ) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
                match self.0 {
                    AcceleratorVendor::Apple => Ok(vec![AcceleratorFacts {
                        vendor: AcceleratorVendor::Apple as i32,
                        device_id: "apple-gpu-0".to_owned(),
                        ..Default::default()
                    }]),
                    _ => Ok(Vec::new()),
                }
            }
        }

        let prober = InitialSiteProber::new(
            Box::new(NoDocker),
            Box::new(Adapter(AcceleratorVendor::Nvidia)),
        )
        .with_apple_accelerator(Box::new(Adapter(AcceleratorVendor::Apple)));
        let mut arguments = arguments();
        arguments.native_workload_lifetime = Some(NativeWorkloadLifetimeChoice::Ephemeral);

        let targets = block_on(probe_targets(
            &arguments,
            InferencePick {
                inference: BTreeSet::from([InitialInferenceRuntime::Apple]),
                nvidia_cdi_selectors: None,
                simulated_hardware: None,
            },
            BTreeSet::from([InitialApplicationRuntime::Simulated]),
            &prober,
        ))
        .expect("probe Apple capabilities");

        assert_eq!(
            targets.model_targets,
            [ModelTargetChoice::Apple {
                device_id: "apple-gpu-0".to_owned(),
                lifetime: NativeWorkloadLifetime::Ephemeral,
            }]
        );
        assert_eq!(
            targets
                .application_targets
                .iter()
                .map(|target| target.applications)
                .collect::<Vec<_>>(),
            [InitialApplicationRuntime::Simulated]
        );
    }

    #[test]
    fn an_unattended_join_needs_an_explicit_name_or_hostname() {
        let mut arguments = arguments();
        arguments.name = Some("explicit".to_owned());

        assert_eq!(choose_name(&arguments, None).expect("name"), "explicit");
    }

    #[test]
    fn the_confirmation_shows_the_proposal_and_the_key_fingerprint() {
        let key = NodeKey::generate().expect("key");
        let proposal = JoinProposal {
            display_name: "gpu-box".to_owned(),
            host: Some(SiteNodeHost::parse("10.0.0.7").expect("host")),
            private_port: 50053,
            targets: NodeTargetChoices {
                model_targets: vec![ModelTargetChoice::Simulated {
                    platform: ExecutionPlatform::linux_arm64(),
                    device_id: "GPU-00000000-0000-0000-0000-00000000gb10".to_owned(),
                }],
                application_targets: vec![ApplicationTargetChoice {
                    applications: InitialApplicationRuntime::Docker,
                    platform: ExecutionPlatform::linux_arm64(),
                    accelerator_device_ids: Vec::new(),
                }],
                accelerator_facts: Vec::new(),
                declared_machine: Some(DeclaredMachine {
                    choices: vec!["gb10".to_owned()],
                    description: Some(
                        dotcore_node_hardware::MachineDescription::named("gb10")
                            .expect("named machine")
                            .to_stored(),
                    ),
                }),
            },
        };
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);

        let sent = confirm_proposal(&proposal, &key, Path::new("/srv/node"), &mut terminal)
            .expect("confirmation");
        let grouped = key.fingerprint().grouped_lines();

        assert!(!sent);
        assert_eq!(
            terminal.lines(),
            [
                "Proposal".to_owned(),
                "  name             gpu-box".to_owned(),
                "  host             10.0.0.7".to_owned(),
                "  models           simulated (GPU-00000000-0000-0000-0000-00000000gb10, linux/arm64)"
                    .to_owned(),
                "  applications     docker (linux/arm64)".to_owned(),
                "  node folder      /srv/node".to_owned(),
                format!("  key fingerprint  {}", grouped[0]),
                format!("                   {}", grouped[1]),
            ]
        );
    }

    #[test]
    fn the_sent_proposal_asks_to_match_the_fingerprint_and_its_pattern() {
        let fingerprint = CertificateFingerprint::parse(
            "87524a4a3c1f9e0b5d26e8f1a7c4093b6e2d51f8c0a97b34e1d6f2085ab9c7e3",
        )
        .expect("fingerprint");
        let mut terminal = ScriptedTerminal::interactive([]);

        show_fingerprint(&mut terminal, "gpu-box", &fingerprint).expect("fingerprint shown");

        assert_eq!(
            terminal.lines(),
            [
                "Confirm this fingerprint where you approve gpu-box:",
                "  ████    ▓▓▓▓    ████",
                "  ████    ▓▓▓▓    ████",
                "  ▓▓▓▓    ░░░░    ▓▓▓▓",
                "  ▓▓▓▓    ░░░░    ▓▓▓▓",
                "  ░░░░            ░░░░",
                "  ░░░░            ░░░░",
                "  ████░░░░░░░░░░░░████",
                "  ████░░░░░░░░░░░░████",
                "  ▓▓▓▓████░░░░████▓▓▓▓",
                "  ▓▓▓▓████░░░░████▓▓▓▓",
                "  key fingerprint  8752 4a4a 3c1f 9e0b 5d26 e8f1 a7c4 093b",
                "                   6e2d 51f8 c0a9 7b34 e1d6 f208 5ab9 c7e3",
            ]
        );
        assert_eq!(
            terminal.messages()[1],
            Message::ColorGrid {
                colors: vec![GridColor::Blue, GridColor::Yellow, GridColor::Magenta],
                rows: vec![
                    vec![Some(0), None, Some(1), None, Some(0)],
                    vec![Some(1), None, Some(2), None, Some(1)],
                    vec![Some(2), None, None, None, Some(2)],
                    vec![Some(0), Some(2), Some(2), Some(2), Some(0)],
                    vec![Some(1), Some(0), Some(2), Some(0), Some(1)],
                ],
            }
        );
    }

    /// Reads the token as `read_token` does when neither a flag nor a file supplies it.
    fn read_token_without_environment(
        arguments: &NodeJoinArgs,
        terminal: &mut ScriptedTerminal,
    ) -> JoinToken {
        read_token(arguments, Some(terminal)).expect("token")
    }

    #[test]
    fn the_start_command_passes_only_the_folders_given_and_quotes_them_for_display() {
        assert_eq!(StartCommand::new(None, None).to_string(), "dotcore");
        assert_eq!(
            StartCommand::new(Some(Path::new("/srv/node")), None).to_string(),
            "dotcore --site-dir /srv/node"
        );

        let command = StartCommand::new(
            Some(Path::new("/srv/my node")),
            Some(Path::new("/var/cache/dotcore")),
        );
        assert_eq!(
            command.to_string(),
            "dotcore --site-dir '/srv/my node' --cache-dir /var/cache/dotcore"
        );
        assert_eq!(
            command.arguments,
            [
                "--site-dir",
                "/srv/my node",
                "--cache-dir",
                "/var/cache/dotcore"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn the_node_starts_with_the_launcher_once_the_countdown_runs_out() {
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Countdown(true)]);
        let command = StartCommand::new(Some(Path::new("/srv/node")), None);

        let start = offer_start(
            &mut terminal,
            command.clone(),
            Some(Path::new("/nix/store/x-dotcore/bin/dotcore")),
            Duration::from_secs(5),
        )
        .expect("offer")
        .expect("start");

        assert_eq!(
            start.launcher,
            Path::new("/nix/store/x-dotcore/bin/dotcore")
        );
        assert_eq!(start.command, command);
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::Countdown(CountdownPrompt::new(
                "Start the node",
                Duration::from_secs(5)
            ))]
        );
        assert!(terminal.lines().is_empty());
    }

    #[test]
    fn a_skipped_start_or_a_dotc_without_launcher_says_the_node_is_not_started() {
        let command = StartCommand::new(Some(Path::new("/srv/node")), None);
        let hint = ["The node is not started"];

        let mut skipped = ScriptedTerminal::interactive([ScriptedAnswer::Countdown(false)]);
        let start = offer_start(
            &mut skipped,
            command.clone(),
            Some(Path::new("/nix/store/x-dotcore/bin/dotcore")),
            Duration::from_secs(5),
        )
        .expect("offer");
        assert!(start.is_none());
        assert_eq!(skipped.lines(), hint);

        let mut without_launcher = ScriptedTerminal::interactive([]);
        let start = offer_start(&mut without_launcher, command, None, Duration::from_secs(5))
            .expect("offer");
        assert!(start.is_none());
        assert!(without_launcher.prompts().is_empty());
        assert_eq!(without_launcher.lines(), hint);
    }

    fn proposal() -> JoinProposal {
        JoinProposal {
            display_name: "gpu-box".to_owned(),
            host: None,
            private_port: 50053,
            targets: NodeTargetChoices::default(),
        }
    }

    fn joined_folder() -> JoinedNodeFolder {
        JoinedNodeFolder {
            site_id: "site-1".to_owned(),
            node_id: "node-7".to_owned(),
            display_name: "gpu-box".to_owned(),
            root: PathBuf::from("/srv/node"),
            host: SiteNodeHost::parse("10.0.0.7").expect("host"),
            private_bind_address: "10.0.0.7:50053".parse().unwrap(),
        }
    }

    fn arguments() -> NodeJoinArgs {
        NodeJoinArgs {
            token: None,
            token_file: None,
            name: None,
            manager_host: None,
            host: None,
            private_port: None,
            inference: Vec::new(),
            applications: Vec::new(),
            nvidia_device: Vec::new(),
            native_workload_lifetime: None,
            yes: false,
        }
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn token() -> JoinToken {
        token_at(50053)
    }

    fn token_at(port: u16) -> JoinToken {
        JoinToken::generate(
            Url::parse(&format!("https://127.0.0.1:{port}")).unwrap(),
            "site-test",
            "node-manager",
            CertificateFingerprint::from_bytes([1; 32]),
            CertificateFingerprint::from_bytes([2; 32]),
        )
        .expect("token")
    }
}
