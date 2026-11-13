//! Owner-neutral description of one individual workload that any runtime can realize:
//! an executable inside an artifact, its arguments and environment, the artifacts it
//! reads, the services it exposes, and its device and IPC needs.
//!
//! Arguments and environment values may contain symbolic tokens that each runtime
//! resolves at materialization, for example `--port={service:http.port}` becomes
//! `--port=8080` inside a container and an assigned loopback port for a host process.
//! Workload identity hashes the symbolic form, so runtime-assigned values never change it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use dotcore_domain::{ExecutionPlatform, WorkloadArtifactKind};
use sha2::Sha256;

use super::{
    ExecutionTarget, RuntimeBackendError, WorkloadIpcMode, WorkloadStorageAttachment,
    individual_workload::update_fingerprint,
};

/// Complete owner-issued launch intent for one individual workload, before runtime resolution.
///
/// Argument and environment values may reference the declared executable, artifact bindings,
/// and ports through tokens. Declared artifact destinations and container ports are fixed
/// values for runtimes that isolate the workload's filesystem and network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadSpec {
    pub executable: WorkloadExecutable,
    pub required_platform: ExecutionPlatform,
    /// Ordered arguments passed after the executable.
    pub arguments: Vec<WorkloadValue>,
    pub environment: BTreeMap<String, WorkloadValue>,
    pub artifacts: Vec<WorkloadArtifactBinding>,
    /// Runtime-provisioned storage; owners never supply its host paths.
    pub storage: Vec<WorkloadStorageAttachment>,
    pub ports: Vec<WorkloadPort>,
    pub accelerator_device_ids: Vec<String>,
    pub required_accelerator_device_count: u32,
    pub ipc_mode: WorkloadIpcMode,
    /// Memory the owner expects the workload to use; it never changes the workload's identity.
    pub expected_memory: WorkloadExpectedMemory,
}

impl WorkloadSpec {
    /// Rejects duplicate declarations, an empty executable path, storage mounted where an
    /// artifact is, and tokens that reference undeclared artifact bindings or ports.
    pub fn validate(&self) -> Result<(), WorkloadSpecError> {
        if self.executable.path.is_empty() {
            return Err(WorkloadSpecError::EmptyExecutablePath);
        }

        let mut bindings = BTreeSet::new();
        for binding in &self.artifacts {
            if !bindings.insert(binding.name.as_str()) {
                return Err(WorkloadSpecError::DuplicateArtifactBinding {
                    binding: binding.name.clone(),
                });
            }
        }

        for storage in &self.storage {
            if self
                .artifacts
                .iter()
                .any(|binding| binding.destination == storage.destination)
            {
                return Err(WorkloadSpecError::StorageDestinationIsArtifact {
                    storage: storage.name.clone(),
                    destination: storage.destination.clone(),
                });
            }
        }

        let mut ports = BTreeSet::new();
        for port in &self.ports {
            if !ports.insert(port.name.as_str()) {
                return Err(WorkloadSpecError::DuplicatePort {
                    port: port.name.clone(),
                });
            }
        }

        for token in self.values().flat_map(WorkloadValue::tokens) {
            match token {
                WorkloadToken::Executable => {}
                WorkloadToken::Artifact(binding) if !bindings.contains(binding.as_str()) => {
                    return Err(WorkloadSpecError::UndeclaredArtifactBinding {
                        binding: binding.clone(),
                    });
                }
                WorkloadToken::ServiceHost(service) | WorkloadToken::ServicePort(service)
                    if !ports.contains(service.as_str()) =>
                {
                    return Err(WorkloadSpecError::UndeclaredServicePort {
                        service: service.clone(),
                    });
                }
                WorkloadToken::Artifact(_)
                | WorkloadToken::ServiceHost(_)
                | WorkloadToken::ServicePort(_) => {}
            }
        }
        Ok(())
    }

    /// Rejects a runtime whose observed platform differs from the one the executable requires.
    pub fn validate_runtime_platform(
        &self,
        observed: &ExecutionPlatform,
    ) -> Result<(), RuntimeBackendError> {
        if observed != &self.required_platform {
            return Err(RuntimeBackendError::TargetPlatformMismatch {
                required_operating_system: self.required_platform.operating_system().to_owned(),
                required_architecture: self.required_platform.architecture().to_owned(),
                observed_operating_system: observed.operating_system().to_owned(),
                observed_architecture: observed.architecture().to_owned(),
            });
        }
        Ok(())
    }

