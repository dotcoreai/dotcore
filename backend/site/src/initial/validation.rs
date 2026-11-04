//! Side-effect-free validation of one complete initial site input bundle.

use std::path::{Path, PathBuf};

use dotcore_catalog::{Catalog, EngineDeclarationValidator, OciArtifactPolicy};
use dotcore_proto::{dotcore::node::v1::NodeConfiguration, yaml};

use crate::{
    NODE_FILE, SITE_FILE,
    node::binding::{NodeBindingError, bound_site_node},
    repository::SiteRepositoryError,
    validation::{SiteDeclarationRules, load_validated_site},
};

/// Validates one site input directory without opening site store or runtime capabilities.
///
/// Engine-specific catalog rules come from the caller's engine declaration validator, so site
/// validation names no engine.
#[derive(Clone)]
pub struct SiteInputBundleValidator {
    engine_validator: &'static dyn EngineDeclarationValidator,
    rules: SiteDeclarationRules,
    platform_catalog_root: Option<PathBuf>,
    oci_artifact_policy: OciArtifactPolicy,
}

impl SiteInputBundleValidator {
    /// Creates a validator that checks the site declarations with the owners' `rules`, and the
    /// site-local catalog with the given engine rules when it is present.
    pub fn new(
        engine_validator: &'static dyn EngineDeclarationValidator,
        rules: SiteDeclarationRules,
    ) -> Self {
        Self {
            engine_validator,
            rules,
            platform_catalog_root: None,
            oci_artifact_policy: OciArtifactPolicy::default(),
        }
    }

    /// Creates a validator for site unit tests, whose catalog fixtures declare no engine.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(
            &dotcore_catalog::test_support::ACCEPT_ENGINE_DECLARATIONS,
            crate::test_support::site_declaration_rules(),
        )
    }

    /// Adds the platform catalog source for cross-source validation.
    pub fn with_platform_catalog_root(mut self, platform_catalog_root: impl Into<PathBuf>) -> Self {
        self.platform_catalog_root = Some(platform_catalog_root.into());
        self
    }

    /// Selects the OCI artifact policy for catalog validation.
    pub fn with_oci_artifact_policy(mut self, policy: OciArtifactPolicy) -> Self {
        self.oci_artifact_policy = policy;
        self
    }

    /// Returns the owner rules the site declarations must satisfy.
    pub fn declaration_rules(&self) -> SiteDeclarationRules {
        self.rules
    }

    /// Validates site, node binding, and selected catalog inputs without mutating any filesystem path.
    pub fn validate(&self, site_root: &Path) -> Result<(), SiteInputValidationError> {
        self.validate_bundle(site_root, true)
    }

    /// Validates the bundle of a site that may already hold deployments and routes.
    pub fn validate_without_runtime_declarations(
        &self,
        site_root: &Path,
    ) -> Result<(), SiteInputValidationError> {
        self.validate_bundle(site_root, false)
    }

    fn validate_bundle(
        &self,
        site_root: &Path,
        reject_runtime_declarations: bool,
    ) -> Result<(), SiteInputValidationError> {
        let site = load_validated_site(&site_root.join(SITE_FILE), &self.rules)?;
        if reject_runtime_declarations && contains_runtime_declarations(&site) {
            return Err(SiteInputValidationError::RuntimeDeclarations);
        }
        let node: NodeConfiguration = yaml::from_file(&site_root.join(NODE_FILE))?;
        bound_site_node(&site, &node)?;
        self.validate_catalog(site_root)?;
        Ok(())
    }

    fn validate_catalog(&self, site_root: &Path) -> Result<(), SiteInputValidationError> {
        let site_catalog_root = site_root.join("catalog");
        match &self.platform_catalog_root {
            Some(platform_catalog_root) => {
                Catalog::load_sources_with_policy(
                    platform_catalog_root,
                    &site_catalog_root,
                    self.oci_artifact_policy,
                    self.engine_validator,
                )
                .map_err(SiteInputValidationError::CatalogComposition)?;
            }
            None if site_catalog_root.exists() => {
                Catalog::load_with_policy(
                    &site_catalog_root,
                    self.oci_artifact_policy,
                    self.engine_validator,
                )
                .map_err(SiteInputValidationError::SiteCatalog)?;
            }
            None => {}
        }
        Ok(())
    }
}

