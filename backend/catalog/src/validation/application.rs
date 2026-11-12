//! Application declaration graph, service, and binding validation.

use anyhow::anyhow;
use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::Application;

use super::graph::validate_application_graph;
use crate::OciArtifactPolicy;

pub fn validate_application_with_policy(
    application: &Application,
    oci_artifact_policy: OciArtifactPolicy,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if !valid_id(&application.application_id) {
        errors.push(anyhow!(
            "application id {:?} must match [a-z][a-z0-9-]{{0,62}}",
            application.application_id
        ));
    }
    if application.display_name.is_empty() || application.version.is_empty() {
        errors.push(anyhow!(
            "application displayName and version must not be empty"
        ));
    }
    if !valid_notes(&application.notes) {
        errors.push(anyhow!(
            "application notes must be at most {MAX_NOTES_BYTES} bytes and contain no control characters other than line breaks"
        ));
    }
    match application.graph.as_ref() {
        Some(graph) => errors.extend(validate_application_graph(graph, oci_artifact_policy)),
        None => errors.push(anyhow!("application requires a service graph")),
    }
    errors
}

const MAX_NOTES_BYTES: usize = 1000;

fn valid_notes(notes: &str) -> bool {
    notes.len() <= MAX_NOTES_BYTES && !notes.chars().any(|c| c.is_control() && c != '\n')
}

pub fn valid_environment_variable(value: &str) -> bool {
    let mut values = value.bytes();
    matches!(values.next(), Some(b'A'..=b'Z' | b'_'))
        && values.all(|value| value.is_ascii_uppercase() || value.is_ascii_digit() || value == b'_')
}