    /// Validates the spec and replaces every token with the value the runtime supplies.
    pub fn resolve(
        &self,
        resolver: &dyn WorkloadTokenResolver,
    ) -> Result<ResolvedWorkloadCommand, WorkloadSpecError> {
        self.validate()?;
        let resolve = |value: &WorkloadValue| self.resolve_value(value, resolver);
        Ok(ResolvedWorkloadCommand {
            executable: resolver.executable(&self.executable),
            arguments: self
                .arguments
                .iter()
                .map(resolve)
                .collect::<Result<_, _>>()?,
            environment: self
                .environment
                .iter()
                .map(|(name, value)| Ok((name.clone(), resolve(value)?)))
                .collect::<Result<_, WorkloadSpecError>>()?,
        })
    }

    /// Computes the durable reconcile identity from the symbolic spec and its execution target.
    ///
    /// Host artifact paths and runtime-resolved token values are excluded, so the identity is
    /// stable across artifact relocation and port reassignment. A spec without tokens hashes
    /// exactly like the equivalent literal command line.
    pub fn reconcile_generation(&self, target: &ExecutionTarget) -> u64 {
        let symbolic = self.is_symbolic();
        let mut values = vec![
            self.executable.artifact.reference().to_owned(),
            self.required_platform.operating_system().to_owned(),
            self.required_platform.architecture().to_owned(),
            target.managed_node_id.to_string(),
            self.executable.path.clone(),
        ];
        values.extend(
            self.arguments
                .iter()
                .map(|argument| argument.identity(symbolic)),
        );
        values.extend(
            self.environment
                .iter()
                .map(|(key, value)| format!("{key}={}", value.identity(symbolic))),
        );
        values.extend(self.artifacts.iter().map(|binding| {
            format!(
                "{}:{}:{}",
                binding.name, binding.destination, binding.read_only
            )
        }));
        values.extend(self.storage.iter().map(|storage| {
            format!(
                "storage:{}:{}:{}:{}:{}:{}",
                storage.kind.as_str(),
                storage.namespace,
                storage.key,
                storage.name,
                storage.destination,
                storage.read_only
            )
        }));
        values.extend(
            self.ports
                .iter()
                .map(|port| format!("{}:{}", port.name, port.container_port)),
        );
        values.extend(self.accelerator_device_ids.clone());
        values.push(self.ipc_mode.as_str().to_owned());
        values.push(self.required_accelerator_device_count.to_string());
        values.extend(target.accelerator_device_ids.clone());
        // Tagged only when present so literal specs keep their identity.
        if symbolic {
            values.push(SYMBOLIC_TAG.to_owned());
        }

        let mut hash = 0xcbf29ce484222325_u64;
        for bytes in values.into_iter().map(String::into_bytes) {
            for byte in (bytes.len() as u64).to_le_bytes().into_iter().chain(bytes) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        hash.max(1)
    }

    /// Adds the spec's exact launch intent, including host artifact paths, to a fingerprint.
    ///
    /// Runtime-resolved token values are excluded. A spec without tokens contributes exactly
    /// the bytes of the equivalent literal command line.
    pub fn update_exact_intent(&self, digest: &mut Sha256) {
        let symbolic = self.is_symbolic();
        update_fingerprint(digest, self.executable.artifact.reference().as_bytes());
        update_fingerprint(digest, self.executable.path.as_bytes());
        for argument in &self.arguments {
            update_fingerprint(digest, argument.identity(symbolic).as_bytes());
        }
        for (key, value) in &self.environment {
            update_fingerprint(digest, key.as_bytes());
            update_fingerprint(digest, value.identity(symbolic).as_bytes());
        }
        for binding in &self.artifacts {
            update_fingerprint(digest, binding.name.as_bytes());
            update_fingerprint(digest, binding.source.as_os_str().as_encoded_bytes());
            update_fingerprint(digest, binding.destination.as_bytes());
            update_fingerprint(digest, &[u8::from(binding.read_only)]);
        }
        // Tagged only when present so workloads without storage keep their fingerprint.
        if !self.storage.is_empty() {
            update_fingerprint(digest, b"storage");
        }
        for storage in &self.storage {
            update_fingerprint(digest, storage.kind.as_str().as_bytes());
            update_fingerprint(digest, storage.namespace.as_bytes());
            update_fingerprint(digest, storage.key.as_bytes());
            update_fingerprint(digest, storage.name.as_bytes());
            update_fingerprint(digest, storage.destination.as_bytes());
            update_fingerprint(digest, &[u8::from(storage.read_only)]);
        }
        for port in &self.ports {
            update_fingerprint(digest, port.name.as_bytes());
            update_fingerprint(digest, &port.container_port.to_le_bytes());
        }
        for device in &self.accelerator_device_ids {
            update_fingerprint(digest, device.as_bytes());
        }
        update_fingerprint(
            digest,
            &self.required_accelerator_device_count.to_le_bytes(),
        );
        update_fingerprint(digest, self.ipc_mode.as_str().as_bytes());
        // Tagged only when present so literal specs keep their fingerprint.
        if symbolic {
            update_fingerprint(digest, SYMBOLIC_TAG.as_bytes());
        }
    }

    fn values(&self) -> impl Iterator<Item = &WorkloadValue> {
        self.arguments.iter().chain(self.environment.values())
    }

    fn is_symbolic(&self) -> bool {
        self.values().any(|value| value.tokens().next().is_some())
    }

    fn resolve_value(
        &self,
        value: &WorkloadValue,
        resolver: &dyn WorkloadTokenResolver,
    ) -> Result<String, WorkloadSpecError> {
        let mut resolved = String::new();
        for segment in &value.segments {
            match segment {
                WorkloadValueSegment::Literal(literal) => resolved.push_str(literal),
                WorkloadValueSegment::Token(WorkloadToken::Executable) => {
                    resolved.push_str(&resolver.executable(&self.executable));
                }
                WorkloadValueSegment::Token(WorkloadToken::Artifact(name)) => {
                    resolved.push_str(&resolver.artifact(self.artifact(name)?));
                }
                WorkloadValueSegment::Token(WorkloadToken::ServiceHost(name)) => {
                    resolved.push_str(&resolver.service_host(self.port(name)?));
                }
                WorkloadValueSegment::Token(WorkloadToken::ServicePort(name)) => {
                    resolved.push_str(&resolver.service_port(self.port(name)?).to_string());
                }
            }
        }
        Ok(resolved)
    }

    fn artifact(&self, name: &str) -> Result<&WorkloadArtifactBinding, WorkloadSpecError> {
        self.artifacts
            .iter()
            .find(|binding| binding.name == name)
            .ok_or_else(|| WorkloadSpecError::UndeclaredArtifactBinding {
                binding: name.to_owned(),
            })
    }

    fn port(&self, name: &str) -> Result<&WorkloadPort, WorkloadSpecError> {
        self.ports
            .iter()
            .find(|port| port.name == name)
            .ok_or_else(|| WorkloadSpecError::UndeclaredServicePort {
                service: name.to_owned(),
            })
    }
}

const SYMBOLIC_TAG: &str = "symbolic";

/// Supplies runtime-specific values for workload tokens during materialization.
///
/// A container runtime answers with in-container paths, a wildcard host, and fixed container
/// ports; a host-process runtime answers with host paths, loopback, and assigned ports. The spec
/// only asks about items it declares.
pub trait WorkloadTokenResolver {
    /// Returns the path that launches the declared executable.
    fn executable(&self, executable: &WorkloadExecutable) -> String;

