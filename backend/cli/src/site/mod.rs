//! Offline site input initialization, reset and validation.
//!
//! Turns arguments and environment values into an initial site plan, probing the host for the
//! capabilities being selected, then writes or validates the site input bundle without starting any
//! services. Missing values can be collected interactively from the terminal before persisting.

mod collection;
mod delete;
mod init;
mod output;
mod reset;
#[cfg(test)]
pub(crate) mod test_support;

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use dotcore_catalog::OciArtifactPolicy;
use dotcore_domain::{HostResolver, SiteNodeHost};
use dotcore_node_hardware::{MachineDescription, hardware::SystemNvidiaAcceleratorAdapter};
use dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime;
use dotcore_runtime_backends::docker::capabilities::SystemDockerCapabilityProbe;
use dotcore_site::{
    initial::{
        inputs::InitialSiteInputs,
        probe::InitialSiteProber,
        request::{
            InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentityGenerator,
            InitialSiteIdentityKind, InitialSiteRequest, RandomInitialSiteIdentityGenerator,
        },
        validation::{SiteInputBundleValidator, SiteInputValidationError},
    },
    node::hostname::local_node_name,
    validation::is_valid_display_name,
};

use crate::{
    RootPathArgs,
    hosts::{HostEnvironment, ListenerHost, SystemHostEnvironment, host_warning},
    runtime_confirmation::RuntimeRemovalArgs,
    terminal::{TerminalCondition, controlling_terminal_condition},
};
pub(crate) use collection::InitialSiteChoices;
use output::write_validate_success;

/// Commands that initialize, reset, or inspect offline site input bundles.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct SiteCommand {
    #[command(subcommand)]
    command: Option<SiteSubcommand>,
}

/// Actions supported by the site command group.
#[derive(Debug, Subcommand)]
enum SiteSubcommand {
    /// Author an initial site with a site signing key and local node identity.
    Init(SiteInitArgs),

    /// Start a fresh site, the same as `site init --replace --force`: remove its runtime
    /// resources after confirmation, its workload caches, state and trust, then write new inputs.
    /// Previously joined nodes become invalid.
    Reset(SiteResetArgs),

    /// Delete a stopped site: remove its runtime resources and workload caches, then its directory.
    Delete(delete::SiteDeleteArgs),

    /// Validate an existing site input bundle without starting services.
    Validate,
}

/// The site directory and optional platform catalog root an offline site command works on,
/// resolved from the root path options.
#[derive(Clone, Debug)]
pub(crate) struct SiteLocation {
    pub(crate) site_dir: PathBuf,
    pub(crate) catalog_dir: Option<PathBuf>,
}

impl SiteLocation {
    /// Resolves the site directory, falling back to the default one, and the optional catalog.
    pub(crate) fn resolve(paths: &RootPathArgs) -> anyhow::Result<Self> {
        Ok(Self {
            site_dir: paths.site_dir()?,
            catalog_dir: paths.catalog_dir()?,
        })
    }
}

/// Shared initial site configuration collected by init and reset.
#[derive(Debug, Args)]
pub struct InitialSiteConfigurationArgs {
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

    /// Optional advanced site identity override; otherwise a valid ID is generated.
    #[arg(long, value_name = "SITE_ID", value_parser = parse_identifier)]
    pub site_id: Option<String>,

    /// Optional advanced SiteNode identity override; otherwise a valid ID is generated.
    #[arg(long, value_name = "NODE_ID", value_parser = parse_identifier)]
    pub node_id: Option<String>,

    /// SiteNode display name; defaults to this machine's hostname without a trailing `.local`.
    #[arg(long, value_name = "NAME", value_parser = parse_display_name)]
    pub display_name: Option<String>,

    /// SiteNode host other nodes reach this node at, an IPv4 address or DNS host name; always
    /// wins, but must resolve to the address the server listener variables (DOTCORE_*_ADDR)
    /// name when they name a specific one. Without it: that address, else the recommended address
    /// of this machine: its tailnet name when MagicDNS resolves it here, else its tailnet IPv4,
    /// else its LAN IPv4, else 127.0.0.1. Start the server after Tailscale when the host is a
    /// tailnet name.
    #[arg(long, value_parser = parse_host)]
    pub host: Option<SiteNodeHost>,

