//! Typed initial site workload choices and identity allocation policy.

use std::{collections::BTreeSet, fmt};

use dotcore_domain::valid_id;
use dotcore_domain::{SiteNodeHost, SiteNodeHostError};
use dotcore_proto::dotcore::site::v1::NativeWorkloadLifetime;

/// SiteNode display name used when neither the operator nor the machine suggests one.
const DEFAULT_SITE_NODE_DISPLAY_NAME: &str = "manager";

/// Inference capability a node offers through its model execution targets. A node may offer
/// several at once, each through its own targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InitialInferenceRuntime {
    /// Uses the deterministic in-memory runtime.
    Simulated,

    /// Uses the NVIDIA Docker runtime capability.
    Nvidia,

    /// Uses the native runtime on the Apple GPU of a macOS node.
    Apple,
}

/// Application capability a node offers through its application execution targets. A node may
/// offer several at once, each through its own target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InitialApplicationRuntime {
    /// Uses the deterministic in-memory runtime.
    Simulated,

    /// Uses the local Docker runtime capability.
    Docker,
}

/// Identity role allocated for an initial single-node site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialSiteIdentityKind {
    /// The site identity.
    Site,

    /// The sole SiteNode identity, when the operator overrides it.
    Node,
}

impl fmt::Display for InitialSiteIdentityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Site => "Site",
            Self::Node => "Node",
        })
    }
}

/// Source of injectable identity entropy and machine-derived naming for initial site
/// configuration.
pub trait InitialSiteIdentityGenerator {
    /// Returns one value from which the policy derives an identity for `kind`. Only the site
    /// identity is generated; node and target identities are derived from it.
    fn generate(&mut self, kind: InitialSiteIdentityKind) -> anyhow::Result<u64>;

    /// Returns the SiteNode display name this machine suggests, used when the operator supplied
    /// none. Without a suggestion the SiteNode is named after its ID.
    fn node_display_name(&self) -> Option<String> {
        None
    }
}

/// System-random identity generator for normal initial site configuration.
#[derive(Default)]
pub struct RandomInitialSiteIdentityGenerator;

impl InitialSiteIdentityGenerator for RandomInitialSiteIdentityGenerator {
    fn generate(&mut self, _kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
        let mut bytes = [0_u8; 5];
        getrandom::fill(&mut bytes).map_err(identity_entropy_error)?;
        Ok(bytes
            .into_iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(byte)))
    }
}

fn identity_entropy_error(source: getrandom::Error) -> anyhow::Error {
    anyhow::Error::new(source).context("initial site identity entropy failed")
}

/// Explicit initial site needs with optional operator-controlled site identity and node name
/// overrides.
///
/// The request names the inference and application capabilities the node offers, either set
/// possibly empty, and no deployment or node collections. Internal topology identities are
/// allocated only by [`Self::resolve_identities`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialSiteRequest {
    inference: BTreeSet<InitialInferenceRuntime>,
    applications: BTreeSet<InitialApplicationRuntime>,
    site_id_override: Option<String>,
    node_id_override: Option<String>,
    site_node_host_override: Option<SiteNodeHost>,
    site_node_host_default: SiteNodeHost,
    site_node_display_name_override: Option<String>,
    nvidia_cdi_selectors: Option<Vec<String>>,
    native_workload_lifetime: NativeWorkloadLifetime,
    simulated_hardware: Option<Vec<String>>,
}

impl InitialSiteRequest {
    /// Node host recorded when the request names neither an override nor another default.
    pub const DEFAULT_SITE_NODE_HOST: SiteNodeHost = SiteNodeHost::LOOPBACK;

    /// Simulated hardware a node with simulated inference declares when the request names none.
    pub const DEFAULT_SIMULATED_HARDWARE: &str = "rtx-5090";