    /// Returns the path at which the workload reads one declared artifact binding.
    fn artifact(&self, binding: &WorkloadArtifactBinding) -> String;

    /// Returns the address one declared service binds to.
    fn service_host(&self, port: &WorkloadPort) -> String;

    /// Returns the port one declared service listens on.
    fn service_port(&self, port: &WorkloadPort) -> u16;
}

/// Concrete command line produced by resolving a spec for one runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedWorkloadCommand {
    pub executable: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

/// The program a workload runs: one path inside one executable artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadExecutable {
    pub artifact: WorkloadExecutableArtifact,
    /// Executable path inside the artifact, for example `/usr/bin/server` in an OCI image.
    pub path: String,
}

/// Artifact that provides a workload's executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadExecutableArtifact {
    /// Canonical OCI image reference.
    OciImage(String),

    /// Absolute top-level Nix store path, for example `/nix/store/<hash>-server`.
    NixStorePath(String),
}

impl WorkloadExecutableArtifact {
    /// Returns the canonical reference that identifies the artifact.
    pub fn reference(&self) -> &str {
        match self {
            Self::OciImage(image) => image,
            Self::NixStorePath(path) => path,
        }
    }

    pub fn kind(&self) -> WorkloadArtifactKind {
        match self {
            Self::OciImage(_) => WorkloadArtifactKind::Oci,
            Self::NixStorePath(_) => WorkloadArtifactKind::Nix,
        }
    }
}