    /// Advanced NVIDIA CDI selector override, such as nvidia.com/gpu=gpu1, picking the GPUs the
    /// node offers: each gets one model target, and the docker application target, when offered,
    /// holds them all. Without it, every GPU is picked. Repeat once per selector.
    #[arg(
        long,
        value_name = "CDI_SELECTOR",
        action = clap::ArgAction::Append
    )]
    pub nvidia_device: Vec<String>,

    /// Whether native workloads outlive the server process; only with --inference apple.
    /// Defaults to persistent.
    #[arg(long, value_enum)]
    pub native_workload_lifetime: Option<NativeWorkloadLifetimeChoice>,

    /// Require deterministic non-interactive behavior.
    #[arg(long)]
    pub non_interactive: bool,

    /// Force guided configuration when standard input and diagnostics are terminals.
    #[arg(long, conflicts_with = "non_interactive")]
    pub interactive: bool,
}

/// Initial site authoring arguments.
#[derive(Debug, Args)]
pub struct SiteInitArgs {
    #[command(flatten)]
    pub configuration: InitialSiteConfigurationArgs,

    /// Change an existing site's configuration in place, keeping its trust and joined nodes.
    /// Refuses when the site has deployments, retained state or runtime resources.
    #[arg(long)]
    pub replace: bool,

    /// Initialize interactively only when no site input bundle exists.
    #[arg(long, conflicts_with_all = ["replace", "force"])]
    pub if_missing: bool,

    /// With --replace, start a fresh site instead, the same as `dotc site reset`: remove its
    /// runtime resources after confirmation, its deployments, state and trust.
    #[arg(long)]
    pub force: bool,

    #[command(flatten)]
    pub removal: RuntimeRemovalArgs,
}

/// Destructive offline reset-and-initialize arguments.
#[derive(Debug, Args)]
pub struct SiteResetArgs {
    #[command(flatten)]
    pub configuration: InitialSiteConfigurationArgs,

    #[command(flatten)]
    pub removal: RuntimeRemovalArgs,
}

/// Inference choices exposed by the initial site and join CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum InferenceChoice {
    /// Deterministic local inference capability, for development.
    Simulated,

    /// NVIDIA Docker inference capability.
    Nvidia,

    /// Native inference on the Apple GPU of a macOS node.
    Apple,

    /// No local inference capability.
    None,
}

impl InferenceChoice {
    /// Returns the inference capabilities repeated choices select, or `None` when no choice was
    /// given. `none` stands alone and selects no capability.
    pub(crate) fn capabilities(
        choices: &[Self],
    ) -> anyhow::Result<Option<BTreeSet<InitialInferenceRuntime>>> {
        selected_capabilities(choices, "--inference", |choice| match choice {
            Self::Simulated => Some(InitialInferenceRuntime::Simulated),
            Self::Nvidia => Some(InitialInferenceRuntime::Nvidia),
            Self::Apple => Some(InitialInferenceRuntime::Apple),
            Self::None => None,
        })
    }
}

/// One `--inference` value: an inference choice and, for `simulated:<name>`, the simulated
/// hardware it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceArgument {
    /// The inference choice.
    pub choice: InferenceChoice,

    /// Simulated hardware named by `simulated:<name>`; `None` for every other value.
    pub simulated: Option<String>,
}

impl InferenceArgument {
    /// Returns the distinct choices of repeated arguments, in the order first given, so several
    /// `simulated:<name>` values offer simulated inference once.
    pub(crate) fn choices(arguments: &[Self]) -> Vec<InferenceChoice> {
        let mut choices = Vec::new();
        for argument in arguments {
            if !choices.contains(&argument.choice) {
                choices.push(argument.choice);
            }
        }
        choices
    }

    /// Returns whether the arguments offer `choice`.
    pub(crate) fn offers(arguments: &[Self], choice: InferenceChoice) -> bool {
        arguments.iter().any(|argument| argument.choice == choice)
    }