/// Failures from read-only site input validation.
#[derive(Debug, thiserror::Error)]
pub enum SiteInputValidationError {
    /// Desired site YAML or semantic validation failed.
    #[error("site input validation failed: {0}")]
    Site(#[from] SiteRepositoryError),

    /// Runtime-owned declarations cannot be authored through initial site inputs.
    #[error(
        "Site input validation failed: initial site inputs cannot contain runtime-owned declarations"
    )]
    RuntimeDeclarations,

    /// Server-local node binding YAML could not be decoded.
    #[error("node.yaml validation failed: {0}")]
    NodeConfiguration(#[from] dotcore_proto::yaml::RestrictedYamlError),

    /// Server-local node binding did not refer to exactly one declared SiteNode.
    #[error("node.yaml binding validation failed: {0}")]
    NodeBinding(#[from] NodeBindingError),

    /// Site-local catalog declarations were invalid.
    #[error("site catalog validation failed: {0}")]
    SiteCatalog(#[source] dotcore_catalog::CatalogError),

    /// Platform and site catalog sources could not compose together.
    #[error("Platform and site catalog composition failed: {0}")]
    CatalogComposition(#[source] dotcore_catalog::CatalogError),
}

fn contains_runtime_declarations(site: &dotcore_proto::dotcore::site::v1::Site) -> bool {
    !site.model_deployments.is_empty()
        || !site.application_deployments.is_empty()
        || !site.inference_routes.is_empty()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{SiteInputBundleValidator, SiteInputValidationError};

    #[test]
    fn validates_a_complete_bundle_and_ignores_site_store_artifacts() {
        let fixture = SiteFixture::valid();
        fs::write(fixture.root.join("state.pb"), "unreadable retained state")
            .expect("generated site store state");
        fs::create_dir(fixture.root.join("applications-data")).expect("generated application data");

        SiteInputBundleValidator::for_test()
            .validate(&fixture.root)
            .expect("complete site input bundle");

        assert_eq!(
            fs::read_to_string(fixture.root.join("state.pb")).expect("retained state"),
            "unreadable retained state"
        );
    }

    #[test]
    fn rejects_malformed_site_yaml_and_semantically_invalid_sites() {
        let malformed = SiteFixture::new();
        malformed.write_node();
        fs::write(malformed.root.join("site.yaml"), "siteId: [\n").expect("malformed site");

        let malformed_error = SiteInputBundleValidator::for_test()
            .validate(&malformed.root)
            .expect_err("malformed YAML");
        assert!(matches!(malformed_error, SiteInputValidationError::Site(_)));

        let invalid = SiteFixture::new();
        invalid.write_node();
        invalid.write_site(VALID_SITE_YAML.replace("siteId: site-test", "siteId: Invalid"));

        let invalid_error = SiteInputBundleValidator::for_test()
            .validate(&invalid.root)
            .expect_err("invalid site semantics");
        assert!(matches!(invalid_error, SiteInputValidationError::Site(_)));
    }

    #[test]
    fn rejects_missing_and_mismatched_node_bindings_without_changing_membership() {
        let missing = SiteFixture::new();
        missing.write_site(VALID_SITE_YAML);

        let missing_error = SiteInputBundleValidator::for_test()
            .validate(&missing.root)
            .expect_err("node binding is required");
        assert!(matches!(
            missing_error,
            SiteInputValidationError::NodeConfiguration(_)
        ));

        let mismatched = SiteFixture::valid();
        fs::write(mismatched.root.join("node.yaml"), "nodeId: other-node\n")
            .expect("mismatched binding");

        let mismatch_error = SiteInputBundleValidator::for_test()
            .validate(&mismatched.root)
            .expect_err("node must be declared");
        assert!(matches!(
            mismatch_error,
            SiteInputValidationError::NodeBinding(_)
        ));
        assert_eq!(
            fs::read_to_string(mismatched.root.join("site.yaml")).expect("Site input"),
            VALID_SITE_YAML
        );
    }

    #[test]
    fn rejects_invalid_log_retention_before_validation_completes() {
        for logging_retention_days in [0, 366] {
            let fixture = SiteFixture::valid();
            fs::write(
                fixture.root.join("node.yaml"),
                format!("logging:\n  retentionDays: {logging_retention_days}\nnodeId: node-test\n"),
            )
            .expect("invalid log retention policy");

            let error = SiteInputBundleValidator::for_test()
                .validate(&fixture.root)
                .expect_err("invalid log retention policy");
            assert!(matches!(error, SiteInputValidationError::NodeBinding(_)));
        }
    }

    #[test]
    fn validates_present_site_catalog_and_platform_source_composition() {
        let valid_catalog = SiteFixture::valid();
        valid_catalog.catalog_directories();
        fs::write(
            valid_catalog
                .root
                .join("catalog/applications/fixture-application.yaml"),
            APPLICATION_CATALOG,
        )
        .expect("valid site catalog application");
        SiteInputBundleValidator::for_test()
            .validate(&valid_catalog.root)
            .expect("valid site catalog");

        let invalid_catalog = SiteFixture::valid();
        invalid_catalog.catalog_directories();
        fs::write(
            invalid_catalog.root.join("catalog/models/broken.yaml"),
            "modelId: [\n",
        )
        .expect("invalid catalog");

        let catalog_error = SiteInputBundleValidator::for_test()
            .validate(&invalid_catalog.root)
            .expect_err("invalid site catalog");
        assert!(matches!(
            catalog_error,
            SiteInputValidationError::SiteCatalog(_)
        ));

        let conflict = SiteFixture::valid();
        conflict.catalog_directories();
        fs::write(
            conflict
                .root
                .join("catalog/applications/fixture-application.yaml"),
            APPLICATION_CATALOG,
        )
        .expect("Site catalog application");
        let platform = SiteFixture::new();
        platform.catalog_directories();
        fs::write(
            platform
                .root
                .join("catalog/applications/fixture-application.yaml"),
            APPLICATION_CATALOG,
        )
        .expect("platform catalog application");

        let conflict_error = SiteInputBundleValidator::for_test()
            .with_platform_catalog_root(platform.root.join("catalog"))
            .validate(&conflict.root)
            .expect_err("Catalog identity conflict");
        assert!(matches!(
            conflict_error,
            SiteInputValidationError::CatalogComposition(_)
        ));
        assert!(
            conflict_error
                .to_string()
                .contains("duplicate application id")
        );
    }

    #[test]
    fn never_creates_a_missing_site_root() {
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-site-input-validation-missing-{}",
            dotcore_domain::test_support::unique_suffix()
        ));

        let error = SiteInputBundleValidator::for_test()
            .validate(&root)
            .expect_err("missing site root");

        assert!(matches!(error, SiteInputValidationError::Site(_)));
        assert!(!root.exists());
    }

    const VALID_SITE_YAML: &str = "siteId: site-test\nmodelDeploymentIdentityAllocator: { namespace: 0123456789abcdef, nextSequence: '1' }\nnodes:\n- nodeId: node-test\n  host: 127.0.0.1\n  displayName: Test node\n  siteManager: {}\nexecutionTargets:\n- executionTargetId: target-test\n  siteNodeId: node-test\n  displayName: target-test\n  platform: { operatingSystem: linux, architecture: amd64 }\n  modelRuntime:\n    simulatedRuntime:\n      defaultScenario: { readyDelayMillis: '2000', stopDelayMillis: '2000' }\n- executionTargetId: application-target-test\n  siteNodeId: node-test\n  displayName: application-target-test\n  platform: { operatingSystem: linux, architecture: amd64 }\n  applicationRuntime: { simulatedRuntime: {} }\n";
    const APPLICATION_CATALOG: &str = "applicationId: fixture-application\ndisplayName: Fixture application\nversion: '1'\ngraph:\n  services:\n  - serviceName: web\n    artifact:\n      oci:\n        repository: { registry: { host: example.invalid }, name: fixture/web }\n        digest: { algorithm: sha256, encoded: '1111111111111111111111111111111111111111111111111111111111111111' }\n      supportedPlatforms:\n      - { operatingSystem: linux, architecture: amd64 }\n    ports:\n    - { portName: http, portNumber: 8080 }\n  primaryEndpoint: { serviceName: web, portName: http, protocol: APPLICATION_ENDPOINT_PROTOCOL_HTTP }\n";

    struct SiteFixture {
        root: PathBuf,
    }

    impl SiteFixture {
        fn new() -> Self {
            let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
                "dotcore-site-input-validation-{}",
                dotcore_domain::test_support::unique_suffix()
            ));
            fs::create_dir_all(&root).expect("Site fixture root");
            Self { root }
        }

        fn valid() -> Self {
            let fixture = Self::new();
            fixture.write_site(VALID_SITE_YAML);
            fixture.write_node();
            fixture
        }

        fn write_site(&self, contents: impl AsRef<str>) {
            fs::write(self.root.join("site.yaml"), contents.as_ref()).expect("Site input");
        }

        fn write_node(&self) {
            fs::write(self.root.join("node.yaml"), "nodeId: node-test\n").expect("node input");
        }

        fn catalog_directories(&self) {
            for directory in ["models", "engines", "applications"] {
                fs::create_dir_all(self.root.join("catalog").join(directory))
                    .expect("Catalog directory");
            }
        }
    }

    impl Drop for SiteFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