pub fn valid_configuration_value(value: &str) -> bool {
    !value.contains('=') && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use dotcore_domain::test_support::nix as nix_fixture;
    use dotcore_domain::test_support::oci::{ServiceArtifactOci as _, digest_artifact};
    use dotcore_proto::dotcore::catalog::v1::{
        Application, ApplicationConfigurationBinding, ApplicationDependencyCondition,
        ApplicationDeploymentInput, ApplicationEndpointProtocol, ApplicationGraph,
        ApplicationHttpReadiness, ApplicationInferenceRouteInput,
        ApplicationInferenceRouteProjection, ApplicationPrimaryEndpoint, ApplicationSecretBinding,
        ApplicationSecretForm, ApplicationService, ApplicationServiceDependency,
        ApplicationServicePort, ApplicationServiceReadiness, BooleanParameterSchema, NixArtifact,
        NixDockerImage, NixSystemStorePath, OciPlatform, ParameterDefinition, ParameterVisibility,
        SecretParameterSchema, StorageBinding, StorageKind, StorageScope,
        StringEnumParameterSchema, TextParameterSchema, UnsignedIntegerParameterSchema,
        application_configuration_binding,
        application_service_artifact::Kind as ServiceArtifactKind, nix_artifact::Provides,
        parameter_definition::ParameterType,
    };

    use super::validate_application_with_policy;
    use crate::OciArtifactPolicy;

    fn validate_application(application: &Application) -> Vec<anyhow::Error> {
        validate_application_with_policy(application, OciArtifactPolicy::DigestRequired)
    }

    fn application() -> Application {
        Application {
            application_id: "open-webui".to_owned(),
            display_name: "Open WebUI".to_owned(),
            description: String::new(),
            notes: String::new(),
            version: "0.10.1".to_owned(),
            graph: Some(ApplicationGraph {
                services: vec![
                    ApplicationService {
                        service_name: "database".to_owned(),
                        artifact: Some(digest_artifact(
                            Some("ghcr.io"),
                            "example/database",
                            "5c75d3cc7ff9c199c07e157ae7710370e2b698979a70a64639ead03b7cf4c679",
                            None,
                        )),
                        ports: vec![ApplicationServicePort {
                            port_name: "postgres".to_owned(),
                            port_number: 5432,
                        }],
                        readiness: Some(http_readiness("postgres", "/ready")),
                        ..Default::default()
                    },
                    ApplicationService {
                        service_name: "web".to_owned(),
                        artifact: Some(digest_artifact(
                            Some("ghcr.io"),
                            "example/web",
                            "5c75d3cc7ff9c199c07e157ae7710370e2b698979a70a64639ead03b7cf4c679",
                            None,
                        )),
                        ports: vec![ApplicationServicePort {
                            port_name: "http".to_owned(),
                            port_number: 8080,
                        }],
                        configuration_bindings: vec![ApplicationConfigurationBinding {
                            binding_name: "database-url".to_owned(),
                            environment_variable: "DATABASE_URL".to_owned(),
                            source: Some(application_configuration_binding::Source::LiteralValue(
                                "postgres://database".to_owned(),
                            )),
                            input: None,
                        }],
                        storage_bindings: vec![StorageBinding {
                            binding_name: "data".to_owned(),
                            mount_path: "/app/data".to_owned(),
                            kind: StorageKind::Data as i32,
                            scope: StorageScope::Deployment as i32,
                            environment: Vec::new(),
                        }],
                        dependencies: vec![ApplicationServiceDependency {
                            service_name: "database".to_owned(),
                            condition: ApplicationDependencyCondition::Ready as i32,
                        }],
                        readiness: Some(http_readiness("http", "/health")),
                        ..Default::default()
                    },
                ],
                primary_endpoint: Some(ApplicationPrimaryEndpoint {
                    service_name: "web".to_owned(),
                    port_name: "http".to_owned(),
                    protocol: ApplicationEndpointProtocol::Http as i32,
                }),
                parameters: Vec::new(),
            }),
        }
    }

    #[test]
    fn application_services_accept_docker_delivered_nix_artifacts() {
        let nix_service = |systems: &[&str], provides: Provides| {
            let mut application = application();
            let artifact = application.graph.as_mut().expect("graph").services[1]
                .artifact
                .as_mut()
                .expect("artifact");
            artifact.supported_platforms = vec![platform("amd64"), platform("arm64")];
            artifact.kind = Some(ServiceArtifactKind::Nix(NixArtifact {
                flake: nix_fixture::locked_flake(),
                package: Some("apps.web.docker".to_owned()),
                provides: Some(provides),
                store_paths: systems
                    .iter()
                    .map(|system| NixSystemStorePath {
                        system: (*system).to_owned(),
                        store_path: nix_fixture::DOCKER_IMAGE_STORE_PATH.to_owned(),
                    })
                    .collect(),
            }));
            validate_application(&application)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let image = || Provides::DockerImage(NixDockerImage {});

        assert_eq!(
            nix_service(
                &["x86_64-linux", "aarch64-linux", "aarch64-darwin"],
                image()
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            nix_service(&["x86_64-linux", "aarch64-linux"], image()),
            [
                "application service \"web\" has an invalid Nix artifact: locked Nix artifact must declare the store path of every Nix system its declared platforms need; aarch64-darwin has none"
            ]
        );
        assert_eq!(
            nix_service(&[], Provides::Executable("web".to_owned())),
            [
                "application service \"web\" has an invalid Nix artifact: application service Nix artifact must provide a Docker image (`dockerImage`), not a native executable"
            ]
        );
    }

    #[test]
    fn application_validation_bounds_notes_and_rejects_control_characters() {
        let notes_error = |notes: &str| {
            let mut application = application();
            application.notes = notes.to_owned();
            validate_application(&application)
                .iter()
                .any(|error| error.to_string().contains("application notes must be"))
        };

        assert!(!notes_error(""));
        assert!(!notes_error(&format!(
            "{}\n{}",
            "a".repeat(499),
            "b".repeat(500)
        )));
        assert!(notes_error(&"a".repeat(1001)));
        assert!(notes_error("two\tcolumns"));
        assert!(notes_error("carriage\rreturn"));
        // Bounded by bytes, not characters.
        assert!(notes_error(&"é".repeat(501)));
    }

    #[test]
    fn application_validation_requires_nonempty_unique_concrete_linux_platforms() {
        let cases = [
            (Vec::new(), "at least one supported platform"),
            (
                vec![dotcore_proto::dotcore::catalog::v1::OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "any".to_owned(),
                    variant: None,
                }],
                "concrete linux/amd64 or linux/arm64",
            ),
            (
                vec![dotcore_proto::dotcore::catalog::v1::OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: Some("v8".to_owned()),
                }],
                "without variants",
            ),
            (
                vec![
                    dotcore_proto::dotcore::catalog::v1::OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "amd64".to_owned(),
                        variant: None,
                    },
                    dotcore_proto::dotcore::catalog::v1::OciPlatform {
                        operating_system: "linux".to_owned(),
                        architecture: "amd64".to_owned(),
                        variant: None,
                    },
                ],
                "duplicate supported platform",
            ),
        ];
        for (platforms, expected) in cases {
            let mut application = application();
            application.graph.as_mut().expect("graph").services[0]
                .artifact
                .as_mut()
                .expect("artifact")
                .supported_platforms = platforms;

            assert!(
                validate_application(&application)
                    .iter()
                    .any(|error| error.to_string().contains(expected)),
                "expected {expected}"
            );
        }
    }

    #[test]
    fn application_validation_accepts_a_complete_two_service_graph() {
        assert!(validate_application(&application()).is_empty());
    }

    #[test]
    fn application_validation_rejects_control_characters_in_command_and_arguments() {
        for (command, arguments) in [
            (
                vec!["sh".to_owned(), "-c".to_owned(), "a\nb".to_owned()],
                vec![],
            ),
            (vec!["app".to_owned()], vec!["--flag\t1".to_owned()]),
        ] {
            let mut application = application();
            let service = &mut application.graph.as_mut().expect("graph").services[0];
            service.command = command;
            service.arguments = arguments;

            assert!(
                validate_application(&application).iter().any(|error| {
                    error
                        .to_string()
                        .contains("command and arguments must not contain control characters")
                }),
                "control characters must be rejected"
            );
        }

        let mut application = application();
        let service = &mut application.graph.as_mut().expect("graph").services[0];
        service.command = vec!["sh".to_owned(), "-c".to_owned(), "exec app".to_owned()];
        assert!(validate_application(&application).is_empty());
    }

    #[test]
    fn application_validation_rejects_invalid_graph_references_and_names() {
        let mut application = application();
        let graph = application.graph.as_mut().expect("graph");
        let reference = graph.services[0]
            .artifact
            .as_mut()
            .expect("artifact")
            .oci_mut()
            .expect("reference");
        reference.tag = Some("latest".to_owned());
        reference.digest = None;
        graph.services[1].service_name = "database".to_owned();
        graph.services[1].ports.push(ApplicationServicePort {
            port_name: "http".to_owned(),
            port_number: 0,
        });
        graph.services[1].dependencies[0].service_name = "missing".to_owned();
        graph.services[1].dependencies[0].condition = 0;
        graph.services[1]
            .readiness
            .as_mut()
            .expect("readiness")
            .check = Some(
            dotcore_proto::dotcore::catalog::v1::application_service_readiness::Check::HttpGet(
                ApplicationHttpReadiness {
                    port_name: "missing".to_owned(),
                    path: "health".to_owned(),
                },
            ),
        );
        let endpoint = graph.primary_endpoint.as_mut().expect("endpoint");
        endpoint.service_name = "database".to_owned();
        endpoint.port_name = "missing".to_owned();

        let errors = validate_application(&application);

        assert!(errors.iter().any(|error| {
            error
                .to_string()
                .contains("OCI artifact policy requires a digest")
        }));
        assert!(errors.iter().any(|error| {
            error
                .to_string()
                .contains("duplicate application service name")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("internal TCP port"))
        );
        assert!(errors.iter().any(|error| {
            error
                .to_string()
                .contains("duplicate application service port name")
        }));
        assert!(!errors.iter().any(|error| {
            let message = error.to_string();
            message.contains("missing dependency service")
                || message.contains("dependencies must be acyclic")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("STARTED or READY"))
        );
        assert!(errors.iter().any(|error| {
            error
                .to_string()
                .contains("readiness references missing port")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("absolute HTTP path"))
        );
        assert!(!errors.iter().any(|error| {
            error
                .to_string()
                .contains("primary endpoint references missing port")
        }));
    }

    #[test]
    fn application_validation_rejects_dependency_cycles_and_missing_primary_endpoints() {
        let mut application = application();
        let graph = application.graph.as_mut().expect("graph");
        graph.services[0]
            .dependencies
            .push(ApplicationServiceDependency {
                service_name: "web".to_owned(),
                condition: ApplicationDependencyCondition::Started as i32,
            });
        graph.primary_endpoint = None;

        let errors = validate_application(&application);

        assert!(
            errors.iter().any(|error| error.to_string()
                == "application graph requires exactly one primary endpoint")
        );
        assert!(
            errors.iter().any(
                |error| error.to_string() == "application service dependencies must be acyclic"
            )
        );
    }

    #[test]
    fn application_validation_requires_safe_unique_configuration_sources_and_destinations() {
        let mut application = application();
        let web = &mut application.graph.as_mut().expect("graph").services[1];
        web.configuration_bindings[0].source = None;
        web.configuration_bindings
            .push(ApplicationConfigurationBinding {
                binding_name: "unsafe-value".to_owned(),
                environment_variable: "UNSAFE_VALUE".to_owned(),
                source: Some(application_configuration_binding::Source::LiteralValue(
                    "not=safe".to_owned(),
                )),
                input: None,
            });
        web.configuration_bindings
            .push(ApplicationConfigurationBinding {
                binding_name: "invalid-source".to_owned(),
                environment_variable: "INVALID_SOURCE".to_owned(),
                source: Some(
                    application_configuration_binding::Source::ResolvedValueName(
                        "invalid_source".to_owned(),
                    ),
                ),
                input: None,
            });
        web.secret_bindings.push(ApplicationSecretBinding {
            binding_name: "secret".to_owned(),
            environment_variable: "DATABASE_URL".to_owned(),
            parameter_id: "password".to_owned(),
            form: ApplicationSecretForm::Plain as i32,
        });

        let errors = validate_application(&application)
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();

        assert!(
            errors
                .iter()
                .any(|error| error.contains("exactly one source"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("unsafe literal value"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("valid resolved value name"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("duplicate application environment destination"))
        );
    }

    #[test]
    fn application_validation_requires_a_known_kind_for_deployment_inputs() {
        let mut application = application();
        application.graph.as_mut().expect("graph").services[1].configuration_bindings[0].input =
            Some(ApplicationDeploymentInput { kind: None });

        let errors = validate_application(&application)
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();

        assert!(errors.iter().any(|error| {
            error.contains("application deployment input \"database-url\" requires a known kind")
        }));
    }

    #[test]
    fn application_validation_requires_a_closed_inference_route_projection() {
        let mut application = application();
        application.graph.as_mut().expect("graph").services[1].configuration_bindings[0].input =
            Some(ApplicationDeploymentInput {
                kind: Some(
                    dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                        ApplicationInferenceRouteInput::default(),
                    ),
                ),
            });

        let errors = validate_application(&application)
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();

        assert!(errors.iter().any(|error| {
            error.contains(
                "application inference route input \"database-url\" requires a known projection",
            )
        }));

        application.graph.as_mut().expect("graph").services[1].configuration_bindings[0].input =
            Some(ApplicationDeploymentInput {
                kind: Some(
                    dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                        ApplicationInferenceRouteInput {
                            projection: ApplicationInferenceRouteProjection::RouteName as i32,
                        },
                    ),
                ),
            });
        assert!(validate_application(&application).is_empty());
    }

    #[test]
    fn application_validation_requires_a_literal_fallback_for_inference_route_inputs() {
        let mut application = application();
        let binding =
            &mut application.graph.as_mut().expect("graph").services[1].configuration_bindings[0];
        binding.source = Some(
            application_configuration_binding::Source::ResolvedValueName(
                "configured-route".to_owned(),
            ),
        );
        binding.input = Some(ApplicationDeploymentInput {
            kind: Some(
                dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                    ApplicationInferenceRouteInput {
                        projection: ApplicationInferenceRouteProjection::RouteName as i32,
                    },
                ),
            ),
        });

        let errors = validate_application(&application)
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            errors,
            [
                "application inference route input \"database-url\" requires a literal fallback value"
            ]
        );
    }

    #[test]
    fn application_yaml_rejects_both_configuration_source_variants() {
        dotcore_proto::yaml::from_str::<Application>(
            r#"graph:
  services:
    - configurationBindings:
        - bindingName: "provider"
          environmentVariable: "PROVIDER_URL"
          literalValue: "http://example.test/v1"
          resolvedValueName: "configured-provider"
"#,
        )
        .expect_err("a configuration binding cannot select both oneof source variants");
    }

    #[test]
    fn application_validation_accepts_parameters_of_each_type() {
        let application = parameterized_application(
            vec![
                parameter("debug", boolean(Some(false))),
                parameter("workers", unsigned(1, 16, Some(2))),
                parameter("mode", string_enum(&["auto", "highvram"], Some("auto"))),
            ],
            &["debug", "workers", "mode"],
        );

        assert!(validate_application(&application).is_empty());
    }

    #[test]
    fn application_validation_rejects_invalid_parameters() {
        let cases = [
            (
                parameterized_application(
                    vec![
                        parameter("mode", string_enum(&["auto"], Some("auto"))),
                        parameter("mode", string_enum(&["auto"], Some("auto"))),
                    ],
                    &["mode"],
                ),
                "duplicate application parameter \"mode\"",
            ),
            (
                parameterized_application(
                    vec![parameter("mode", string_enum(&["auto"], None))],
                    &["mode"],
                ),
                "application parameter \"mode\" requires a default",
            ),
            (
                parameterized_application(
                    vec![parameter("workers", unsigned(1, 16, Some(17)))],
                    &["workers"],
                ),
                "parameter \"workers\" default is invalid: 17 is outside 1..=16",
            ),
            (
                parameterized_application(
                    vec![parameter("mode", string_enum(&["a=b"], Some("a=b")))],
                    &["mode"],
                ),
                "application parameter \"mode\" has an unsafe enum value",
            ),
            (
                parameterized_application(
                    vec![parameter("mode", string_enum(&["auto"], Some("auto")))],
                    &["mode", "other"],
                ),
                "application configuration binding \"other\" on service \"web\" references missing parameter \"other\"",
            ),
            (
                parameterized_application(
                    vec![
                        parameter("mode", string_enum(&["auto"], Some("auto"))),
                        parameter("debug", boolean(Some(true))),
                    ],
                    &["mode"],
                ),
                "application parameter \"debug\" is not used by any binding",
            ),
            (
                parameterized_application(
                    vec![parameter_with_visibility(
                        "mode",
                        string_enum(&["auto"], Some("auto")),
                        ParameterVisibility::Hidden,
                    )],
                    &["mode"],
                ),
                "application parameter \"mode\" must be basic or advanced, not hidden",
            ),
            (
                parameterized_application(
                    vec![parameter_with_visibility(
                        "mode",
                        string_enum(&["auto"], Some("auto")),
                        ParameterVisibility::Unspecified,
                    )],
                    &["mode"],
                ),
                "parameter \"mode\" requires a basic, advanced or hidden visibility",
            ),
        ];

        for (application, expected) in cases {
            let errors = validate_application(&application)
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            assert_eq!(errors, [expected]);
        }
    }

    #[test]
    fn application_validation_accepts_text_and_secret_parameters() {
        let mut application = parameterized_application(
            vec![
                parameter("username", text(1, 64, Some("admin"))),
                parameter("password", secret(32, 12)),
            ],
            &["username"],
        );
        bind_secret(&mut application, "password", "password");

        assert!(validate_application(&application).is_empty());
    }

    #[test]
    fn application_validation_rejects_invalid_text_and_secret_parameters() {
        let with_secret_binding = |parameters, bound_ids: &[&str], secret_id: &str| {
            let mut application = parameterized_application(parameters, bound_ids);
            bind_secret(&mut application, "password", secret_id);
            application
        };
        let cases = [
            (
                with_secret_binding(vec![parameter("password", secret(8, 12))], &[], "password"),
                "parameter \"password\" minimum length must not exceed its generated length",
            ),
            (
                with_secret_binding(vec![parameter("password", secret(32, 0))], &[], "password"),
                "parameter \"password\" minimum length must be at least 1",
            ),
            (
                parameterized_application(
                    vec![parameter("username", text(1, 4, Some("administrator")))],
                    &["username"],
                ),
                "parameter \"username\" default is invalid: it is longer than the maximum length of 4",
            ),
            (
                parameterized_application(
                    vec![parameter("username", text(6, 64, Some("admin")))],
                    &["username"],
                ),
                "parameter \"username\" default is invalid: it is shorter than the minimum length of 6",
            ),
            (
                parameterized_application(
                    vec![parameter("username", text(1, 64, Some("a=b")))],
                    &["username"],
                ),
                "parameter \"username\" default is invalid: it contains `=` or a control character",
            ),
            (
                parameterized_application(
                    vec![parameter("username", text(0, 0, Some("")))],
                    &["username"],
                ),
                "parameter \"username\" maximum length must be at least 1",
            ),
            (
                parameterized_application(
                    vec![parameter("username", text(1, 64, None))],
                    &["username"],
                ),
                "application parameter \"username\" requires a default",
            ),
            (
                with_secret_binding(vec![parameter("password", secret(32, 12))], &[], "missing"),
                "application secret binding \"password\" on service \"web\" references missing parameter \"missing\"",
            ),
            (
                with_secret_binding(
                    vec![
                        parameter("username", text(1, 64, Some("admin"))),
                        parameter("password", secret(32, 12)),
                    ],
                    &["password"],
                    "username",
                ),
                "application configuration binding \"password\" on service \"web\" references secret parameter \"password\"; secrets use secret bindings",
            ),
        ];

        for (application, expected) in cases {
            let errors = validate_application(&application)
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            assert!(
                errors.iter().any(|error| error == expected),
                "expected {expected:?} in {errors:?}"
            );
        }

        let not_secret = with_secret_binding(
            vec![parameter("username", text(1, 64, Some("admin")))],
            &[],
            "username",
        );
        assert_eq!(
            validate_application(&not_secret)
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>(),
            [
                "application secret binding \"password\" on service \"web\" references parameter \"username\", which is not a secret"
            ]
        );

        let mut unknown_form =
            parameterized_application(vec![parameter("password", secret(32, 12))], &[]);
        bind_secret(&mut unknown_form, "password", "password");
        unknown_form.graph.as_mut().expect("graph").services[1].secret_bindings[0].form =
            ApplicationSecretForm::Unspecified as i32;
        assert_eq!(
            validate_application(&unknown_form)
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>(),
            ["application secret binding \"password\" requires a known form"]
        );
    }

    #[test]
    fn application_yaml_declares_secret_parameters_without_a_default() {
        let graph = dotcore_proto::yaml::from_str::<ApplicationGraph>(
            r#"parameters:
  - parameterId: "password"
    displayName: "password"
    visibility: "PARAMETER_VISIBILITY_BASIC"
    secret:
      generatedLength: 32
      minimumLength: 12
services:
  - secretBindings:
      - bindingName: "password"
        environmentVariable: "PASSWORD"
        parameterId: "password"
        form: "APPLICATION_SECRET_FORM_PLAIN"
"#,
        )
        .expect("secret graph decodes");

        assert_eq!(graph.parameters, [parameter("password", secret(32, 12))]);
        assert_eq!(
            graph.services[0].secret_bindings,
            [ApplicationSecretBinding {
                binding_name: "password".to_owned(),
                environment_variable: "PASSWORD".to_owned(),
                parameter_id: "password".to_owned(),
                form: ApplicationSecretForm::Plain as i32,
            }]
        );
        dotcore_proto::yaml::from_str::<ApplicationGraph>(
            r#"parameters:
  - parameterId: "password"
    displayName: "password"
    visibility: "PARAMETER_VISIBILITY_BASIC"
    secret:
      generatedLength: 32
      minimumLength: 12
      defaultValue: "dotcore"
"#,
        )
        .expect_err("a secret parameter cannot declare a default");
    }

    #[test]
    fn application_validation_rejects_a_parameter_binding_with_a_route_input() {
        let mut application = parameterized_application(
            vec![parameter("mode", string_enum(&["auto"], Some("auto")))],
            &["mode"],
        );
        let binding = application.graph.as_mut().expect("graph").services[1]
            .configuration_bindings
            .last_mut()
            .expect("parameter binding");
        binding.input = Some(ApplicationDeploymentInput {
            kind: Some(
                dotcore_proto::dotcore::catalog::v1::application_deployment_input::Kind::InferenceRoute(
                    ApplicationInferenceRouteInput {
                        projection: ApplicationInferenceRouteProjection::RouteName as i32,
                    },
                ),
            ),
        });

        let errors = validate_application(&application)
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();

        assert_eq!(
            errors,
            ["application inference route input \"mode\" requires a literal fallback value"]
        );
    }

    #[test]
    fn application_yaml_declares_parameters_and_parameter_bindings() {
        let graph = dotcore_proto::yaml::from_str::<ApplicationGraph>(
            r#"parameters:
  - parameterId: "mode"
    displayName: "mode"
    visibility: "PARAMETER_VISIBILITY_ADVANCED"
    stringEnum:
      values: ["auto", "highvram"]
      defaultValue: "auto"
services:
  - configurationBindings:
      - bindingName: "mode"
        environmentVariable: "MODE"
        parameterId: "mode"
"#,
        )
        .expect("parameter graph decodes");

        assert_eq!(
            graph.parameters,
            [parameter_with_visibility(
                "mode",
                string_enum(&["auto", "highvram"], Some("auto")),
                ParameterVisibility::Advanced
            )]
        );
        assert_eq!(
            graph.services[0].configuration_bindings[0].source,
            Some(application_configuration_binding::Source::ParameterId(
                "mode".to_owned()
            ))
        );
        dotcore_proto::yaml::from_str::<Application>(
            r#"graph:
  services:
    - configurationBindings:
        - bindingName: "mode"
          environmentVariable: "MODE"
          literalValue: "auto"
          parameterId: "mode"
"#,
        )
        .expect_err("a configuration binding cannot select both a literal and a parameter");
    }

    fn http_readiness(port_name: &str, path: &str) -> ApplicationServiceReadiness {
        ApplicationServiceReadiness {
            check: Some(
                dotcore_proto::dotcore::catalog::v1::application_service_readiness::Check::HttpGet(
                    ApplicationHttpReadiness {
                        port_name: port_name.to_owned(),
                        path: path.to_owned(),
                    },
                ),
            ),
        }
    }

    fn platform(architecture: &str) -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        }
    }

    fn parameterized_application(
        parameters: Vec<ParameterDefinition>,
        bound_ids: &[&str],
    ) -> Application {
        let mut application = application();
        let graph = application.graph.as_mut().expect("graph");
        graph.parameters = parameters;
        for id in bound_ids {
            graph.services[1]
                .configuration_bindings
                .push(ApplicationConfigurationBinding {
                    binding_name: (*id).to_owned(),
                    environment_variable: id.to_uppercase(),
                    source: Some(application_configuration_binding::Source::ParameterId(
                        (*id).to_owned(),
                    )),
                    input: None,
                });
        }
        application
    }

    fn bind_secret(application: &mut Application, binding_name: &str, parameter_id: &str) {
        application.graph.as_mut().expect("graph").services[1]
            .secret_bindings
            .push(ApplicationSecretBinding {
                binding_name: binding_name.to_owned(),
                environment_variable: binding_name.to_uppercase(),
                parameter_id: parameter_id.to_owned(),
                form: ApplicationSecretForm::Plain as i32,
            });
    }

    fn parameter(id: &str, parameter_type: ParameterType) -> ParameterDefinition {
        parameter_with_visibility(id, parameter_type, ParameterVisibility::Basic)
    }

    fn parameter_with_visibility(
        id: &str,
        parameter_type: ParameterType,
        visibility: ParameterVisibility,
    ) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            visibility: visibility as i32,
            parameter_type: Some(parameter_type),
        }
    }

    fn boolean(default_value: Option<bool>) -> ParameterType {
        ParameterType::Boolean(BooleanParameterSchema { default_value })
    }

    fn unsigned(minimum: u64, maximum: u64, default_value: Option<u64>) -> ParameterType {
        ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
            minimum,
            maximum,
            unit: String::new(),
            default_value,
        })
    }

    fn text(
        minimum_length: u32,
        maximum_length: u32,
        default_value: Option<&str>,
    ) -> ParameterType {
        ParameterType::Text(TextParameterSchema {
            minimum_length,
            maximum_length,
            default_value: default_value.map(str::to_owned),
        })
    }

    fn secret(generated_length: u32, minimum_length: u32) -> ParameterType {
        ParameterType::Secret(SecretParameterSchema {
            generated_length,
            minimum_length,
        })
    }

    fn string_enum(values: &[&str], default_value: Option<&str>) -> ParameterType {
        ParameterType::StringEnum(StringEnumParameterSchema {
            values: values.iter().map(|value| (*value).to_owned()).collect(),
            default_value: default_value.map(str::to_owned),
        })
    }
}
