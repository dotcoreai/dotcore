//! `dotc node join`: turns this machine into a node of an existing site by reaching the
//! manager's join listener with the one-time token from `dotc node add`.
//!
//! The command probes this machine, proposes a name, address, and runtime capabilities, and
//! asks the operator to confirm before anything is sent. The node's private key is created
//! here and never leaves; the finished node folder is what `dotcore` runs. A node whose
//! intact folder of the same site already exists joins again instead, with no probe or prompt,
//! keeping its key, identity, and deployments and changing only its addresses.

use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::Args;
use dotcore::composition::DEFAULT_PRIVATE_PORT;
use dotcore_domain::{SiteNodeHost, SystemHostResolver};
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
        DEFAULT_JOIN_CONNECT_TIMEOUT, JoinConnection, JoinProposal, JoinToken, JoinedNodeFolder,
        NodeJoinError, NodeKey, RejoinProposal, RejoiningNodeFolder,
    },
    node::{folder::WORKER_MANIFEST_FILE, hostname::local_node_name},
};

use super::{Notices, boxed, lend, record_value, write_records};
use crate::{
    RootPathArgs,
    failure::shell_word,
    guided::{
        CapabilityRecommendations, InferencePick, collect_applications, collect_inference,
        in_dev_env,
    },
    site::{
        ApplicationsChoice, InferenceArgument, InferenceChoice, NativeWorkloadLifetimeChoice,
        inference_long_help, loopback_host_warning, parse_absolute_path, parse_display_name,
        parse_host, parse_inference, system_initial_site_prober,
    },
    terminal::{
        ConfirmPrompt, InputPrompt, Message, NextStep, Terminal, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
};

/// Shown after the proposal is sent, while the manager operator decides.
const APPROVAL_NOTICE: &str = "Waiting for approval on the manager...";

/// Hint shown when the manager cannot be reached at the address tried.
const MANAGER_HOST_HINT: &str =
    "pass --manager-host <address or host name this machine reaches the manager at>";

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
    /// DOTCORE_MACHINE_HOST, else the address the manager observes; bound when it resolves to an
    /// address of this machine.
    #[arg(long, env = "DOTCORE_MACHINE_HOST", value_parser = parse_host)]
    pub host: Option<SiteNodeHost>,

    /// Port of this node's private listener.
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
pub(crate) async fn node_join(paths: &RootPathArgs, arguments: NodeJoinArgs) -> anyhow::Result<()> {
    let mut terminal = if arguments.yes {
        None
    } else if controlling_terminal_condition() == TerminalCondition::Interactive {
        Some(open_terminal())
    } else {
        anyhow::bail!(
            "joining asks for confirmation and needs a terminal; run in a terminal or pass --yes"
        );
    };
    let prober = system_initial_site_prober();
    join_site(paths, arguments, boxed(&mut terminal), &prober).await
}

/// Joins as a new node, or joins again when `--site-dir` holds the intact folder of a node of the
/// token's site.
async fn join_site(
    paths: &RootPathArgs,
    arguments: NodeJoinArgs,
    mut terminal: Option<&mut dyn Terminal>,
    prober: &InitialSiteProber,
) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    let explicit_folder = paths.command_line_site_dir().is_some();
    anyhow::ensure!(
        std::fs::symlink_metadata(&site_dir).is_err() || holds_node_folder(&site_dir),
        "{} already exists; a joined node needs a new directory, pass --site-dir with a path that does not exist",
        site_dir.display()
    );

    let token = read_token(&arguments, lend(&mut terminal))?;
    if std::fs::symlink_metadata(&site_dir).is_ok() {
        let folder = open_rejoining_folder(&site_dir, &token, arguments.name.as_deref())?;
        return rejoin_site(arguments, token, folder, explicit_folder, terminal).await;
    }
    join_new_site(
        arguments,
        token,
        site_dir,
        explicit_folder,
        terminal,
        prober,
    )
    .await
}