/// One named, prepared artifact the workload reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadArtifactBinding {
    pub name: String,
    /// Prepared host path of the artifact.
    pub source: PathBuf,
    /// Workload-visible path for runtimes that isolate the workload's filesystem.
    pub destination: String,
    pub read_only: bool,
}

/// Memory the owner expects one workload to use, from its resolved estimate: bytes on each
/// accelerator device it holds, and bytes of host memory outside any accelerator. Runtimes that
/// run real processes ignore it; the simulated runtime reports it as the workload's use. Empty
/// when the owner has no estimate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkloadExpectedMemory {
    /// Bytes on each accelerator device, by the device ID the workload holds it under.
    pub device_bytes: BTreeMap<String, u64>,

    /// Bytes of host memory outside any accelerator.
    pub host_bytes: u64,
}

/// One named TCP service port declared by the workload owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadPort {
    pub name: String,
    /// Listening port for runtimes that isolate the workload's network.
    pub container_port: u16,
}

/// One argument or environment value made of literal text and symbolic tokens.
///
/// Its canonical text form escapes literal braces by doubling them, for example
/// `--model={artifact:model}` or `{{"json":true}}` for the literal `{"json":true}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkloadValue {
    segments: Vec<WorkloadValueSegment>,
}

impl WorkloadValue {
    /// Builds a value that is used verbatim, even when it contains braces.
    pub fn literal(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            segments: if value.is_empty() {
                Vec::new()
            } else {
                vec![WorkloadValueSegment::Literal(value)]
            },
        }
    }

    /// Builds a value made of exactly one token.
    pub fn token(token: WorkloadToken) -> Self {
        Self {
            segments: vec![WorkloadValueSegment::Token(token)],
        }
    }

    /// Parses the canonical text form, rejecting unknown tokens and unbalanced braces.
    pub fn parse(template: &str) -> Result<Self, WorkloadSpecError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut characters = template.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '{' if characters.next_if_eq(&'{').is_some() => literal.push('{'),
                '}' if characters.next_if_eq(&'}').is_some() => literal.push('}'),
                '{' => {
                    let mut token = String::new();
                    loop {
                        match characters.next() {
                            Some('}') => break,
                            Some('{') | None => {
                                return Err(WorkloadSpecError::UnterminatedToken {
                                    template: template.to_owned(),
                                });
                            }
                            Some(character) => token.push(character),
                        }
                    }
                    if !literal.is_empty() {
                        segments.push(WorkloadValueSegment::Literal(std::mem::take(&mut literal)));
                    }
                    segments.push(WorkloadValueSegment::Token(token.parse()?));
                }
                '}' => {
                    return Err(WorkloadSpecError::UnmatchedClosingBrace {
                        template: template.to_owned(),
                    });
                }
                character => literal.push(character),
            }
        }
        if !literal.is_empty() {
            segments.push(WorkloadValueSegment::Literal(literal));
        }
        Ok(Self { segments })
    }

    /// Returns the literal text when the value contains no token.
    pub fn as_literal(&self) -> Option<String> {
        self.segments
            .iter()
            .map(|segment| match segment {
                WorkloadValueSegment::Literal(literal) => Some(literal.as_str()),
                WorkloadValueSegment::Token(_) => None,
            })
            .collect()
    }

    pub fn segments(&self) -> &[WorkloadValueSegment] {
        &self.segments
    }

    fn tokens(&self) -> impl Iterator<Item = &WorkloadToken> {
        self.segments.iter().filter_map(|segment| match segment {
            WorkloadValueSegment::Token(token) => Some(token),
            WorkloadValueSegment::Literal(_) => None,
        })
    }

    /// Raw literal text for literal specs, canonical escaped text once any value is symbolic.
    fn identity(&self, symbolic: bool) -> String {
        match self.as_literal() {
            Some(literal) if !symbolic => literal,
            _ => self.to_string(),
        }
    }
}