    /// Starts a request with independent sets of inference and application capabilities; a
    /// capability named twice is offered once.
    pub fn new(
        inference: impl IntoIterator<Item = InitialInferenceRuntime>,
        applications: impl IntoIterator<Item = InitialApplicationRuntime>,
    ) -> Self {
        Self {
            inference: inference.into_iter().collect(),
            applications: applications.into_iter().collect(),
            site_id_override: None,
            node_id_override: None,
            site_node_host_override: None,
            site_node_host_default: Self::DEFAULT_SITE_NODE_HOST,
            site_node_display_name_override: None,
            nvidia_cdi_selectors: None,
            native_workload_lifetime: NativeWorkloadLifetime::Persistent,
            simulated_hardware: None,
        }
    }

    /// Selects an optional stable site identity for controlled automation.
    pub fn with_site_id_override(mut self, site_id: impl Into<String>) -> Self {
        self.site_id_override = Some(site_id.into());
        self
    }

    /// Selects an optional stable SiteNode identity for controlled automation.
    pub fn with_node_id_override(mut self, node_id: impl Into<String>) -> Self {
        self.node_id_override = Some(node_id.into());
        self
    }

    /// Selects a specific IPv4 address or DNS host name as the node host instead of the
    /// loopback default; a name is recorded lowercase without a trailing dot.
    pub fn with_site_node_host_override(
        mut self,
        host: impl AsRef<str>,
    ) -> Result<Self, InitialSiteRequestError> {
        let host = host.as_ref();
        self.site_node_host_override = Some(SiteNodeHost::normalize(host).map_err(|reason| {
            InitialSiteRequestError::InvalidHost {
                host: host.to_owned(),
                reason,
            }
        })?);
        Ok(self)
    }

    /// Selects the node host recorded when no override is given. A default is not operator
    /// intent: an existing site with another host still matches the request.
    pub fn with_site_node_host_default(mut self, host: SiteNodeHost) -> Self {
        self.site_node_host_default = host;
        self
    }

    /// Selects an explicit SiteNode display name, stored trimmed. Site validation rejects an
    /// invalid name when the site is composed.
    pub fn with_site_node_display_name_override(mut self, name: impl AsRef<str>) -> Self {
        self.site_node_display_name_override = Some(name.as_ref().trim().to_owned());
        self
    }

    /// Selects explicit NVIDIA CDI selectors instead of automatic full-GPU selection.
    pub fn with_nvidia_cdi_selectors(
        mut self,
        selectors: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.nvidia_cdi_selectors = Some(selectors.into_iter().map(Into::into).collect());
        self
    }

    /// Selects whether native workloads outlive the server process; only the Apple inference
    /// capability declares a native target, and the default is persistent.
    pub fn with_native_workload_lifetime(mut self, lifetime: NativeWorkloadLifetime) -> Self {
        self.native_workload_lifetime = lifetime;
        self
    }

    /// Selects the simulated hardware the node declares for simulated inference instead of the
    /// default: named GPUs, in order, or one named machine. Composing the site rejects an empty,
    /// repeated, unknown or invalid choice.
    pub fn with_simulated_hardware(mut self, choices: Vec<String>) -> Self {
        self.simulated_hardware = Some(choices);
        self
    }

    /// Returns the inference capabilities the node offers, in stable order.
    pub fn inference(&self) -> &BTreeSet<InitialInferenceRuntime> {
        &self.inference
    }

    /// Returns the application capabilities the node offers, in stable order.
    pub fn applications(&self) -> &BTreeSet<InitialApplicationRuntime> {
        &self.applications
    }

    /// Returns explicit NVIDIA CDI selectors, when automatic selection was overridden.
    pub fn nvidia_cdi_selectors(&self) -> Option<&[String]> {
        self.nvidia_cdi_selectors.as_deref()
    }

    /// Returns the lifetime of native workloads declared by the Apple inference capability.
    pub fn native_workload_lifetime(&self) -> NativeWorkloadLifetime {
        self.native_workload_lifetime
    }

    /// Returns the simulated hardware a node with simulated inference declares, in order: the
    /// selected choices, else the default.
    pub fn simulated_hardware(&self) -> Vec<String> {
        self.simulated_hardware
            .clone()
            .unwrap_or_else(|| vec![Self::DEFAULT_SIMULATED_HARDWARE.to_owned()])
    }

    /// Returns the explicit node host override, when the operator supplied one.
    pub fn site_node_host_override(&self) -> Option<&SiteNodeHost> {
        self.site_node_host_override.as_ref()
    }