async fn join_new_site(
    arguments: NodeJoinArgs,
    token: JoinToken,
    site_dir: PathBuf,
    explicit_folder: bool,
    mut terminal: Option<&mut dyn Terminal>,
    prober: &InitialSiteProber,
) -> anyhow::Result<()> {
    let mut connection = connect_to_manager(&token, arguments.manager_host.clone()).await?;
    announce_connection(&connection, lend(&mut terminal))?;

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
        terminal.message(&Message::Info(APPROVAL_NOTICE.to_owned()))?;
    }

    let folder = loop {
        match connection.join(&proposal, &key, &site_dir).await {
            Ok(folder) => break folder,
            Err(NodeJoinError::DuplicateName(status)) if terminal.is_some() => {
                let terminal = lend(&mut terminal).expect("checked by the guard");
                terminal.message(&Message::Error(status.message().to_owned()))?;
                terminal.message(&Message::Hint(replaced_node_hint(&proposal.display_name)))?;
                proposal.display_name = prompt_name(terminal, None)?;
            }
            Err(error) => return Err(join_failure(error)),
        }
    };

    let command = start_command(&site_dir, explicit_folder);
    warn_about_addresses(&folder, &connection, lend(&mut terminal)).await?;
    if let Some(records) = report_joined(lend(&mut terminal), &proposal, &folder, &command)? {
        write_records(&records)?;
    }
    Ok(())
}