    /// Returns the simulated hardware the arguments name, in order, or `None` when they name
    /// none; a bare `simulated` among named ones stands for the default hardware.
    pub(crate) fn simulated_hardware(arguments: &[Self]) -> Option<Vec<String>> {
        if !arguments
            .iter()
            .any(|argument| argument.simulated.is_some())
        {
            return None;
        }
        Some(
            arguments
                .iter()
                .filter(|argument| argument.choice == InferenceChoice::Simulated)
                .map(|argument| {
                    argument.simulated.clone().unwrap_or_else(|| {
                        InitialSiteRequest::DEFAULT_SIMULATED_HARDWARE.to_owned()
                    })
                })
                .collect(),
        )
    }
}

impl From<InferenceChoice> for InferenceArgument {
    fn from(choice: InferenceChoice) -> Self {
        Self {
            choice,
            simulated: None,
        }
    }
}

/// Parses one `--inference` value: a choice, or `simulated:<name>`.
pub(crate) fn parse_inference(value: &str) -> Result<InferenceArgument, String> {
    if let Some(name) = value.strip_prefix("simulated:") {
        if name.is_empty() {
            return Err("simulated:<name> needs a hardware name".to_owned());
        }
        return Ok(InferenceArgument {
            choice: InferenceChoice::Simulated,
            simulated: Some(name.to_owned()),
        });
    }
    InferenceChoice::from_str(value, false).map(InferenceArgument::from)
}

/// The long help of `--inference`, listing the simulated hardware compiled into this binary.
pub(crate) fn inference_long_help() -> String {
    let hardware = MachineDescription::simulated_choices()
        .into_iter()
        .map(|choice| format!("  {}: {}", choice.name, choice.summary))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Inference capability the node offers: simulated, nvidia, apple or none; repeat to offer \
         several, or pass none for a node that runs no local models.\n\n\
         `simulated:<name>` simulates hardware in place of the real GPUs, for development. Repeat \
         it to put several GPUs on one simulated host; a whole machine stands alone. Bare \
         `simulated` is `simulated:{}`. Simulated hardware can't be combined with nvidia or \
         apple.\n\nHardware:\n{hardware}",
        InitialSiteRequest::DEFAULT_SIMULATED_HARDWARE
    )
}

/// Native workload lifetimes exposed by the initial site and join CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NativeWorkloadLifetimeChoice {
    /// Workloads keep running across server restarts and are adopted again.
    Persistent,

    /// Workloads stop once the server process that created them exits.
    Ephemeral,
}

impl NativeWorkloadLifetimeChoice {
    /// Returns the site declaration this choice selects.
    pub(crate) fn to_lifetime(self) -> NativeWorkloadLifetime {
        match self {
            Self::Persistent => NativeWorkloadLifetime::Persistent,
            Self::Ephemeral => NativeWorkloadLifetime::Ephemeral,
        }
    }
}

/// Applications choices exposed by the initial site and join CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ApplicationsChoice {
    /// Deterministic local applications capability, for development.
    Simulated,

    /// Docker-backed applications capability.
    Docker,

    /// No applications capability.
    None,
}

impl ApplicationsChoice {
    /// Returns the application capabilities repeated choices select, or `None` when no choice
    /// was given. `none` stands alone and selects no capability.
    pub(crate) fn capabilities(
        choices: &[Self],
    ) -> anyhow::Result<Option<BTreeSet<InitialApplicationRuntime>>> {
        selected_capabilities(choices, "--applications", |choice| match choice {
            Self::Simulated => Some(InitialApplicationRuntime::Simulated),
            Self::Docker => Some(InitialApplicationRuntime::Docker),
            Self::None => None,
        })
    }
}

/// Maps repeated capability choices to a set, where `capability` returns `None` for the `none`
/// choice, which must stand alone.
fn selected_capabilities<Choice: Copy, Capability: Ord>(
    choices: &[Choice],
    flag: &str,
    capability: impl Fn(Choice) -> Option<Capability>,
) -> anyhow::Result<Option<BTreeSet<Capability>>> {
    if choices.is_empty() {
        return Ok(None);
    }
    let selected = choices
        .iter()
        .copied()
        .map(&capability)
        .collect::<Option<BTreeSet<_>>>();
    match selected {
        Some(capabilities) => Ok(Some(capabilities)),
        None if choices.iter().all(|choice| capability(*choice).is_none()) => {
            Ok(Some(BTreeSet::new()))
        }
        None => bail!("{flag} none cannot be combined with another {flag} value"),
    }
}