    /// Returns the explicit SiteNode display name, when the operator supplied one.
    pub fn site_node_display_name_override(&self) -> Option<&str> {
        self.site_node_display_name_override.as_deref()
    }

    /// Returns the explicit host, else the default host.
    pub fn site_node_host(&self) -> SiteNodeHost {
        self.site_node_host_override
            .clone()
            .unwrap_or_else(|| self.site_node_host_default.clone())
    }

    /// Returns the explicit site identity override, when one was supplied.
    pub fn site_id_override(&self) -> Option<&str> {
        self.site_id_override.as_deref()
    }

    /// Returns the explicit SiteNode identity override, when one was supplied.
    pub fn node_id_override(&self) -> Option<&str> {
        self.node_id_override.as_deref()
    }

    /// Validates optional overrides, generates the site identity, and derives the SiteNode identity
    /// from it the same way node join does.
    pub fn resolve_identities(
        &self,
        generator: &mut dyn InitialSiteIdentityGenerator,
    ) -> Result<InitialSiteIdentities, InitialSiteRequestError> {
        let site_id = match self.site_id_override.as_deref() {
            Some(site_id) => site_id.to_owned(),
            None => format!(
                "site-{:010x}",
                generator
                    .generate(InitialSiteIdentityKind::Site)
                    .map_err(|source| InitialSiteRequestError::IdentityGeneration {
                        kind: InitialSiteIdentityKind::Site,
                        source,
                    })?
            ),
        };
        validate_identity(InitialSiteIdentityKind::Site, &site_id)?;
        let node_display_name = self
            .site_node_display_name_override
            .clone()
            .or_else(|| generator.node_display_name())
            .unwrap_or_else(|| DEFAULT_SITE_NODE_DISPLAY_NAME.to_owned());
        let node_id = match self.node_id_override.as_deref() {
            Some(node_id) => node_id.to_owned(),
            None => derive_site_node_id(&site_id, &node_display_name),
        };
        validate_identity(InitialSiteIdentityKind::Node, &node_id)?;
        Ok(InitialSiteIdentities {
            site_id,
            node_id,
            node_display_name,
        })
    }
}

/// Resolved identities for the exact single-node topology an initial request may create. Its
/// execution target identities derive from the node identity when the targets are built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialSiteIdentities {
    site_id: String,
    node_id: String,
    node_display_name: String,
}

impl InitialSiteIdentities {
    /// Returns the configured or generated site identity.
    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    /// Returns the configured or generated SiteNode identity.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Returns the SiteNode display name: the explicit name, else the generator's suggestion, else
    /// `manager`. Site validation checks it when the site is composed.
    pub fn node_display_name(&self) -> &str {
        &self.node_display_name
    }

    /// Iterates over every resolved identity in site topology order.
    pub fn values(&self) -> impl Iterator<Item = &str> {
        [self.site_id.as_str(), self.node_id.as_str()].into_iter()
    }
}

/// Failures while validating or allocating initial site identities.
#[derive(Debug, thiserror::Error)]
pub enum InitialSiteRequestError {
    /// The injected source could not provide identity entropy.
    #[error("initial {kind} ID generation failed")]
    IdentityGeneration {
        /// Identity role being allocated.
        kind: InitialSiteIdentityKind,
        /// Underlying identity-source failure.
        #[source]
        source: anyhow::Error,
    },

    /// An operator override or generated value violated site identity rules.
    #[error("initial {kind} ID {value:?} must match [a-z][a-z0-9-]{{0,62}}")]
    InvalidIdentity {
        /// Identity role being validated.
        kind: InitialSiteIdentityKind,
        /// Rejected identity value.
        value: String,
    },

    /// The requested node host violated the shared site host rule.
    #[error("initial node host {host:?} {reason}")]
    InvalidHost {
        /// Rejected host value.
        host: String,
        /// Shared site host validation diagnostic.
        reason: SiteNodeHostError,
    },
}