impl fmt::Display for WorkloadValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for segment in &self.segments {
            match segment {
                WorkloadValueSegment::Literal(literal) => {
                    formatter.write_str(&literal.replace('{', "{{").replace('}', "}}"))?;
                }
                WorkloadValueSegment::Token(token) => write!(formatter, "{token}")?,
            }
        }
        Ok(())
    }
}

/// One piece of a workload value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadValueSegment {
    Literal(String),

    Token(WorkloadToken),
}

/// Symbolic reference to a value that only the realizing runtime knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadToken {
    /// `{engine.executable}`: the launch path of the declared executable.
    Executable,

    /// `{artifact:<binding>}`: the path of one declared artifact binding.
    Artifact(String),

    /// `{service:<name>.host}`: the address one declared service binds to.
    ServiceHost(String),

    /// `{service:<name>.port}`: the port one declared service listens on.
    ServicePort(String),
}

impl std::str::FromStr for WorkloadToken {
    type Err = WorkloadSpecError;

    /// Parses a token body without its surrounding braces.
    fn from_str(body: &str) -> Result<Self, Self::Err> {
        let unknown = || WorkloadSpecError::UnknownToken {
            token: format!("{{{body}}}"),
        };
        if body == "engine.executable" {
            return Ok(Self::Executable);
        }
        if let Some(binding) = body.strip_prefix("artifact:") {
            return (!binding.is_empty())
                .then(|| Self::Artifact(binding.to_owned()))
                .ok_or_else(unknown);
        }
        let (service, field) = body
            .strip_prefix("service:")
            .and_then(|service| service.rsplit_once('.'))
            .filter(|(service, _)| !service.is_empty())
            .ok_or_else(unknown)?;
        match field {
            "host" => Ok(Self::ServiceHost(service.to_owned())),
            "port" => Ok(Self::ServicePort(service.to_owned())),
            _ => Err(unknown()),
        }
    }
}

impl fmt::Display for WorkloadToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Executable => formatter.write_str("{engine.executable}"),
            Self::Artifact(binding) => write!(formatter, "{{artifact:{binding}}}"),
            Self::ServiceHost(service) => write!(formatter, "{{service:{service}.host}}"),
            Self::ServicePort(service) => write!(formatter, "{{service:{service}.port}}"),
        }
    }
}

/// Invalid workload spec or token syntax.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkloadSpecError {
    #[error(
        "workload value uses unknown token {token}; supported tokens are {{engine.executable}}, \
         {{artifact:<binding>}}, {{service:<name>.host}}, and {{service:<name>.port}}"
    )]
    UnknownToken { token: String },

    #[error(
        "workload value {template:?} opens a token without closing it; write {{{{ for a literal brace"
    )]
    UnterminatedToken { template: String },

    #[error(
        "workload value {template:?} has an unmatched closing brace; write }}}} for a literal brace"
    )]
    UnmatchedClosingBrace { template: String },

    #[error("workload spec requires a nonempty executable path")]
    EmptyExecutablePath,

    #[error("workload spec declares artifact binding {binding:?} more than once")]
    DuplicateArtifactBinding { binding: String },

    #[error("workload spec declares port {port:?} more than once")]
    DuplicatePort { port: String },

    #[error(
        "workload storage {storage:?} is mounted at {destination:?}, where an artifact is mounted"
    )]
    StorageDestinationIsArtifact {
        storage: String,
        destination: String,
    },

    #[error(
        "workload token references artifact binding {binding:?}, which the workload does not declare"
    )]
    UndeclaredArtifactBinding { binding: String },

    #[error(
        "workload token references service {service:?}, which the workload does not declare as a port"
    )]
    UndeclaredServicePort { service: String },
}

#[cfg(test)]
mod tests {
    use dotcore_domain::SiteNodeId;
    use sha2::Digest;

    use super::*;