/// Validated in-memory initial site inputs ready for the shared persistence boundary.
pub(crate) struct PreparedInitialSite {
    pub(crate) request: InitialSiteRequest,
    pub(crate) inputs: InitialSiteInputs,

    /// What breaks with the node host, for the caller to show before persisting.
    pub(crate) host_warning: Option<String>,
}

impl SiteCommand {
    pub(crate) async fn dispatch(self, paths: &RootPathArgs) -> anyhow::Result<()> {
        match self.command {
            Some(SiteSubcommand::Init(arguments)) => init::initialize(arguments, paths).await,
            Some(SiteSubcommand::Reset(arguments)) => reset::reset(arguments, paths).await,
            Some(SiteSubcommand::Delete(arguments)) => delete::delete(arguments, paths).await,
            Some(SiteSubcommand::Validate) => validate(paths).context("site validate failed"),
            None => bail!("site command requires an action"),
        }
    }

    pub(crate) fn argument_contract_error(&self) -> Option<&'static str> {
        match &self.command {
            Some(SiteSubcommand::Init(arguments)) => {
                if arguments.force && !arguments.replace {
                    return Some("--force requires --replace");
                }
                if (arguments.removal.yes || arguments.removal.skip_runtime_cleanup)
                    && !arguments.force
                {
                    return Some("--yes and --skip-runtime-cleanup require --replace --force");
                }
                initial_configuration_argument_error(&arguments.configuration)
            }
            Some(SiteSubcommand::Reset(arguments)) => {
                initial_configuration_argument_error(&arguments.configuration)
            }
            Some(SiteSubcommand::Delete(_) | SiteSubcommand::Validate) | None => None,
        }
    }
}

fn initial_configuration_argument_error(
    arguments: &InitialSiteConfigurationArgs,
) -> Option<&'static str> {
    if !arguments.nvidia_device.is_empty()
        && !arguments.inference.is_empty()
        && !InferenceArgument::offers(&arguments.inference, InferenceChoice::Nvidia)
    {
        return Some("--nvidia-device requires --inference nvidia");
    }
    if arguments.native_workload_lifetime.is_some()
        && !arguments.inference.is_empty()
        && !InferenceArgument::offers(&arguments.inference, InferenceChoice::Apple)
    {
        return Some("--native-workload-lifetime requires --inference apple");
    }
    if InferenceArgument::offers(&arguments.inference, InferenceChoice::None)
        && arguments
            .inference
            .iter()
            .any(|argument| argument.choice != InferenceChoice::None)
    {
        return Some("--inference none cannot be combined with another --inference value");
    }
    if arguments.applications.contains(&ApplicationsChoice::None)
        && arguments
            .applications
            .iter()
            .any(|choice| *choice != ApplicationsChoice::None)
    {
        return Some("--applications none cannot be combined with another --applications value");
    }
    let missing_choices = arguments.inference.is_empty() || arguments.applications.is_empty();
    if missing_choices
        && (arguments.non_interactive
            || (!arguments.interactive && !controlling_terminal_available()))
    {
        return Some(
            "missing workload choices; pass --inference and --applications or use --interactive in a TTY",
        );
    }
    None
}

fn controlling_terminal_available() -> bool {
    controlling_terminal_condition() == TerminalCondition::Interactive
}

/// Returns the host this process's listener variables require, rejecting a `--host` that the
/// server does not listen on.
async fn system_listener_host(
    arguments: &InitialSiteConfigurationArgs,
) -> anyhow::Result<Option<ListenerHost>> {
    ListenerHost::constrain(arguments.host.as_ref(), &SystemHostEnvironment).await
}

/// Probes Docker, NVIDIA, and, on macOS, the Apple GPU of this machine.
pub(crate) fn system_initial_site_prober() -> InitialSiteProber {
    let prober = InitialSiteProber::new(
        Box::new(SystemDockerCapabilityProbe),
        Box::new(SystemNvidiaAcceleratorAdapter::for_current_host()),
    );
    #[cfg(target_os = "macos")]
    let prober = prober.with_apple_accelerator(Box::new(
        dotcore_node_hardware::apple::SystemAppleAcceleratorAdapter::for_current_host(),
    ));
    prober
}

