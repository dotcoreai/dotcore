//! Interactive collection of an initial site plan.
//!
//! Asks the operator on the controlling terminal for the values the arguments did not supply,
//! recommends capabilities by probing the host, and composes a fully validated in-memory site plan.
//! Nothing is written here; persistence is left to the caller after the operator confirms the plan.

mod capability;
mod host;
mod lifetime;
mod node_name;
mod summary;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime;
use dotcore_site::initial::{
    inputs::InitialSiteInputs,
    probe::InitialSiteProber,
    request::{
        InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentityGenerator,
        InitialSiteRequest,
    },
};

use crate::{
    hosts::{HostCandidate, HostEnvironment, SelectedHost},
    site::{InitialSiteChoices, PreparedInitialSite, prepare_initial_site},
    terminal::{ConfirmPrompt, Message, Terminal},
};
pub(crate) use capability::{
    CapabilityRecommendations, InferencePick, collect_applications, collect_inference, in_dev_env,
};
use host::collect_host;
use lifetime::collect_native_workload_lifetime;
use node_name::collect_node_name;
use summary::display_summary;

/// Pre-resolved CLI and environment values that guided collection must not ask for. The site
/// directory is always resolved before collection, so it is never asked for.
#[derive(Clone, Debug)]
pub struct GuidedInitialSiteDefaults {
    site_dir: PathBuf,
    host: SelectedHost,
    host_candidates: Vec<HostCandidate>,
    inference: Option<BTreeSet<InitialInferenceRuntime>>,
    applications: Option<BTreeSet<InitialApplicationRuntime>>,
    site_id: Option<String>,
    node_id: Option<String>,
    node_name: Option<String>,
    nvidia_cdi_selectors: Option<Vec<String>>,
    native_workload_lifetime: Option<NativeWorkloadLifetime>,
    simulated_hardware: Option<Vec<String>>,
    development: bool,
}

impl GuidedInitialSiteDefaults {
    /// Starts from the resolved site directory and the loopback host offered as the default
    /// with no other candidate, with every other value left to ask for.
    pub fn new(site_dir: impl Into<PathBuf>) -> Self {
        Self {
            site_dir: site_dir.into(),
            host: SelectedHost::loopback(),
            host_candidates: Vec::new(),
            inference: None,
            applications: None,
            site_id: None,
            node_id: None,
            node_name: None,
            nvidia_cdi_selectors: None,
            native_workload_lifetime: None,
            simulated_hardware: None,
            development: false,
        }
    }

    /// Supplies the selected node host: a named host is used without prompting, a host the
    /// listener variables require is shown as fixed, and a default is offered first in a choice.
    pub fn with_host(mut self, host: SelectedHost) -> Self {
        self.host = host;
        self
    }

    /// Supplies this machine's host candidates in rank order, listed after an offered default.
    pub fn with_host_candidates(mut self, candidates: Vec<HostCandidate>) -> Self {
        self.host_candidates = candidates;
        self
    }

    /// Supplies the resolved inference capabilities, possibly none, without prompting.
    pub fn with_inference(
        mut self,
        inference: impl IntoIterator<Item = InitialInferenceRuntime>,
    ) -> Self {
        self.inference = Some(inference.into_iter().collect());
        self
    }

    /// Supplies the resolved application capabilities, possibly none, without prompting.
    pub fn with_applications(
        mut self,
        applications: impl IntoIterator<Item = InitialApplicationRuntime>,
    ) -> Self {
        self.applications = Some(applications.into_iter().collect());
        self
    }

    /// Supplies an advanced site identity override without prompting.
    pub fn with_site_id(mut self, site_id: impl Into<String>) -> Self {
        self.site_id = Some(site_id.into());
        self
    }

    /// Supplies an advanced SiteNode identity override without prompting.
    pub fn with_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }

    /// Supplies an explicit SiteNode name without prompting.
    pub fn with_node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = Some(name.into());
        self
    }

    /// Supplies advanced NVIDIA CDI selectors without prompting.
    pub fn with_nvidia_cdi_selectors(
        mut self,
        selectors: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.nvidia_cdi_selectors = Some(selectors.into_iter().map(Into::into).collect());
        self
    }

    /// Supplies the lifetime of native workloads without prompting.
    pub fn with_native_workload_lifetime(mut self, lifetime: NativeWorkloadLifetime) -> Self {
        self.native_workload_lifetime = Some(lifetime);
        self
    }

    /// Offers simulated inference among the accelerators, as a development workspace does.
    pub fn with_development(mut self, development: bool) -> Self {
        self.development = development;
        self
    }

    /// Supplies the simulated hardware simulated inference declares, instead of the default.
    pub fn with_simulated_hardware(mut self, hardware: Vec<String>) -> Self {
        self.simulated_hardware = Some(hardware);
        self
    }
}

/// A validated initial site plan whose persistence remains the caller's responsibility.
pub struct GuidedInitialSitePlan {
    site_dir: PathBuf,
    host: SelectedHost,
    request: InitialSiteRequest,
    inputs: InitialSiteInputs,
}