    #[test]
    fn values_round_trip_tokens_and_escaped_braces() {
        let value = WorkloadValue::parse(
            "{engine.executable} --model={artifact:model} {{\"json\":true}} \
             http://{service:http.host}:{service:http.port}",
        )
        .expect("valid template");

        assert_eq!(
            value.segments(),
            [
                WorkloadValueSegment::Token(WorkloadToken::Executable),
                WorkloadValueSegment::Literal(" --model=".to_owned()),
                WorkloadValueSegment::Token(WorkloadToken::Artifact("model".to_owned())),
                WorkloadValueSegment::Literal(" {\"json\":true} http://".to_owned()),
                WorkloadValueSegment::Token(WorkloadToken::ServiceHost("http".to_owned())),
                WorkloadValueSegment::Literal(":".to_owned()),
                WorkloadValueSegment::Token(WorkloadToken::ServicePort("http".to_owned())),
            ]
        );
        assert_eq!(WorkloadValue::parse(&value.to_string()), Ok(value));
        assert_eq!(
            WorkloadValue::parse("{service:metrics.v2.port}"),
            Ok(WorkloadValue::token(WorkloadToken::ServicePort(
                "metrics.v2".to_owned()
            )))
        );
        assert_eq!(WorkloadValue::parse(""), Ok(WorkloadValue::literal("")));
    }

    #[test]
    fn literal_values_keep_braces_verbatim() {
        let value = WorkloadValue::literal("{service:http.port}");

        assert_eq!(value.as_literal().as_deref(), Some("{service:http.port}"));
        assert_eq!(value.to_string(), "{{service:http.port}}");
        assert_eq!(WorkloadValue::parse(&value.to_string()), Ok(value));
    }

    #[test]
    fn parse_rejects_unknown_tokens_and_unbalanced_braces() {
        for (template, expected) in [
            ("{engine.path}", unknown("{engine.path}")),
            ("{artifact:}", unknown("{artifact:}")),
            ("{service:http}", unknown("{service:http}")),
            ("{service:.port}", unknown("{service:.port}")),
            ("{service:http.address}", unknown("{service:http.address}")),
            ("{}", unknown("{}")),
            (
                "--port={service:http.port",
                WorkloadSpecError::UnterminatedToken {
                    template: "--port={service:http.port".to_owned(),
                },
            ),
            (
                "{artifact:{model}}",
                WorkloadSpecError::UnterminatedToken {
                    template: "{artifact:{model}}".to_owned(),
                },
            ),
            (
                "value}",
                WorkloadSpecError::UnmatchedClosingBrace {
                    template: "value}".to_owned(),
                },
            ),
        ] {
            assert_eq!(WorkloadValue::parse(template), Err(expected), "{template}");
        }
        assert_eq!(
            unknown("{engine.path}").to_string(),
            "workload value uses unknown token {engine.path}; supported tokens are \
             {engine.executable}, {artifact:<binding>}, {service:<name>.host}, and \
             {service:<name>.port}"
        );
    }

    #[test]
    fn validation_rejects_undeclared_and_duplicate_declarations() {
        let mut undeclared_binding = spec();
        undeclared_binding
            .arguments
            .push(template("{artifact:projector}"));
        let mut undeclared_port = spec();
        undeclared_port
            .environment
            .insert("METRICS".to_owned(), template("{service:metrics.port}"));
        let mut undeclared_host = spec();
        undeclared_host
            .arguments
            .push(template("{service:metrics.host}"));
        let mut duplicate_binding = spec();
        duplicate_binding
            .artifacts
            .push(duplicate_binding.artifacts[0].clone());
        let mut duplicate_port = spec();
        duplicate_port.ports.push(duplicate_port.ports[0].clone());
        let mut empty_executable = spec();
        empty_executable.executable.path.clear();
        let mut storage_over_artifact = spec();
        storage_over_artifact
            .storage
            .push(crate::WorkloadStorageAttachment {
                name: "engine-cache".to_owned(),
                destination: storage_over_artifact.artifacts[0].destination.clone(),
                kind: crate::WorkloadStorageKind::Cache,
                namespace: "deployment".to_owned(),
                key: "deployment-one".to_owned(),
                read_only: false,
                expires_when_unused: false,
            });

        assert_eq!(spec().validate(), Ok(()));
        for (spec, expected) in [
            (
                undeclared_binding,
                WorkloadSpecError::UndeclaredArtifactBinding {
                    binding: "projector".to_owned(),
                },
            ),
            (
                undeclared_port,
                WorkloadSpecError::UndeclaredServicePort {
                    service: "metrics".to_owned(),
                },
            ),
            (
                undeclared_host,
                WorkloadSpecError::UndeclaredServicePort {
                    service: "metrics".to_owned(),
                },
            ),
            (
                duplicate_binding,
                WorkloadSpecError::DuplicateArtifactBinding {
                    binding: "model".to_owned(),
                },
            ),
            (
                duplicate_port,
                WorkloadSpecError::DuplicatePort {
                    port: "http".to_owned(),
                },
            ),
            (empty_executable, WorkloadSpecError::EmptyExecutablePath),
            (
                storage_over_artifact,
                WorkloadSpecError::StorageDestinationIsArtifact {
                    storage: "engine-cache".to_owned(),
                    destination: "/opt/artifacts/model.bin".to_owned(),
                },
            ),
        ] {
            assert_eq!(spec.validate(), Err(expected.clone()));
            assert_eq!(spec.resolve(&ContainerResolver).map(|_| ()), Err(expected));
        }
    }