/// Checks whether other machines can reach the request's node host and the server can listen
/// on it, then probes this machine and composes the validated in-memory site.
pub(crate) async fn prepare_initial_site(
    request: InitialSiteRequest,
    prober: &InitialSiteProber,
    environment: &dyn HostEnvironment,
    identity_generator: &mut dyn InitialSiteIdentityGenerator,
) -> anyhow::Result<PreparedInitialSite> {
    let host_warning = host_warning(&request.site_node_host(), environment).await;
    let probe = prober.probe(&request).await?;
    let identities = request.resolve_identities(identity_generator)?;
    let inputs = InitialSiteInputs::compose(&request, identities, &probe)?;
    Ok(PreparedInitialSite {
        request,
        inputs,
        host_warning,
    })
}

/// Returns the node host an existing site at `site_dir` records for this machine's node; `None`
/// when no valid site is there.
pub(crate) fn recorded_host(site_dir: &Path) -> Option<SiteNodeHost> {
    let inputs =
        InitialSiteInputs::load(site_dir, &dotcore::model::site_declaration_rules()).ok()?;
    SiteNodeHost::parse(&output::local_node(&inputs).ok()?.host).ok()
}

/// Describes why other machines cannot reach `host` when it is a host name that resolves to a
/// loopback address on this machine; `None` otherwise, including when it does not resolve here.
pub(crate) async fn loopback_host_warning(
    host: &SiteNodeHost,
    resolver: &dyn HostResolver,
) -> Option<String> {
    host.resolve_with_loopback_warning(resolver).await.ok()?.1
}

/// Random site identities that suggest this machine's name for the SiteNode; the SiteNode and
/// target identities are derived from the site identity and that name.
struct HostAwareIdentityGenerator {
    random: RandomInitialSiteIdentityGenerator,
}

impl HostAwareIdentityGenerator {
    fn new() -> Self {
        Self {
            random: RandomInitialSiteIdentityGenerator,
        }
    }
}

impl InitialSiteIdentityGenerator for HostAwareIdentityGenerator {
    fn generate(&mut self, kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
        self.random.generate(kind)
    }

    fn node_display_name(&self) -> Option<String> {
        local_node_name()
    }
}

fn validate(paths: &RootPathArgs) -> anyhow::Result<()> {
    let location = SiteLocation::resolve(paths)?;
    let site_dir = location.site_dir.as_path();
    validator(location.catalog_dir.as_deref())
        .validate_without_runtime_declarations(site_dir)
        .map_err(|error| validation_failure(error, site_dir))?;
    write_validate_success(site_dir)
}

fn validator(catalog_dir: Option<&Path>) -> SiteInputBundleValidator {
    let validator = SiteInputBundleValidator::new(
        dotcore::model::engine::declaration_validator(),
        dotcore::model::site_declaration_rules(),
    )
    .with_oci_artifact_policy(OciArtifactPolicy::Development);
    match catalog_dir {
        Some(catalog_dir) => validator.with_platform_catalog_root(catalog_dir),
        None => validator,
    }
}

fn validation_failure(error: SiteInputValidationError, site_dir: &Path) -> anyhow::Error {
    let source = match &error {
        SiteInputValidationError::Site(_) | SiteInputValidationError::RuntimeDeclarations => {
            "site.yaml"
        }
        SiteInputValidationError::NodeConfiguration(_)
        | SiteInputValidationError::NodeBinding(_) => "node.yaml",
        SiteInputValidationError::SiteCatalog(_) => "Site catalog",
        SiteInputValidationError::CatalogComposition(_) => "platform and site catalog sources",
    };
    anyhow::Error::new(error).context(format!(
        "{source} validation failed under {}",
        site_dir.display()
    ))
}