impl GuidedInitialSitePlan {
    /// Returns the selected site input root.
    pub fn site_dir(&self) -> &Path {
        &self.site_dir
    }

    /// Returns the selected node host and where it came from.
    pub fn host(&self) -> &SelectedHost {
        &self.host
    }

    /// Returns the typed request shared with non-interactive initialization.
    pub fn request(&self) -> &InitialSiteRequest {
        &self.request
    }

    /// Returns the validated in-memory inputs shared with non-interactive initialization.
    pub fn inputs(&self) -> &InitialSiteInputs {
        &self.inputs
    }

    /// Returns the request and inputs ready for the shared persistence boundary.
    pub(crate) fn into_prepared(self) -> PreparedInitialSite {
        PreparedInitialSite {
            request: self.request,
            inputs: self.inputs,
            // Guided collection shows it before the confirmation.
            host_warning: None,
        }
    }
}

/// The operator's final guided confirmation decision.
pub enum GuidedInitialSiteCollection {
    /// The operator confirmed a validated in-memory site plan.
    Confirmed(Box<GuidedInitialSitePlan>),

    /// The operator declined before any persistence boundary.
    Declined,
}

/// Collects, probes, composes, and confirms a guided initial site plan without writing files.
pub async fn collect_guided_initial_site(
    defaults: GuidedInitialSiteDefaults,
    terminal: &mut dyn Terminal,
    environment: &dyn HostEnvironment,
    prober: &InitialSiteProber,
    identity_generator: &mut dyn InitialSiteIdentityGenerator,
) -> anyhow::Result<GuidedInitialSiteCollection> {
    let host = collect_host(defaults.host.clone(), &defaults.host_candidates, terminal)?;
    let node_name = collect_node_name(
        defaults.node_name.clone(),
        identity_generator.node_display_name().as_deref(),
        terminal,
    )?;
    let recommendations = CapabilityRecommendations::collect(
        prober,
        defaults.inference.is_none(),
        defaults.applications.is_none(),
    )
    .await;
    let inference = collect_inference(
        defaults.inference.clone(),
        defaults.development,
        terminal,
        &recommendations,
    )?;
    let lifetime = collect_native_workload_lifetime(
        defaults.native_workload_lifetime,
        &inference.inference,
        terminal,
    )?;
    let applications =
        collect_applications(defaults.applications.clone(), terminal, &recommendations)?;
    let request = choices_from_defaults(
        &defaults,
        host.clone(),
        node_name,
        inference,
        applications,
        lifetime,
    )?
    .request()?;
    let prepared = prepare_initial_site(request, prober, environment, identity_generator).await?;
    let host_warning = prepared.host_warning.clone();
    let plan = plan_from_prepared(defaults.site_dir.clone(), host, prepared);
    display_summary(terminal, &plan)?;
    if let Some(warning) = host_warning {
        terminal.message(&Message::Warning(warning))?;
    }

    if confirm(terminal)? {
        Ok(GuidedInitialSiteCollection::Confirmed(Box::new(plan)))
    } else {
        Ok(GuidedInitialSiteCollection::Declined)
    }
}

/// Combines the pre-resolved values with the answers, checking that advanced values still fit
/// the final inference choices. Explicit NVIDIA device selectors win over the GPUs picked.
fn choices_from_defaults(
    defaults: &GuidedInitialSiteDefaults,
    host: SelectedHost,
    node_name: Option<String>,
    inference: InferencePick,
    applications: BTreeSet<InitialApplicationRuntime>,
    lifetime: Option<NativeWorkloadLifetime>,
) -> anyhow::Result<InitialSiteChoices> {
    let offers = |capability| inference.inference.contains(&capability);
    anyhow::ensure!(
        defaults.nvidia_cdi_selectors.is_none() || offers(InitialInferenceRuntime::Nvidia),
        "--nvidia-device requires the final guided inference choices to include nvidia"
    );
    anyhow::ensure!(
        lifetime.is_none() || offers(InitialInferenceRuntime::Apple),
        "--native-workload-lifetime requires the final guided inference choices to include apple"
    );
    Ok(InitialSiteChoices {
        inference: inference.inference,
        applications,
        host,
        site_id: defaults.site_id.clone(),
        node_id: defaults.node_id.clone(),
        node_name,
        nvidia_cdi_selectors: defaults
            .nvidia_cdi_selectors
            .clone()
            .or(inference.nvidia_cdi_selectors),
        native_workload_lifetime: lifetime,
        simulated_hardware: defaults
            .simulated_hardware
            .clone()
            .or(inference.simulated_hardware),
    })
}

fn plan_from_prepared(
    site_dir: PathBuf,
    host: SelectedHost,
    prepared: PreparedInitialSite,
) -> GuidedInitialSitePlan {
    GuidedInitialSitePlan {
        site_dir,
        host,
        request: prepared.request,
        inputs: prepared.inputs,
    }
}

fn confirm(terminal: &mut dyn Terminal) -> anyhow::Result<bool> {
    Ok(terminal.confirm(&ConfirmPrompt::new("Write this site configuration?", true))?)
}