    #[test]
    fn runtimes_resolve_tokens_to_their_own_values() {
        let spec = spec();

        assert_eq!(
            spec.resolve(&ContainerResolver),
            Ok(ResolvedWorkloadCommand {
                executable: "/app/server".to_owned(),
                arguments: strings([
                    "--model",
                    "/opt/artifacts/model.bin",
                    "--host",
                    "0.0.0.0",
                    "--port=8080",
                    "--options={\"json\":true}",
                ]),
                environment: BTreeMap::from([
                    ("LAUNCHER".to_owned(), "/app/server".to_owned()),
                    ("MODE".to_owned(), "serve".to_owned()),
                ]),
            })
        );
        assert_eq!(
            spec.resolve(&HostResolver { port: 41_234 }),
            Ok(ResolvedWorkloadCommand {
                executable: "/store/server/app/server".to_owned(),
                arguments: strings([
                    "--model",
                    "/cache/artifacts/model.bin",
                    "--host",
                    "127.0.0.1",
                    "--port=41234",
                    "--options={\"json\":true}",
                ]),
                environment: BTreeMap::from([
                    ("LAUNCHER".to_owned(), "/store/server/app/server".to_owned()),
                    ("MODE".to_owned(), "serve".to_owned()),
                ]),
            })
        );
    }

    #[test]
    fn identity_excludes_runtime_assigned_values() {
        let spec = spec();
        let target = target();
        let generation = spec.reconcile_generation(&target);
        let launch_fingerprint = fingerprint(&spec);
        let first = spec
            .resolve(&HostResolver { port: 41_234 })
            .expect("first resolution");
        let second = spec
            .resolve(&HostResolver { port: 52_001 })
            .expect("second resolution");

        assert_ne!(first.arguments, second.arguments);
        assert_eq!(spec.reconcile_generation(&target), generation);
        assert_eq!(fingerprint(&spec), launch_fingerprint);

        let mut relocated = spec.clone();
        relocated.artifacts[0].source = PathBuf::from("/cache/moved/model.bin");
        assert_eq!(
            spec.reconcile_generation(&target),
            relocated.reconcile_generation(&target)
        );
        assert_ne!(fingerprint(&spec), fingerprint(&relocated));

        let mut renumbered = spec.clone();
        renumbered.ports[0].container_port = 9090;
        assert_ne!(
            spec.reconcile_generation(&target),
            renumbered.reconcile_generation(&target)
        );
        assert_ne!(fingerprint(&spec), fingerprint(&renumbered));
    }

    #[test]
    fn tokens_and_lookalike_literals_have_distinct_identities() {
        let target = target();
        let mut symbolic = literal_spec();
        symbolic.arguments[1] = template("{service:http.port}");
        let mut lookalike = literal_spec();
        lookalike.arguments[1] = WorkloadValue::literal("{service:http.port}");

        assert_ne!(
            symbolic.reconcile_generation(&target),
            lookalike.reconcile_generation(&target)
        );
        assert_ne!(fingerprint(&symbolic), fingerprint(&lookalike));
        assert_ne!(
            symbolic.reconcile_generation(&target),
            literal_spec().reconcile_generation(&target)
        );
    }

    struct ContainerResolver;