/// Joins the node of the intact folder to the site again, probing and asking nothing: the folder
/// already holds the node's identity, targets, and private port.
async fn rejoin_site(
    arguments: NodeJoinArgs,
    token: JoinToken,
    folder: RejoiningNodeFolder,
    explicit_folder: bool,
    mut terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    let mut connection = connect_to_manager(&token, arguments.manager_host.clone()).await?;
    announce_connection(&connection, lend(&mut terminal))?;
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
        terminal.message(&Message::Info(format!(
            "Joining again from {}",
            folder.root().display()
        )))?;
        terminal.message(&Message::Info(APPROVAL_NOTICE.to_owned()))?;
    }
    let joined = connection
        .rejoin(&proposal, &folder)
        .await
        .map_err(join_failure)?;
    drop(folder);

    let command = start_command(&joined.root, explicit_folder);
    warn_about_addresses(&joined, &connection, lend(&mut terminal)).await?;
    if let Some(records) = report_rejoined(lend(&mut terminal), &joined, &command)? {
        write_records(&records)?;
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

/// Shows which site and manager the token reached.
fn announce_connection(
    connection: &JoinConnection,
    terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
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

/// Warns about addresses of the joined node that the network may not deliver.
async fn warn_about_addresses(
    folder: &JoinedNodeFolder,
    connection: &JoinConnection,
    terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    let mut notices = Notices::new(terminal);
    let resolver = SystemHostResolver;
    if let Some(warning) = folder.address_translation_warning(&resolver).await {
        notices.warn(&warning)?;
    }
    for host in [&folder.host, connection.manager_host()] {
        if let Some(warning) = loopback_host_warning(host, &resolver).await {
            notices.warn(&warning)?;
        }
    }
    Ok(())
}

/// Reports the installed node folder, on the terminal when interactive, else as records.
/// Returns the records to write when there is no terminal.
fn report_joined(
    terminal: Option<&mut dyn Terminal>,
    proposal: &JoinProposal,
    folder: &JoinedNodeFolder,
    command: &str,
) -> anyhow::Result<Option<String>> {
    let Some(terminal) = terminal else {
        return Ok(Some(format!(
            "Node joined\nDOTCORE_SITE_ID={}\nDOTCORE_NODE_ID={}\nDOTCORE_NODE_NAME={}\nDOTCORE_NODE_HOST={}\nDOTCORE_PRIVATE_ADDR={}\nDOTCORE_START_COMMAND={}\n",
            record_value(&folder.site_id)?,
            record_value(&folder.node_id)?,
            record_value(&proposal.display_name)?,
            folder.host,
            folder.private_bind_address,
            record_value(command)?,
        )));
    };
    terminal.message(&Message::Success(format!(
        "Joined {} as {} at {}",
        folder.site_id, proposal.display_name, folder.host
    )))?;
    terminal.message(&Message::Next(vec![
        NextStep::new("Start the node:").with_command(command),
    ]))?;
    Ok(None)
}

/// Reports the rewritten node folder, on the terminal when interactive, else as records.
/// Returns the records to write when there is no terminal.
fn report_rejoined(
    terminal: Option<&mut dyn Terminal>,
    folder: &JoinedNodeFolder,
    command: &str,
) -> anyhow::Result<Option<String>> {
    let Some(terminal) = terminal else {
        return Ok(Some(format!(
            "Node rejoined\nDOTCORE_SITE_ID={}\nDOTCORE_NODE_ID={}\nDOTCORE_NODE_NAME={}\nDOTCORE_NODE_HOST={}\nDOTCORE_PRIVATE_ADDR={}\nDOTCORE_START_COMMAND={}\n",
            record_value(&folder.site_id)?,
            record_value(&folder.node_id)?,
            record_value(&folder.display_name)?,
            folder.host,
            folder.private_bind_address,
            record_value(command)?,
        )));
    };
    terminal.message(&Message::Success(format!(
        "Rejoined {} at {}",
        folder.display_name, folder.host
    )))?;
    terminal.message(&Message::Next(vec![
        NextStep::new("Start the manager on its machine"),
        NextStep::new("Start the node here:").with_command(command),
    ]))?;
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
            Err(error) => terminal.message(&Message::Error(format!(
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

/// Connects once to the join host at the token's host, or at `manager_host` when given. A
/// failure names the address tried, plus the flag to use when the manager was not reachable.
async fn connect_to_manager(
    token: &JoinToken,
    manager_host: Option<SiteNodeHost>,
) -> anyhow::Result<JoinConnection> {
    let mut tried = token.endpoint.clone();
    if let Some(host) = &manager_host {
        let _ = tried.set_host(Some(&host.to_string()));
    }
    JoinConnection::connect(token.clone(), manager_host, DEFAULT_JOIN_CONNECT_TIMEOUT)
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
            Err(message) => terminal.message(&Message::Error(message))?,
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
        (
            "key fingerprint".to_owned(),
            key.fingerprint().as_str().to_owned(),
        ),
    ];
    if proposal.private_port != DEFAULT_PRIVATE_PORT {
        facts.insert(
            2,
            ("private port".to_owned(), proposal.private_port.to_string()),
        );
    }
    terminal.message(&Message::Heading("Proposal".to_owned()))?;
    terminal.message(&Message::KeyValues(facts))?;
    Ok(terminal.confirm(&ConfirmPrompt::new(
        "Send this proposal to the manager?",
        true,
    ))?)
}

/// Adds the command the operator should run next to a join failure.
fn join_failure(error: NodeJoinError) -> anyhow::Error {
    let hint = match &error {
        NodeJoinError::CommitOutcomeUnknown { .. } => {
            "check what `dotc node add` printed on the manager before retrying"
        }
        NodeJoinError::Connect(_) if is_unreachable(&error) => MANAGER_HOST_HINT,
        NodeJoinError::Connect(_) => {
            "check the token and that `dotc node add` is still running on the manager"
        }
        NodeJoinError::DuplicateName(_) => {
            "choose another name with --name, or if this machine replaces that node, run \
             `dotc node remove <node name>` on the manager and join again"
        }
        NodeJoinError::DuplicateHost(_) | NodeJoinError::UnusableHost(_) => {
            "set this machine's address with --host"
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

/// Returns the command that starts the joined node from its folder: the plain command when the
/// folder was not given with `--site-dir` (the default one, or the environment's), else with
/// `--site-dir`.
fn start_command(site_dir: &Path, explicit_folder: bool) -> String {
    if explicit_folder {
        format!("dotcore --site-dir {}", site_dir.display())
    } else {
        "dotcore".to_owned()
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

    use dotcore_domain::ExecutionPlatform;
    use dotcore_proto::dotcore::{node::v1::DeclaredMachine, site::v1::NativeWorkloadLifetime};
    use dotcore_site::initial::inputs::{ApplicationTargetChoice, ModelTargetChoice};

    use super::*;
    use crate::terminal::{ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};

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

        let error = block_on(connect_to_manager(&token, None))
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
    fn an_unreachable_manager_host_flag_is_the_address_reported() {
        let closed = dotcore::testing::reserve_port();
        let port = closed.port();
        let token = token_at(port);

        let error = block_on(connect_to_manager(
            &token,
            Some(SiteNodeHost::parse("127.0.0.2").expect("manager host")),
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
    fn an_interactive_join_report_shows_the_start_command_once_and_writes_no_records() {
        let mut terminal = ScriptedTerminal::interactive([]);

        let records = report_joined(
            Some(&mut terminal),
            &proposal(),
            &joined_folder(),
            "dotcore",
        )
        .expect("report");

        assert_eq!(records, None);
        assert_eq!(
            terminal.lines(),
            [
                "✔ Joined site-1 as gpu-box at 10.0.0.7",
                "Next",
                "  › Start the node:",
                "      dotcore",
            ]
        );
    }

    #[test]
    fn a_rejoin_report_names_the_node_and_both_start_steps() {
        let mut terminal = ScriptedTerminal::interactive([]);

        let records =
            report_rejoined(Some(&mut terminal), &joined_folder(), "dotcore").expect("report");

        assert_eq!(records, None);
        assert_eq!(
            terminal.lines(),
            [
                "✔ Rejoined gpu-box at 10.0.0.7",
                "Next",
                "  › Start the manager on its machine",
                "  › Start the node here:",
                "      dotcore",
            ]
        );
        assert_eq!(
            report_rejoined(None, &joined_folder(), "dotcore --site-dir /srv/node")
                .expect("records")
                .as_deref(),
            Some(
                "Node rejoined\nDOTCORE_SITE_ID=site-1\nDOTCORE_NODE_ID=node-7\nDOTCORE_NODE_NAME=gpu-box\nDOTCORE_NODE_HOST=10.0.0.7\nDOTCORE_PRIVATE_ADDR=10.0.0.7:50053\nDOTCORE_START_COMMAND=dotcore --site-dir /srv/node\n"
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
    fn an_unattended_join_report_is_records_only() {
        let records = report_joined(
            None,
            &proposal(),
            &joined_folder(),
            "dotcore --site-dir /srv/node",
        )
        .expect("report");

        assert_eq!(
            records.as_deref(),
            Some(
                "Node joined\nDOTCORE_SITE_ID=site-1\nDOTCORE_NODE_ID=node-7\nDOTCORE_NODE_NAME=gpu-box\nDOTCORE_NODE_HOST=10.0.0.7\nDOTCORE_PRIVATE_ADDR=10.0.0.7:50053\nDOTCORE_START_COMMAND=dotcore --site-dir /srv/node\n"
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
            std::time::Duration::from_millis(300),
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

        let error = block_on(connect_to_manager(&token, None))
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
                format!("  key fingerprint  {}", key.fingerprint().as_str()),
            ]
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
    fn the_start_command_names_the_folder_only_when_it_is_not_the_default() {
        assert_eq!(
            start_command(Path::new("/home/x/.local/state/dotcore"), false),
            "dotcore"
        );
        assert_eq!(
            start_command(Path::new("/srv/node"), true),
            "dotcore --site-dir /srv/node"
        );
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