/// Derives the identity of the SiteNode named `display_name` in site `site_id`, for example
/// `node-6c62272e07bb0142`. The same site and name always give the same identity, so a node whose
/// join failed before it was recorded gets the same identity when it tries again.
pub fn derive_site_node_id(site_id: &str, display_name: &str) -> String {
    format!(
        "node-{:010x}",
        stable_identity_value(&format!("{site_id}\n{display_name}"))
    )
}

/// Derives the identity of a SiteNode's model execution target from its node, its inference
/// capability and the device it holds, such as `execution-target-6c62272e07bb0142-nvidia-1f0d3a9c`
/// for one NVIDIA GPU or `execution-target-6c62272e07bb0142-simulated`. A target keeps its
/// identity while its node, capability and device do, and targets of different capabilities on
/// one node never share one.
pub fn derive_model_execution_target_id(
    node_id: &str,
    inference: InitialInferenceRuntime,
    device: Option<&str>,
) -> String {
    let kind = match inference {
        InitialInferenceRuntime::Simulated => "simulated",
        InitialInferenceRuntime::Nvidia => "nvidia",
        InitialInferenceRuntime::Apple => "apple",
    };
    let node = stable_identity_value(node_id);
    match device {
        Some(device) => format!(
            "execution-target-{node:010x}-{kind}-{:08x}",
            stable_identity_value(device) & u64::from(u32::MAX)
        ),
        None => format!("execution-target-{node:010x}-{kind}"),
    }
}

/// Derives the identity of a SiteNode's application execution target from its node and its
/// application capability, such as `application-execution-target-6c62272e07bb0142-docker`.
pub fn derive_application_execution_target_id(
    node_id: &str,
    applications: InitialApplicationRuntime,
) -> String {
    let kind = match applications {
        InitialApplicationRuntime::Simulated => "simulated",
        InitialApplicationRuntime::Docker => "docker",
    };
    format!(
        "application-execution-target-{:010x}-{kind}",
        stable_identity_value(node_id)
    )
}

fn validate_identity(
    kind: InitialSiteIdentityKind,
    value: &str,
) -> Result<(), InitialSiteRequestError> {
    if valid_id(value) {
        Ok(())
    } else {
        Err(InitialSiteRequestError::InvalidIdentity {
            kind,
            value: value.to_owned(),
        })
    }
}