    impl WorkloadTokenResolver for ContainerResolver {
        fn executable(&self, executable: &WorkloadExecutable) -> String {
            executable.path.clone()
        }

        fn artifact(&self, binding: &WorkloadArtifactBinding) -> String {
            binding.destination.clone()
        }

        fn service_host(&self, _: &WorkloadPort) -> String {
            "0.0.0.0".to_owned()
        }

        fn service_port(&self, port: &WorkloadPort) -> u16 {
            port.container_port
        }
    }

    struct HostResolver {
        port: u16,
    }

    impl WorkloadTokenResolver for HostResolver {
        fn executable(&self, executable: &WorkloadExecutable) -> String {
            format!("/store/server{}", executable.path)
        }

        fn artifact(&self, binding: &WorkloadArtifactBinding) -> String {
            binding.source.display().to_string()
        }

        fn service_host(&self, _: &WorkloadPort) -> String {
            "127.0.0.1".to_owned()
        }

        fn service_port(&self, _: &WorkloadPort) -> u16 {
            self.port
        }
    }

    fn spec() -> WorkloadSpec {
        WorkloadSpec {
            executable: WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::OciImage("image@sha256:abc".to_owned()),
                path: "/app/server".to_owned(),
            },
            required_platform: ExecutionPlatform::linux_amd64(),
            arguments: vec![
                WorkloadValue::literal("--model"),
                template("{artifact:model}"),
                WorkloadValue::literal("--host"),
                template("{service:http.host}"),
                template("--port={service:http.port}"),
                WorkloadValue::literal("--options={\"json\":true}"),
            ],
            environment: BTreeMap::from([
                ("LAUNCHER".to_owned(), template("{engine.executable}")),
                ("MODE".to_owned(), WorkloadValue::literal("serve")),
            ]),
            artifacts: vec![WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: PathBuf::from("/cache/artifacts/model.bin"),
                destination: "/opt/artifacts/model.bin".to_owned(),
                read_only: true,
            }],
            storage: Vec::new(),
            ports: vec![WorkloadPort {
                name: "http".to_owned(),
                container_port: 8080,
            }],
            accelerator_device_ids: vec!["gpu-0".to_owned()],
            required_accelerator_device_count: 1,
            ipc_mode: WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        }
    }

    fn literal_spec() -> WorkloadSpec {
        WorkloadSpec {
            executable: WorkloadExecutable {
                artifact: WorkloadExecutableArtifact::OciImage("image@sha256:abc".to_owned()),
                path: "serve".to_owned(),
            },
            required_platform: ExecutionPlatform::linux_amd64(),
            arguments: vec![
                WorkloadValue::literal("--port"),
                WorkloadValue::literal("8080"),
            ],
            environment: BTreeMap::from([
                ("A".to_owned(), WorkloadValue::literal("1")),
                ("B".to_owned(), WorkloadValue::literal("2")),
            ]),
            artifacts: vec![
                WorkloadArtifactBinding {
                    name: "cache".to_owned(),
                    source: PathBuf::from("/host/one"),
                    destination: "/cache".to_owned(),
                    read_only: true,
                },
                WorkloadArtifactBinding {
                    name: "data".to_owned(),
                    source: PathBuf::from("/host/two"),
                    destination: "/data".to_owned(),
                    read_only: false,
                },
            ],
            storage: Vec::new(),
            ports: vec![
                WorkloadPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                },
                WorkloadPort {
                    name: "metrics".to_owned(),
                    container_port: 9090,
                },
            ],
            accelerator_device_ids: vec!["gpu-0".to_owned()],
            required_accelerator_device_count: 1,
            ipc_mode: WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        }
    }

    fn target() -> ExecutionTarget {
        ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-a"),
            accelerator_device_ids: vec!["gpu-0".to_owned(), "gpu-1".to_owned()],
        }
    }

    fn fingerprint(spec: &WorkloadSpec) -> String {
        let mut digest = Sha256::new();
        spec.update_exact_intent(&mut digest);
        hex::encode(digest.finalize())
    }

    fn template(value: &str) -> WorkloadValue {
        WorkloadValue::parse(value).expect("valid template")
    }

    fn unknown(token: &str) -> WorkloadSpecError {
        WorkloadSpecError::UnknownToken {
            token: token.to_owned(),
        }
    }

    fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
        values.into_iter().map(str::to_owned).collect()
    }
}