pub(crate) fn parse_absolute_path(value: &str) -> Result<PathBuf, String> {
    if value.contains(['\r', '\n']) {
        return Err("path must not contain carriage returns or newlines".to_owned());
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err("path must be absolute".to_owned());
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

/// Parses a host flag: a specific IPv4 address or a DNS host name, recorded lowercase without a
/// trailing dot.
pub(crate) fn parse_host(value: &str) -> Result<SiteNodeHost, String> {
    SiteNodeHost::normalize(value).map_err(|error| format!("host {error}"))
}

pub(crate) fn parse_display_name(value: &str) -> Result<String, String> {
    if is_valid_display_name(value) {
        Ok(value.trim().to_owned())
    } else {
        Err("display name must contain 1 to 128 non-control characters".to_owned())
    }
}

fn parse_identifier(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let valid = (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes
            .iter()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || *value == b'-');
    if valid {
        Ok(value.to_owned())
    } else {
        Err("ID must match [a-z][a-z0-9-]{0,62}".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, net::Ipv4Addr};

    use clap::error::ErrorKind;
    use dotcore::testing::FakeHostResolver;
    use dotcore_node_hardware::MachineDescription;

    use super::{
        ApplicationsChoice, InferenceChoice, InitialInferenceRuntime, NativeWorkloadLifetimeChoice,
        SiteNodeHost, SiteSubcommand, inference_long_help, test_support::flagged_request,
    };
    use crate::{Cli, Command};

    fn init(arguments: &[&str]) -> super::SiteInitArgs {
        let cli = Cli::try_parse_from(arguments).expect("site init arguments");
        let Command::Site(site) = cli.command else {
            panic!("Site command")
        };
        let Some(SiteSubcommand::Init(arguments)) = site.command else {
            panic!("Site init command")
        };
        arguments
    }

    #[test]
    fn parses_independent_workload_choices_and_advanced_defaults() {
        let arguments = init(&[
            "dotc",
            "site",
            "init",
            "--inference",
            "nvidia",
            "--applications",
            "docker",
            "--nvidia-device",
            "nvidia.com/gpu=gpu0",
            "--nvidia-device",
            "nvidia.com/gpu=gpu1",
        ]);

        assert_eq!(
            arguments.configuration.inference,
            [InferenceChoice::Nvidia.into()]
        );
        assert_eq!(
            arguments.configuration.applications,
            [ApplicationsChoice::Docker]
        );
        assert_eq!(arguments.configuration.host, None);
        assert_eq!(
            arguments.configuration.nvidia_device,
            ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=gpu1"]
        );
    }

    #[test]
    fn nvidia_selectors_are_rejected_for_simulated_inference_at_parse_boundary() {
        let error = Cli::try_parse_from([
            "dotc",
            "site",
            "init",
            "--site-dir",
            "/tmp/site",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--nvidia-device",
            "nvidia.com/gpu=gpu0",
        ])
        .expect_err("NVIDIA selector without NVIDIA mode");

        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("requires --inference nvidia"));
    }

    #[test]
    fn native_workload_lifetime_parses_and_requires_apple_inference() {
        let arguments = init(&[
            "dotc",
            "site",
            "init",
            "--site-dir",
            "/tmp/site",
            "--inference",
            "apple",
            "--applications",
            "simulated",
            "--native-workload-lifetime",
            "ephemeral",
        ]);
        assert_eq!(
            arguments.configuration.inference,
            [InferenceChoice::Apple.into()]
        );
        assert_eq!(
            arguments.configuration.native_workload_lifetime,
            Some(NativeWorkloadLifetimeChoice::Ephemeral)
        );

        let error = Cli::try_parse_from([
            "dotc",
            "site",
            "init",
            "--site-dir",
            "/tmp/site",
            "--inference",
            "nvidia",
            "--applications",
            "simulated",
            "--native-workload-lifetime",
            "persistent",
        ])
        .expect_err("lifetime without Apple inference");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        assert!(
            error
                .to_string()
                .contains("--native-workload-lifetime requires --inference apple")
        );
    }

    #[test]
    fn simulated_hardware_names_reach_the_request_in_order() {
        let parse = |inference: &[&str]| {
            let mut arguments = vec!["dotc", "site", "init", "--site-dir", "/tmp/site"];
            for value in inference {
                arguments.extend(["--inference", value]);
            }
            arguments.extend(["--applications", "docker", "--host", "192.0.2.10"]);
            flagged_request(&init(&arguments).configuration)
        };

        let named = parse(&["simulated:rtx-4090", "simulated:rtx-4080"]);
        let bare = parse(&["simulated"]);
        let repeated = parse(&["simulated:rtx-5090", "simulated:rtx-5090"]);
        let mixed = parse(&["simulated", "simulated:rtx-4090"]);

        assert_eq!(
            named.inference(),
            &BTreeSet::from([InitialInferenceRuntime::Simulated])
        );
        assert_eq!(named.simulated_hardware(), ["rtx-4090", "rtx-4080"]);
        assert_eq!(bare.simulated_hardware(), ["rtx-5090"]);
        assert_eq!(repeated.simulated_hardware(), ["rtx-5090", "rtx-5090"]);
        assert_eq!(mixed.simulated_hardware(), ["rtx-5090", "rtx-4090"]);
    }

    #[test]
    fn an_empty_or_unknown_inference_value_is_invalid() {
        for value in ["simulated:", "nvidia:rtx-5090", "gpu"] {
            let error = Cli::try_parse_from([
                "dotc",
                "site",
                "init",
                "--site-dir",
                "/tmp/site",
                "--inference",
                value,
                "--applications",
                "docker",
            ])
            .expect_err(value);
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{value}");
        }
    }

    #[test]
    fn the_inference_help_lists_the_simulated_hardware() {
        let help = inference_long_help();

        for choice in MachineDescription::simulated_choices() {
            assert!(
                help.contains(&format!("  {}: {}", choice.name, choice.summary)),
                "{help}"
            );
        }
    }

    #[test]
    fn missing_noninteractive_choices_are_invalid_usage() {
        for arguments in [
            &[
                "dotc",
                "site",
                "init",
                "--site-dir",
                "/tmp/site",
                "--non-interactive",
            ][..],
            &[
                "dotc",
                "site",
                "init",
                "--site-dir",
                "/tmp/site",
                "--inference",
                "simulated",
                "--non-interactive",
            ][..],
        ] {
            let error = Cli::try_parse_from(arguments).expect_err("conflicting site init options");
            assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn interactive_flag_is_accepted_for_phase_six_guided_execution() {
        let command = Cli::try_parse_from(["dotc", "site", "init", "--interactive"])
            .expect("interactive init arguments");
        assert!(matches!(command.command, crate::Command::Site(_)));
    }

    #[test]
    fn parser_callbacks_emit_stable_source_independent_messages() {
        assert_eq!(
            super::parse_absolute_path("relative"),
            Err("path must be absolute".to_owned())
        );
        assert_eq!(
            super::parse_host("not_a_host"),
            Err("host must be an IPv4 address or a DNS host name".to_owned())
        );
        assert_eq!(
            super::parse_host("::1"),
            Err("host must be an IPv4 address or a DNS host name, not an IPv6 address".to_owned())
        );
        assert_eq!(
            super::parse_host("Manager.Example.TS.net.").map(|host| host.to_string()),
            Ok("manager.example.ts.net".to_owned())
        );
    }

    #[tokio::test]
    async fn only_host_names_resolving_to_loopback_warn() {
        let loopback = FakeHostResolver::default().with("manager", [Ipv4Addr::new(127, 0, 0, 2)]);
        let tailnet = FakeHostResolver::default().with("manager", [Ipv4Addr::new(100, 64, 0, 7)]);
        let name = SiteNodeHost::parse("manager").expect("name");

        assert_eq!(
            super::loopback_host_warning(&name, &loopback)
                .await
                .as_deref(),
            Some(
                "host name manager resolves to the loopback address 127.0.0.2 on this machine, \
                 which other machines cannot reach; use its fully qualified name"
            )
        );
        assert_eq!(super::loopback_host_warning(&name, &tailnet).await, None);
        assert_eq!(
            super::loopback_host_warning(&SiteNodeHost::LOOPBACK, &loopback).await,
            None
        );
    }

    #[test]
    fn unsafe_hosts_are_rejected_at_parse_time() {
        let error = Cli::try_parse_from([
            "dotc",
            "site",
            "init",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--host",
            "0.0.0.0",
        ])
        .expect_err("unsafe host");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn identity_overrides_use_the_existing_site_identifier_rule() {
        let error = Cli::try_parse_from([
            "dotc",
            "site",
            "init",
            "--site-dir",
            "/tmp/site",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
            "--site-id",
            "Invalid_site",
        ])
        .expect_err("invalid site ID");

        assert_eq!(error.kind(), ErrorKind::ValueValidation);
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("[a-z][a-z0-9-]{0,62}"));
    }

    #[test]
    fn reset_and_init_share_the_same_configuration_argument_contract() {
        let common = [
            "--site-dir",
            "/tmp/shared-site",
            "--catalog-dir",
            "/tmp/shared-catalog",
            "--inference",
            "simulated",
            "--applications",
            "docker",
            "--site-id",
            "shared-site",
            "--node-id",
            "shared-node",
            "--display-name",
            "Shared node",
            "--host",
            "192.0.2.10",
            "--non-interactive",
        ];
        let mut init_arguments = vec!["dotc", "site", "init"];
        init_arguments.extend(common);
        let mut reset_arguments = vec!["dotc", "site", "reset"];
        reset_arguments.extend(common);
        let init = Cli::try_parse_from(init_arguments).expect("init arguments");
        let reset = Cli::try_parse_from(reset_arguments).expect("reset arguments");
        let Command::Site(init) = init.command else {
            panic!("Site init command")
        };
        let Command::Site(reset) = reset.command else {
            panic!("Site reset command")
        };
        let Some(SiteSubcommand::Init(init)) = init.command else {
            panic!("init action")
        };
        let Some(SiteSubcommand::Reset(reset)) = reset.command else {
            panic!("reset action")
        };

        let init_request = flagged_request(&init.configuration);
        let reset_request = flagged_request(&reset.configuration);
        assert_eq!(init_request.inference(), reset_request.inference());
        assert_eq!(init_request.applications(), reset_request.applications());
        assert_eq!(
            init_request.site_id_override(),
            reset_request.site_id_override()
        );
        assert_eq!(
            init_request.node_id_override(),
            reset_request.node_id_override()
        );
        assert_eq!(
            init_request.site_node_host_override(),
            reset_request.site_node_host_override()
        );
        assert_eq!(
            init_request.site_node_display_name_override(),
            Some("Shared node")
        );
        assert_eq!(
            reset_request.site_node_display_name_override(),
            Some("Shared node")
        );
    }

    #[test]
    fn invalid_display_names_are_rejected_at_parse_time_on_init_and_reset() {
        for command in ["init", "reset"] {
            for value in ["", "   ", "bad\u{7}name", &"a".repeat(129)] {
                let error = Cli::try_parse_from([
                    "dotc",
                    "site",
                    command,
                    "--site-dir",
                    "/tmp/site",
                    "--inference",
                    "simulated",
                    "--applications",
                    "simulated",
                    "--display-name",
                    value,
                ])
                .expect_err("invalid display name");

                assert_eq!(error.kind(), ErrorKind::ValueValidation, "{value:?}");
                assert!(
                    error
                        .to_string()
                        .contains("display name must contain 1 to 128 non-control characters"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn display_name_flag_is_trimmed() {
        assert_eq!(
            super::parse_display_name("  Test node \n"),
            Ok("Test node".to_owned())
        );
    }

    #[test]
    fn validate_has_only_path_inputs_and_no_initialization_choices() {
        let cli = Cli::try_parse_from([
            "dotc",
            "site",
            "validate",
            "--site-dir",
            "/tmp/site",
            "--catalog-dir",
            "/tmp/catalog",
        ])
        .expect("site validate arguments");
        assert!(matches!(cli.command, Command::Site(_)));
    }

    #[test]
    fn help_describes_composable_choices_and_advanced_controls() {
        let error =
            Cli::try_parse_from(["dotc", "site", "init", "--help"]).expect_err("site init help");
        let output = error.to_string();
        for term in [
            "simulated",
            "nvidia",
            "applications",
            "--site-id",
            "--node-id",
            "--display-name",
            "127.0.0.1",
            "--nvidia-device",
            "--replace",
            "--force",
            "--non-interactive",
        ] {
            assert!(output.contains(term), "help missing {term:?}: {output}");
        }
    }
}