/// FNV-1a: a stable 64-bit value of `value` across processes and builds.
fn stable_identity_value(value: &str) -> u64 {
    value
        .bytes()
        .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, error::Error};

    use anyhow::anyhow;
    use dotcore_domain::valid_id;
    use dotcore_domain::{SiteNodeHost, SiteNodeHostError};

    use super::{
        InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteIdentityGenerator,
        InitialSiteIdentityKind, InitialSiteRequest, InitialSiteRequestError,
        derive_application_execution_target_id, derive_model_execution_target_id,
        derive_site_node_id, identity_entropy_error, stable_identity_value,
    };

    #[test]
    fn independent_workload_choices_resolve_generated_bounded_identities() {
        let request = InitialSiteRequest::new(
            [
                InitialInferenceRuntime::Apple,
                InitialInferenceRuntime::Nvidia,
                InitialInferenceRuntime::Apple,
            ],
            [InitialApplicationRuntime::Docker],
        );
        let mut generator = FixedIdentityGenerator::new([0x01]);

        assert_eq!(
            request.inference().iter().copied().collect::<Vec<_>>(),
            [
                InitialInferenceRuntime::Nvidia,
                InitialInferenceRuntime::Apple
            ]
        );
        assert_eq!(
            request.applications().iter().copied().collect::<Vec<_>>(),
            [InitialApplicationRuntime::Docker]
        );
        let identities = request
            .resolve_identities(&mut generator)
            .expect("identities");

        assert_eq!(identities.site_id(), "site-0000000001");
        assert_eq!(identities.node_display_name(), "manager");
        assert_eq!(
            identities.node_id(),
            derive_site_node_id("site-0000000001", "manager")
        );
        assert_eq!(generator.kinds, [InitialSiteIdentityKind::Site]);
    }

    #[test]
    fn generated_identities_always_use_the_existing_site_identifier_rule() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        );
        for value in [0, u64::MAX] {
            let identities = request
                .resolve_identities(&mut FixedIdentityGenerator::new([value]))
                .expect("identities");

            for identity in identities.values() {
                assert!(valid_id(identity), "{identity:?} must be a valid site ID");
                assert!(identity.len() <= 63, "{identity:?} must be bounded");
            }
        }
    }

    #[test]
    fn default_identity_generation_requires_no_operator_input() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        );

        let identities = request
            .resolve_identities(&mut super::RandomInitialSiteIdentityGenerator)
            .expect("system entropy");

        assert!(identities.values().all(valid_id));
    }

    #[test]
    fn identity_entropy_failure_retains_its_concrete_source() {
        let error = identity_entropy_error(getrandom::Error::UNSUPPORTED);

        assert_eq!(error.to_string(), "initial site identity entropy failed");
        let source = error.source().expect("entropy source");
        assert_eq!(
            source.to_string(),
            "getrandom: this target is not supported"
        );
        assert_eq!(
            source.downcast_ref::<getrandom::Error>(),
            Some(&getrandom::Error::UNSUPPORTED)
        );
    }

    #[test]
    fn site_and_node_overrides_use_the_same_identifier_validation() {
        let request = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Docker],
        )
        .with_site_id_override("automation-site")
        .with_node_id_override("automation-node");
        let mut generator = FixedIdentityGenerator::new([]);

        let identities = request
            .resolve_identities(&mut generator)
            .expect("identities");

        assert_eq!(identities.site_id(), "automation-site");
        assert_eq!(identities.node_id(), "automation-node");
        assert!(generator.kinds.is_empty());
    }

    #[test]
    fn derived_identities_are_stable_per_site_and_name() {
        let node_id = derive_site_node_id("site-one", "gpu-box");

        assert_eq!(
            node_id,
            format!("node-{:010x}", stable_identity_value("site-one\ngpu-box"))
        );
        assert_eq!(derive_site_node_id("site-one", "gpu-box"), node_id);
        assert_ne!(derive_site_node_id("site-two", "gpu-box"), node_id);
        assert_ne!(derive_site_node_id("site-one", "other"), node_id);
        assert_eq!(stable_identity_value(""), 14_695_981_039_346_656_037);
    }

    #[test]
    fn target_identities_derive_from_the_node_the_capability_and_the_device() {
        let node = stable_identity_value("node-gpu");
        let device = |value: &str| stable_identity_value(value) & u64::from(u32::MAX);
        let model = |inference, device: Option<&str>| {
            derive_model_execution_target_id("node-gpu", inference, device)
        };

        assert_eq!(
            model(InitialInferenceRuntime::Simulated, None),
            format!("execution-target-{node:010x}-simulated")
        );
        assert_eq!(
            model(InitialInferenceRuntime::Apple, Some("apple-gpu-0")),
            format!(
                "execution-target-{node:010x}-apple-{:08x}",
                device("apple-gpu-0")
            )
        );
        assert_eq!(
            derive_application_execution_target_id("node-gpu", InitialApplicationRuntime::Docker),
            format!("application-execution-target-{node:010x}-docker")
        );
        assert_eq!(
            derive_application_execution_target_id(
                "node-gpu",
                InitialApplicationRuntime::Simulated
            ),
            format!("application-execution-target-{node:010x}-simulated")
        );
        let gpu0 = model(InitialInferenceRuntime::Nvidia, Some("nvidia.com/gpu=gpu0"));
        assert_eq!(
            gpu0,
            format!(
                "execution-target-{node:010x}-nvidia-{:08x}",
                device("nvidia.com/gpu=gpu0")
            )
        );
        assert_eq!(
            model(InitialInferenceRuntime::Nvidia, Some("nvidia.com/gpu=gpu0")),
            gpu0
        );
        for other in [
            model(InitialInferenceRuntime::Nvidia, Some("nvidia.com/gpu=gpu1")),
            model(InitialInferenceRuntime::Nvidia, Some("nvidia.com/gpu=0")),
            derive_model_execution_target_id(
                "node-other",
                InitialInferenceRuntime::Nvidia,
                Some("nvidia.com/gpu=gpu0"),
            ),
        ] {
            assert_ne!(other, gpu0);
        }
        for longest in [
            derive_model_execution_target_id(
                &"n".repeat(63),
                InitialInferenceRuntime::Nvidia,
                Some("nvidia.com/gpu=MIG-f9a4a3aa-f02d-97ca-9972-3f61413b228b"),
            ),
            derive_model_execution_target_id(
                &"n".repeat(63),
                InitialInferenceRuntime::Simulated,
                Some(&"x".repeat(400)),
            ),
            derive_application_execution_target_id(
                &"n".repeat(63),
                InitialApplicationRuntime::Simulated,
            ),
        ] {
            assert!(valid_id(&longest), "{longest:?} must be a valid site ID");
        }
    }

    #[test]
    fn invalid_site_and_node_overrides_are_rejected_by_the_existing_rule() {
        for request in [
            InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Simulated],
            )
            .with_site_id_override("Invalid-site"),
            InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Simulated],
            )
            .with_node_id_override("invalid_node"),
        ] {
            let error = request
                .resolve_identities(&mut FixedIdentityGenerator::new([1]))
                .expect_err("invalid override");

            assert!(
                error
                    .to_string()
                    .contains("must match [a-z][a-z0-9-]{0,62}")
            );
        }
    }

    #[test]
    fn site_node_host_intent_defaults_and_uses_the_shared_host_rule() {
        let default = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        );
        assert_eq!(default.site_node_host_override(), None);
        assert_eq!(default.site_node_host().to_string(), "127.0.0.1");

        let explicit = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
        .with_site_node_host_override("192.0.2.10")
        .expect("specific IPv4 override");
        assert_eq!(
            explicit
                .site_node_host_override()
                .map(|host| host.to_string()),
            Some("192.0.2.10".to_owned())
        );

        let named = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
        .with_site_node_host_override("Manager.Example.TS.net.")
        .expect("host name override");
        assert_eq!(named.site_node_host().to_string(), "manager.example.ts.net");

        let tailnet = SiteNodeHost::parse("100.64.0.7").expect("default host");
        let defaulted = InitialSiteRequest::new(
            [InitialInferenceRuntime::Simulated],
            [InitialApplicationRuntime::Simulated],
        )
        .with_site_node_host_default(tailnet.clone());
        assert_eq!(defaulted.site_node_host_override(), None);
        assert_eq!(defaulted.site_node_host(), tailnet);
        assert_eq!(
            defaulted
                .with_site_node_host_override("192.0.2.10")
                .expect("override over a default")
                .site_node_host()
                .to_string(),
            "192.0.2.10"
        );

        for (host, expected_reason, expected_message) in [
            (
                "0.0.0.0",
                SiteNodeHostError::Unspecified,
                "initial node host \"0.0.0.0\" must be a specific non-wildcard IPv4 address \
                 or a DNS host name",
            ),
            (
                "not_a_host",
                SiteNodeHostError::Invalid,
                "initial node host \"not_a_host\" must be an IPv4 address or a DNS host name",
            ),
            (
                "::1",
                SiteNodeHostError::Ipv6,
                "initial node host \"::1\" must be an IPv4 address or a DNS host name, not \
                 an IPv6 address",
            ),
        ] {
            let error = InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Simulated],
            )
            .with_site_node_host_override(host)
            .expect_err("invalid node host");

            assert_eq!(error.to_string(), expected_message);
            assert!(error.source().is_none());
            assert!(matches!(
                error,
                InitialSiteRequestError::InvalidHost {
                    host: actual_host,
                    reason,
                } if actual_host == host && reason == expected_reason
            ));
        }
    }

    struct FixedIdentityGenerator {
        values: VecDeque<u64>,
        kinds: Vec<InitialSiteIdentityKind>,
    }

    impl FixedIdentityGenerator {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self {
                values: values.into_iter().collect(),
                kinds: Vec::new(),
            }
        }
    }

    impl InitialSiteIdentityGenerator for FixedIdentityGenerator {
        fn generate(&mut self, kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
            self.kinds.push(kind);
            self.values
                .pop_front()
                .ok_or_else(|| anyhow!("no test value"))
        }
    }
}
