//! Explicitly gated Docker application lifecycle integration coverage.
//!
//! This suite deploys small synthetic applications against a real Docker runtime to pin
//! configuration and inference-route rendering, storage retention, pause and resume, container
//! recovery, local-development image identity, deployment isolation, and logs. It is ignored
//! unless the Docker applications gate is enabled; simulated lifecycle and public transport behavior
//! belong to ordinary backend integration suites.

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions, TryLockError},
    future::Future,
    net::Ipv4Addr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bollard::{Docker, errors::Error as BollardError, query_parameters::CreateImageOptionsBuilder};
use dotcore_domain::SitePaths;
use dotcore_proto::dotcore::application::v1::{
    ApplicationDeployment, ApplicationDeploymentDesiredState, ApplicationLifecycleState,
    ApplicationReadinessState, CreateApplicationDeploymentRequest, FollowDeploymentLogsRequest,
    GetApplicationDeploymentRequest, PauseDeploymentRequest, ReadDeploymentLogsRequest,
    ReadDeploymentLogsResponse, RemoveApplicationDeploymentRequest, ResumeDeploymentRequest,
    application_deployment_service_client::ApplicationDeploymentServiceClient,
    application_deployment_service_server::ApplicationDeploymentService as ApplicationServiceContract,
};
use dotcore_proto::dotcore::catalog::v1::{
    ListApplicationsRequest, OciPlatform,
    catalog_service_server::CatalogService as CatalogServiceContract,
};
use dotcore_proto::dotcore::deployment::v1::ArtifactPreparationPolicy;
#[cfg(not(target_os = "macos"))]
use dotcore_proto::dotcore::deployment::v1::accelerator_memory_observation;
use dotcore_proto::dotcore::inference::v1::{ListRoutesRequest, ReplaceRouteRequest};
use dotcore_proto::dotcore::model::v1::{
    CreateModelDeploymentRequest, DirectRouteExposure, ModelDeploymentExposure,
    model_deployment_exposure,
};
use dotcore_runtime::{
    WorkloadCorrelation, WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupLaunch, WorkloadPort,
    WorkloadPublicationRequest, WorkloadUnitLaunch, WorkloadUnitName, WorkloadUnitResources,
    WorkloadUnitResourcesUnavailable,
};
use dotcore_runtime_backends::docker::{
    DockerAcceleratorTransport, DockerBackendFactory, DockerRuntimeFactory,
};
use prost::Message;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, sleep, timeout},
};
use tokio_stream::StreamExt;
use tonic::{Code, Request};

use crate::support;

type ApplicationClient = ApplicationDeploymentServiceClient<tonic::transport::Channel>;

const READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const TARGET_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds waiting for output a ready workload already wrote to appear in Docker's log history.
const LOG_HISTORY_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds waiting for dropped follows to release their admission on the manager and the node.
const ADMISSION_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const IMAGE_INSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const IMAGE_DIAGNOSTIC_MAX_CHARS: usize = 256;
const BUSYBOX_IMAGE: &str =
    "busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";
const BUSYBOX_DIGEST: &str = "73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";
/// Debian-based so the driver's glibc `libcuda` that CDI injects loads into its Python.
#[cfg(not(target_os = "macos"))]
const PYTHON_IMAGE: &str =
    "python@sha256:b3575dcf816cdacc5292df77a6e3e59b8941631078373a045efa2ac1e9c5c3b2";
#[cfg(not(target_os = "macos"))]
const PYTHON_DIGEST: &str = "b3575dcf816cdacc5292df77a6e3e59b8941631078373a045efa2ac1e9c5c3b2";
const ARM64_DEFAULT_VARIANT: &str = "v8";
const LIFECYCLE_APPLICATION_ID: &str = "synthetic-lifecycle";
const STORAGE_SCOPES_APPLICATION_ID: &str = "synthetic-storage-scopes";
const SECRET_APPLICATION_ID: &str = "secret-fixture";
/// A typed secret value no other site content contains.
const SECRET_SENTINEL: &str = "SentinelQv8Rk2Ld7Wn4";
const LOCAL_IMAGE_REPOSITORY: &str = "dotcore-test/synthetic-lifecycle";
const UNROUTED_VALUE: &str = "unrouted";
const SUPPORTED_PLATFORMS: &str = "        supportedPlatforms:\n          - operatingSystem: \"linux\"\n            architecture: \"amd64\"\n          - operatingSystem: \"linux\"\n            architecture: \"arm64\"\n";
const STORAGE_MARKER_FILE: &str = ".dotcore-application-integration-marker";
const STORAGE_MARKER_CONTENT: &str = "dotcore-application-storage-retained\n";

/// The CDI device selector the NVIDIA qualification host injects into group units.
#[cfg(not(target_os = "macos"))]
const NVIDIA_CDI_SELECTOR: &str = "nvidia.com/gpu=0";
/// GPU memory the accelerator-memory fixture's unit allocates on top of its CUDA context.
#[cfg(not(target_os = "macos"))]
const GPU_FIXTURE_ALLOCATION_BYTES: u64 = 256 * 1024 * 1024;
/// Execution target of the runtime the group resources check drives directly.
const RESOURCES_TARGET_ID: &str = "docker-resources-target";

#[test]
fn temporary_site_configuration_supplies_its_own_publication_host() {
    let root = crate::support::canonical_temp_dir().join(format!(
        "dotcore-docker-apps-publication-host-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("read system clock")
            .as_nanos(),
    ));
    fs::create_dir(&root).expect("create temporary site root");
    let result = (|| {
        materialize_temporary_site_configuration(
            &root,
            "docker-apps-test",
            "192.0.2.1",
            &OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            },
        )?;
        configured_publication_host(&root)
    })();
    fs::remove_dir_all(&root).expect("remove temporary site root");
    assert_eq!(
        result.expect("read temporary site configuration"),
        Ipv4Addr::new(192, 0, 2, 1)
    );
}

#[test]
fn temporary_site_configuration_uses_the_supplied_native_platform() {
    let root = crate::support::canonical_temp_dir().join(format!(
        "dotcore-docker-apps-platform-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("read system clock")
            .as_nanos(),
    ));
    fs::create_dir(&root).expect("create temporary site root");
    materialize_temporary_site_configuration(
        &root,
        "docker-apps-test",
        "127.0.0.1",
        &OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        },
    )
    .expect("materialize arm64 site");
    let site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("load arm64 site");
    fs::remove_dir_all(&root).expect("remove temporary site root");

    assert_eq!(
        site.execution_targets
            .iter()
            .find(|target| target.execution_target_id == "docker-apps-target")
            .and_then(|target| target.platform.as_ref()),
        Some(&OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        })
    );
}

#[tokio::test]
async fn backend_release_probe_preserves_immediate_restart() {
    let fixture = support::temporary_fast_fixture();
    let configuration = support::configuration(&fixture);
    let first = support::RunningBackend::start(configuration.clone()).await;

    first.stop().await;
    wait_for_backend_service_release(&configuration)
        .await
        .expect("released backend site lock");

    let restarted = support::RunningBackend::start(configuration).await;
    restarted.stop().await;
}

#[test]
fn docker_platform_aliases_normalize_to_supported_oci_platforms() {
    assert_eq!(
        normalize_docker_platform("linux", "x86_64").expect("normalize x86_64"),
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }
    );
    assert_eq!(
        normalize_docker_platform("linux", "aarch64").expect("normalize aarch64"),
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: None,
        }
    );
    assert_eq!(
        normalize_docker_platform("darwin", "arm64").expect_err("reject Darwin"),
        "Docker application integration requires Docker operating system linux, got darwin"
    );
    assert_eq!(
        normalize_docker_platform("linux", "riscv64").expect_err("reject unknown architecture"),
        "Docker application integration requires Docker architecture amd64 or arm64, got riscv64"
    );
}

#[test]
fn docker_gate_accepts_native_daemons_and_docker_desktop_only_on_macos() {
    assert_eq!(
        verify_native_docker_daemon("linux", "Docker Desktop", "arm64", "arm64")
            .expect_err("reject Docker Desktop on Linux"),
        "Docker application integration does not run on Docker Desktop on Linux"
    );
    assert_eq!(
        verify_native_docker_daemon("linux", "Ubuntu 24.04", "amd64", "arm64")
            .expect_err("reject cross-architecture daemon"),
        "Docker application integration requires a native Docker daemon matching host architecture arm64, got amd64"
    );
    verify_native_docker_daemon("linux", "Ubuntu 24.04", "arm64", "arm64")
        .expect("accept native Docker");
    assert_eq!(
        verify_native_docker_daemon("macos", "Colima", "arm64", "arm64")
            .expect_err("reject other macOS Docker daemons"),
        "Docker application integration requires Docker Desktop on macOS"
    );
    assert_eq!(
        verify_native_docker_daemon("macos", "Docker Desktop", "amd64", "arm64")
            .expect_err("reject emulated Docker Desktop"),
        "Docker application integration requires a native Docker daemon matching host architecture arm64, got amd64"
    );
    verify_native_docker_daemon("macos", "Docker Desktop", "arm64", "arm64")
        .expect("accept Docker Desktop on macOS");
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_local_development_image_missing_on_the_node_fails_then_recovers_by_its_pinned_image_id()
 {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    let result = async {
        let (image_tag, image_id) = resources.create_local_image()?;
        let archive = resources.root.join("local-image.tar");
        command_owned(
            "docker",
            &[
                "save".to_owned(),
                "--output".to_owned(),
                path_argument(&archive)?,
                image_tag.clone(),
            ],
        )?;
        resources.write_lifecycle_application(&local_image_artifact(&resources.site_id))?;
        let service = resources
            .composition_services()
            .await
            .map_err(|error| format!("start local-image apps-only site: {error}"))?;
        let deployment = ApplicationServiceContract::create_application_deployment(
            &service,
            Request::new(lifecycle_deployment_request(None)),
        )
        .await
        .map_err(|error| format!("create local-image application: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "create local-image application returned no deployment".to_owned())?;
        let deployment_id = deployment.application_deployment_id.clone();
        let snapshot = deployment
            .snapshot
            .as_ref()
            .ok_or_else(|| "local-image deployment has no resolved snapshot".to_owned())?;
        let declared_image = snapshot
            .graph
            .as_ref()
            .and_then(|graph| graph.services.first())
            .and_then(|service| service.artifact.as_ref())
            .and_then(dotcore_domain::oci::service_artifact_reference)
            .and_then(support::oci_reference);
        if declared_image.as_deref() != Some(image_tag.as_str()) {
            return Err(format!(
                "local-image snapshot did not retain the development tag: {declared_image:?}"
            ));
        }
        if snapshot.service_selections.len() != 1
            || !snapshot.service_selections[0].launch_identity.is_empty()
            || snapshot.service_selections[0].preparation_policy
                != ArtifactPreparationPolicy::LocalDevelopment as i32
        {
            return Err(format!(
                "local-image snapshot did not leave the development identity to the hosting node: {:?}",
                snapshot.service_selections
            ));
        }
        // Removing the only tag deletes the image before the hosting node first reconciles the
        // deployment, so the node cannot pin it and reports a typed failure without launching.
        command("docker", ["image", "rm", &image_tag])?;
        service.start_observer();
        let unavailable =
            wait_for_application_stage(&service, &deployment_id, "launch-preparation").await?;
        let state = unavailable
            .state
            .as_ref()
            .ok_or_else(|| "missing-image deployment has no state".to_owned())?;
        if state.lifecycle_state != ApplicationLifecycleState::Unavailable as i32
            || state.readiness_state != ApplicationReadinessState::NotReady as i32
            || state.endpoint.is_some()
            || state.failure.as_ref().map(|failure| failure.code.as_str())
                != Some("LaunchIdentityUnresolved")
        {
            return Err(format!(
                "missing-image deployment did not project a safe unresolved state: {state:?}"
            ));
        }
        if !resources.container_ids(true, None)?.is_empty() {
            return Err("missing-image deployment created Docker containers".to_owned());
        }

        command_owned(
            "docker",
            &[
                "load".to_owned(),
                "--input".to_owned(),
                path_argument(&archive)?,
            ],
        )?;
        // The restored tag resolves on a later reconciliation; the node pins its image ID once.
        wait_for_endpoint(&service, &deployment_id).await?;
        let container = resources.container_id_for_group(&deployment_id, "web")?;
        let launched_image = command("docker", ["inspect", "--format", "{{.Image}}", &container])?;
        let configured_image =
            command("docker", ["inspect", "--format", "{{.Config.Image}}", &container])?;
        if launched_image != image_id || configured_image != image_id {
            return Err(format!(
                "recovered workload did not launch by the pinned image ID {image_id}: image={launched_image} configured={configured_image}"
            ));
        }
        let log_driver =
            command("docker", ["inspect", "--format", "{{.HostConfig.LogConfig.Type}}", &container])?;
        if log_driver != "local" {
            return Err(format!(
                "application workload log driver is {log_driver:?}, not \"local\""
            ));
        }

        ApplicationServiceContract::remove_application_deployment(
            &service,
            Request::new(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment_id.clone(),
            }),
        )
        .await
        .map_err(|error| format!("remove local-image application: {error}"))?;
        resources
            .wait_for_no_docker_resources(&service, &deployment_id)
            .await?;
        drop(service);
        Ok(())
    }
    .await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_route_bound_application_keeps_rendered_configuration_and_data_through_lifecycle_operations()
 {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    resources
        .write_fixture_inference_route()
        .expect("fixture inference route site");
    resources
        .write_lifecycle_application(&remote_image_artifact())
        .expect("lifecycle application catalog declaration");
    let backend = support::RunningBackend::start_at_with_simulated_runtime_test_support(
        resources.configuration.clone(),
        resources.publication_host.into(),
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
    )
    .await;
    let result = async {
        let mut models = backend.model_client();
        let mut routes = backend.route_client();
        let mut applications = backend.application_client();
        models
            .create_model_deployment(CreateModelDeploymentRequest {
                source: Some(
                    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                        dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                            model_id: "fixture-model".to_owned(),
                            profile_id: "fixture-profile".to_owned(),
                            placement: Some(support::exact_placement(
                                "docker-apps",
                                "fixture-target",
                            )),
                            configuration: Some(support::local_model_configuration(
                                "fixture-artifact",
                                32_768,
                            )),
                        },
                    ),
                ),
                exposure: Some(ModelDeploymentExposure {
                    kind: Some(model_deployment_exposure::Kind::Direct(DirectRouteExposure {
                        route_name: "fixture-route".to_owned(),
                    })),
                }),
                model_deployment_name: None,
            })
            .await
            .map_err(|error| format!("load fixture model: {error}"))?;
        let route = wait_for_fixture_route(&mut routes).await?;
        let deployment = create_when_target_observed(
            &mut applications,
            lifecycle_deployment_request(Some(&route)),
        )
        .await?;
        let deployment_id = deployment.application_deployment_id.clone();
        let snapshot = deployment
            .snapshot
            .as_ref()
            .ok_or_else(|| "route-bound deployment has no resolved snapshot".to_owned())?;
        if snapshot.placement.as_ref()
            != Some(&support::exact_placement(
                "docker-apps",
                "docker-apps-target",
            ))
            || snapshot.platform.as_ref() != Some(&resources.platform)
            || snapshot.service_selections.len() != 1
            || snapshot.service_selections[0].preparation_policy
                != ArtifactPreparationPolicy::ImmutableRemote as i32
        {
            return Err(format!(
                "route-bound snapshot did not retain the native target, platform, and pinned image: {snapshot:?}"
            ));
        }
        let storage_name = storage_name(&deployment)?;
        let data = resources
            .seed_realized_application_data(&deployment_id, storage_name)
            .await?;
        let routed = ConfigurationExpectation {
            inference_base_url: format!("http://{}/v1", backend.inference_address()),
            route_name: "fixture-route".to_owned(),
        };
        let ready =
            wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &routed)
                .await?;
        wait_for_http_body(&endpoint_url(&ready)?, "lifecycle-ready").await?;

        let running_group = resources.group_containers(&deployment_id)?;
        assert_running_group(&running_group)?;
        let network = resources.only_network_id_for_group(&deployment_id)?;
        let pausing = applications
            .pause_deployment(PauseDeploymentRequest {
                application_deployment_id: deployment_id.clone(),
            })
            .await
            .map_err(|error| format!("pause lifecycle application: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "pause lifecycle application returned no deployment".to_owned())?;
        assert_suppressed_application(&pausing, ApplicationLifecycleState::Pausing)?;
        let paused = wait_for_application_lifecycle_client(
            &mut applications,
            &deployment_id,
            ApplicationLifecycleState::Paused,
        )
        .await?;
        assert_suppressed_application(&paused, ApplicationLifecycleState::Paused)?;
        let paused_group = resources.group_containers(&deployment_id)?;
        assert_stopped_group(&paused_group)?;
        assert_retained_group(&running_group, &paused_group)?;
        if resources.only_network_id_for_group(&deployment_id)? != network {
            return Err("Pause replaced the retained Docker network".to_owned());
        }
        resources.assert_storage_marker(&data)?;

        let resumed = applications
            .resume_deployment(ResumeDeploymentRequest {
                application_deployment_id: deployment_id.clone(),
            })
            .await
            .map_err(|error| format!("resume lifecycle application: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "resume lifecycle application returned no deployment".to_owned())?;
        if resumed.desired_state != ApplicationDeploymentDesiredState::Present as i32 {
            return Err("Resume did not restore PRESENT desired state".to_owned());
        }
        wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &routed)
            .await?;
        let resumed_group = resources.group_containers(&deployment_id)?;
        assert_running_group(&resumed_group)?;
        assert_retained_group(&running_group, &resumed_group)?;
        if resources.only_network_id_for_group(&deployment_id)? != network {
            return Err("Resume replaced the retained Docker network".to_owned());
        }

        let stopped = resources.running_container_id_for_group(&deployment_id, "web")?;
        command("docker", ["stop", &stopped])?;
        wait_for_group_recovery(&resources, &deployment_id, "web", None).await?;
        wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &routed)
            .await?;

        let removed = resources.running_container_id_for_group(&deployment_id, "web")?;
        command("docker", ["rm", "-f", &removed])?;
        wait_for_group_replacement(&resources, &deployment_id, "web", &removed).await?;
        wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &routed)
            .await?;

        let renamed_route = routes
            .replace_route(ReplaceRouteRequest {
                inference_route_id: route.inference_route_id.clone(),
                expected_generation: route.generation,
                name: "fixture-route-renamed".to_owned(),
                enabled: route.enabled,
                fallback_groups: route.fallback_groups.clone(),
            })
            .await
            .map_err(|error| format!("rename fixture inference route: {error}"))?
            .into_inner()
            .route
            .and_then(|view| view.route)
            .ok_or_else(|| "rename fixture inference route returned no route".to_owned())?;
        if renamed_route.name != "fixture-route-renamed" {
            return Err("fixture inference route rename returned the wrong route".to_owned());
        }
        let renamed = ConfigurationExpectation {
            route_name: "fixture-route-renamed".to_owned(),
            ..routed
        };
        wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &renamed)
            .await?;
        Ok::<
            (
                String,
                PathBuf,
                dotcore_proto::dotcore::inference::v1::InferenceRoute,
                String,
            ),
            String,
        >((deployment_id, data, renamed_route, renamed.inference_base_url))
    }
    .await;
    backend.stop().await;
    if result.is_ok() {
        wait_for_backend_service_release(&resources.configuration)
            .await
            .expect("release backend repositories before lifecycle restart");
    }
    let result = match result {
        Ok((deployment_id, data, route, inference_base_url)) => {
            let restarted = support::RunningBackend::start_at_with_simulated_runtime_test_support(
                resources.configuration.clone(),
                resources.publication_host.into(),
                dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
            )
            .await;
            let restart_result = async {
                let mut applications = restarted.application_client();
                let mut routes = restarted.route_client();
                // The base URL resolved at creation is retained across the listener change.
                let restarted_expectation = ConfigurationExpectation {
                    inference_base_url,
                    route_name: "fixture-route-renamed".to_owned(),
                };
                let retained = wait_for_lifecycle_runtime(
                    &resources,
                    &mut applications,
                    &deployment_id,
                    &data,
                    &restarted_expectation,
                )
                .await?;
                if retained.placement
                    != Some(support::exact_placement(
                        "docker-apps",
                        "docker-apps-target",
                    ))
                {
                    return Err("backend restart did not retain exact placement".to_owned());
                }
                routes
                    .replace_route(ReplaceRouteRequest {
                        inference_route_id: route.inference_route_id,
                        expected_generation: route.generation,
                        name: route.name,
                        enabled: false,
                        fallback_groups: route.fallback_groups,
                    })
                    .await
                    .map_err(|error| {
                        format!("disable fixture inference route before teardown: {error}")
                    })?;
                applications
                    .remove_application_deployment(RemoveApplicationDeploymentRequest {
                        application_deployment_id: deployment_id.clone(),
                    })
                    .await
                    .map_err(|error| {
                        format!("remove lifecycle application after backend restart: {error}")
                    })?;
                wait_for_no_docker_resources_client(&resources, &mut applications, &deployment_id)
                    .await?;
                resources.wait_for_application_data_removed(&data).await
            }
            .await;
            restarted.stop().await;
            restart_result
        }
        Err(error) => Err(error),
    };
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_two_deployments_of_one_application_isolate_data_and_networks() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    resources
        .write_lifecycle_application(&remote_image_artifact())
        .expect("lifecycle application catalog declaration");
    let backend = support::RunningBackend::start_at_with_simulated_runtime_test_support(
        resources.configuration.clone(),
        resources.publication_host.into(),
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
    )
    .await;
    let result = async {
        let mut applications = backend.application_client();
        let site: dotcore_proto::dotcore::site::v1::Site =
            dotcore_proto::yaml::from_file(&resources.root.join("site.yaml"))
                .map_err(|error| format!("load exact application target: {error}"))?;
        let application_targets = site
            .execution_targets
            .iter()
            .filter(|target| {
                matches!(
                    target.runtime_binding.as_ref(),
                    Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(_))
                )
            })
            .collect::<Vec<_>>();
        if application_targets.len() != 1
            || application_targets[0].site_node_id != "docker-apps"
            || application_targets[0].execution_target_id != "docker-apps-target"
        {
            return Err("apps-only site did not declare its exact local Docker target".to_owned());
        }

        let mut deployments = Vec::new();
        let inference_base_url = format!("http://{}/v1", backend.inference_address());
        for name in ["first", "second"] {
            let deployment = create_when_target_observed(
                &mut applications,
                lifecycle_deployment_request(None),
            )
            .await?;
            let storage_name = storage_name(&deployment)?;
            let data = resources
                .seed_realized_application_data(&deployment.application_deployment_id, storage_name)
                .await?;
            resources.write_marker(&data, name, &format!("{name} deployment data\n"))?;
            deployments.push((
                name,
                deployment.application_deployment_id,
                data,
                ConfigurationExpectation {
                    inference_base_url: inference_base_url.clone(),
                    route_name: UNROUTED_VALUE.to_owned(),
                },
            ));
        }
        let [
            (first_name, first_id, first_data, first),
            (second_name, second_id, second_data, second),
        ] = &deployments[..]
        else {
            return Err("expected exactly two lifecycle deployments".to_owned());
        };
        if first_data == second_data {
            return Err("lifecycle deployments share an application-data root".to_owned());
        }
        wait_for_lifecycle_runtime(&resources, &mut applications, first_id, first_data, first)
            .await?;
        wait_for_lifecycle_runtime(
            &resources,
            &mut applications,
            second_id,
            second_data,
            second,
        )
        .await?;
        if resources.only_network_id_for_group(first_id)?
            == resources.only_network_id_for_group(second_id)?
        {
            return Err("lifecycle deployments share a Docker network".to_owned());
        }
        for (id, own, other) in [
            (first_id, first_name, second_name),
            (second_id, second_name, first_name),
        ] {
            let container = resources.container_id_for_group(id, "web")?;
            if container_file(&container, &format!("/data/{own}"))?
                != format!("{own} deployment data\n")
            {
                return Err(format!(
                    "{own} deployment does not see its own application data"
                ));
            }
            if container_file(&container, &format!("/data/{other}")).is_ok() {
                return Err(format!(
                    "{own} deployment sees {other} deployment application data"
                ));
            }
        }

        for id in [first_id, second_id] {
            applications
                .remove_application_deployment(RemoveApplicationDeploymentRequest {
                    application_deployment_id: id.clone(),
                })
                .await
                .map_err(|error| format!("remove lifecycle application {id}: {error}"))?;
        }
        for id in [first_id, second_id] {
            wait_for_no_docker_resources_client(&resources, &mut applications, id).await?;
        }
        resources.wait_for_application_data_removed(first_data).await?;
        resources.wait_for_application_data_removed(second_data).await
    }
    .await;
    backend.stop().await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_application_scoped_storage_outlives_the_deployment_that_wrote_it() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    resources
        .write_storage_scopes_application(&remote_image_artifact())
        .expect("storage scopes application catalog declaration");
    let backend = support::RunningBackend::start_at_with_simulated_runtime_test_support(
        resources.configuration.clone(),
        resources.publication_host.into(),
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
    )
    .await;
    let result = async {
        let mut applications = backend.application_client();
        let request = || {
            let mut request = lifecycle_deployment_request(None);
            request.application_id = STORAGE_SCOPES_APPLICATION_ID.to_owned();
            request
        };
        let application_root = resources
            .root
            .join("applications-data")
            .join("application")
            .join(STORAGE_SCOPES_APPLICATION_ID);

        // The first deployment writes one model file and one output file.
        let first = create_when_target_observed(&mut applications, request()).await?;
        let first_id = first.application_deployment_id.clone();
        let first_data =
            resources.application_data_path(&first_id, &attached_storage(&first, "data")?);
        let first_models = application_root.join(attached_storage(&first, "models")?);
        let container =
            wait_for_container_exec(&resources, &first_id, "mkdir -p /data/output").await?;
        container_exec(&container, "echo weights > /data/models/model.bin")?;
        container_exec(&container, "echo image > /data/output/image.png")?;
        assert_nested_models_mount(&container, &first_data, &first_models)?;
        if container_file(&container, "/data/models/model.bin")? != "weights\n" {
            return Err("first deployment does not read back its model file".to_owned());
        }
        if !first_data.parent().is_some_and(Path::exists) || !first_models.exists() {
            return Err("running deployment is missing its host storage folders".to_owned());
        }

        // Removing it deletes the deployment folder and keeps the application folder.
        applications
            .remove_application_deployment(RemoveApplicationDeploymentRequest {
                application_deployment_id: first_id.clone(),
            })
            .await
            .map_err(|error| format!("remove first application {first_id}: {error}"))?;
        wait_for_no_docker_resources_client(&resources, &mut applications, &first_id).await?;
        resources
            .wait_for_application_data_removed(&first_data)
            .await?;
        if !first_models.exists() {
            return Err("removing a deployment deleted the application-scoped folder".to_owned());
        }

        // A new deployment sees the model file and none of the first one's output.
        let second = create_when_target_observed(&mut applications, request()).await?;
        let second_id = second.application_deployment_id.clone();
        let second_data =
            resources.application_data_path(&second_id, &attached_storage(&second, "data")?);
        let second_models = application_root.join(attached_storage(&second, "models")?);
        if second_models != first_models {
            return Err(format!(
                "second deployment uses application storage {second_models:?}, not {first_models:?}"
            ));
        }
        let container =
            wait_for_container_exec(&resources, &second_id, "mkdir -p /data/output").await?;
        assert_nested_models_mount(&container, &second_data, &second_models)?;
        if container_file(&container, "/data/models/model.bin")? != "weights\n" {
            return Err("second deployment lost the model file".to_owned());
        }
        if container_file(&container, "/data/output/image.png").is_ok() {
            return Err("second deployment sees the first deployment's output file".to_owned());
        }

        applications
            .remove_application_deployment(RemoveApplicationDeploymentRequest {
                application_deployment_id: second_id.clone(),
            })
            .await
            .map_err(|error| format!("remove second application {second_id}: {error}"))?;
        wait_for_no_docker_resources_client(&resources, &mut applications, &second_id).await?;
        resources
            .wait_for_application_data_removed(&second_data)
            .await
    }
    .await;
    backend.stop().await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_a_taken_published_port_fails_resume_until_released_and_keeps_the_port() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    resources
        .write_lifecycle_application(&remote_image_artifact())
        .expect("lifecycle application catalog declaration");
    let backend = support::RunningBackend::start_at_with_simulated_runtime_test_support(
        resources.configuration.clone(),
        resources.publication_host.into(),
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
    )
    .await;
    let result = async {
        let mut applications = backend.application_client();
        let deployment =
            create_when_target_observed(&mut applications, lifecycle_deployment_request(None))
                .await?;
        let deployment_id = deployment.application_deployment_id.clone();
        let data = resources
            .seed_realized_application_data(&deployment_id, storage_name(&deployment)?)
            .await?;
        let expected = ConfigurationExpectation {
            inference_base_url: format!("http://{}/v1", backend.inference_address()),
            route_name: UNROUTED_VALUE.to_owned(),
        };
        let ready =
            wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &expected)
                .await?;
        let endpoint = endpoint_url(&ready)?;
        let port = endpoint_port(&endpoint)?;
        if !(30_000..=32_767).contains(&port) {
            return Err(format!("published port {port} is outside the published range"));
        }

        applications
            .pause_deployment(PauseDeploymentRequest {
                application_deployment_id: deployment_id.clone(),
            })
            .await
            .map_err(|error| format!("pause application: {error}"))?;
        wait_for_application_lifecycle_client(
            &mut applications,
            &deployment_id,
            ApplicationLifecycleState::Paused,
        )
        .await?;
        let holder = std::net::TcpListener::bind((resources.publication_host, port))
            .map_err(|error| format!("hold published port {port}: {error}"))?;
        applications
            .resume_deployment(ResumeDeploymentRequest {
                application_deployment_id: deployment_id.clone(),
            })
            .await
            .map_err(|error| format!("resume application: {error}"))?;
        let failed = wait_for_failure_code(&mut applications, &deployment_id, "PublishedPortInUse")
            .await?;
        let message = failed
            .state
            .and_then(|state| state.failure)
            .map(|failure| failure.message)
            .unwrap_or_default();
        if message
            != "Application published port is in use on this node; free the port or delete the deployment"
        {
            return Err(format!("unexpected port-in-use message {message:?}"));
        }

        // The controller keeps recreating the deployment on its retained port until it is free.
        drop(holder);
        let recovered =
            wait_for_lifecycle_runtime(&resources, &mut applications, &deployment_id, &data, &expected)
                .await?;
        if endpoint_url(&recovered)? != endpoint {
            return Err("the recovered deployment changed its published endpoint".to_owned());
        }

        applications
            .remove_application_deployment(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment_id.clone(),
            })
            .await
            .map_err(|error| format!("remove application: {error}"))?;
        wait_for_no_docker_resources_client(&resources, &mut applications, &deployment_id).await
    }
    .await;
    backend.stop().await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_network_only_residue_converges_for_present_and_teardown_across_restart() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    let result = async {
        resources.write_lifecycle_application(&remote_image_artifact())?;
        let service = resources
            .composition_services()
            .await
            .map_err(|error| format!("start apps-only site: {error}"))?;
        let deployment = ApplicationServiceContract::create_application_deployment(
            &service,
            Request::new(lifecycle_deployment_request(None)),
        )
        .await
        .map_err(|error| format!("create lifecycle application: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "create lifecycle application returned no deployment".to_owned())?;
        let storage_name = storage_name(&deployment)?;
        service.start_observer();
        let data = resources
            .seed_realized_application_data(&deployment.application_deployment_id, storage_name)
            .await?;
        wait_for_endpoint(&service, &deployment.application_deployment_id).await?;
        drop(service);
        wait_for_backend_service_release(&resources.configuration).await?;

        let original_network =
            resources.only_network_id_for_group(&deployment.application_deployment_id)?;
        resources.remove_group_containers(&deployment.application_deployment_id)?;
        resources.assert_exact_empty_group_network(
            &deployment.application_deployment_id,
            &original_network,
        )?;

        let restarted_present = resources
            .composition_services()
            .await
            .map_err(|error| format!("restart desired PRESENT site: {error}"))?;
        restarted_present.start_observer();
        let catalog = dotcore::composition::catalog_service_for(&resources.configuration)
            .map_err(|error| format!("compose catalog service after restart: {error}"))?;
        CatalogServiceContract::list_applications(
            &catalog,
            Request::new(ListApplicationsRequest {}),
        )
        .await
        .map_err(|error| format!("list catalog applications after restart: {error}"))?;
        wait_for_endpoint(&restarted_present, &deployment.application_deployment_id).await?;
        if resources.only_network_id_for_group(&deployment.application_deployment_id)?
            != original_network
        {
            return Err("desired PRESENT recreated the exact retained group network".to_owned());
        }
        resources.assert_storage_marker(&data)?;
        drop(restarted_present);
        wait_for_backend_service_release(&resources.configuration).await?;

        resources.remove_group_containers(&deployment.application_deployment_id)?;
        resources.assert_exact_empty_group_network(
            &deployment.application_deployment_id,
            &original_network,
        )?;
        let absent_writer = resources
            .composition_services()
            .await
            .map_err(|error| format!("restart before desired TEARDOWN: {error}"))?;
        ApplicationServiceContract::remove_application_deployment(
            &absent_writer,
            Request::new(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
            }),
        )
        .await
        .map_err(|error| format!("persist desired TEARDOWN: {error}"))?;
        drop(absent_writer);
        wait_for_backend_service_release(&resources.configuration).await?;

        let restarted_absent = resources
            .composition_services()
            .await
            .map_err(|error| format!("restart desired TEARDOWN site: {error}"))?;
        restarted_absent.start_observer();
        let catalog = dotcore::composition::catalog_service_for(&resources.configuration)
            .map_err(|error| format!("compose catalog service after TEARDOWN restart: {error}"))?;
        CatalogServiceContract::list_applications(
            &catalog,
            Request::new(ListApplicationsRequest {}),
        )
        .await
        .map_err(|error| format!("list catalog applications after TEARDOWN restart: {error}"))?;
        resources
            .wait_for_no_docker_resources(&restarted_absent, &deployment.application_deployment_id)
            .await?;
        resources.wait_for_application_data_removed(&data).await?;
        drop(restarted_absent);
        Ok(())
    }
    .await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

// Docker Desktop's VM does not route group bridge addresses, which the READY launch gate probes
// from the host.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_synthetic_two_service_group_recovers_with_private_dns_and_one_public_binding() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    let result = async {
        resources.write_synthetic_application()?;
        let service = resources
            .composition_services()
            .await
            .map_err(|error| format!("start synthetic apps-only site: {error}"))?;
        let deployment = ApplicationServiceContract::create_application_deployment(
            &service,
            Request::new(CreateApplicationDeploymentRequest {
                application_id: "synthetic-two-service".to_owned(),
                placement: Some(support::exact_placement(
                    "docker-apps",
                    "docker-apps-target",
                )),
                inference_route_id: None,
                expected_inference_route_generation: 0,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            }),
        )
        .await
        .map_err(|error| format!("create synthetic application: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "create synthetic application returned no deployment".to_owned())?;
        service.start_observer();

        let ready = wait_for_endpoint(&service, &deployment.application_deployment_id).await?;
        let endpoint = endpoint_url(&ready)?;
        if endpoint != resources.docker_endpoint_for_unit("published")? {
            return Err(
                "controller endpoint does not match the sole published Docker binding".to_owned(),
            );
        }
        resources.assert_group_units(&["dependency", "published"])?;
        resources.assert_one_configured_publication("published", "dependency")?;
        wait_for_http_body(&endpoint, "private-dns-ok").await?;

        let stopped_container = resources.running_container_id_for_unit("published")?;
        command("docker", ["stop", &stopped_container])?;
        let recovered = wait_for_replacement(
            &service,
            &deployment.application_deployment_id,
            &resources,
            &deployment.application_deployment_id,
            "published",
            &stopped_container,
        )
        .await?;
        let recovered_endpoint = endpoint_url(&recovered)?;
        if recovered_endpoint != resources.docker_endpoint_for_unit("published")? {
            return Err("recovered endpoint does not match current Docker inspection".to_owned());
        }
        wait_for_http_body(&recovered_endpoint, "private-dns-ok").await?;

        ApplicationServiceContract::remove_application_deployment(
            &service,
            Request::new(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
            }),
        )
        .await
        .map_err(|error| format!("remove synthetic application: {error}"))?;
        resources
            .wait_for_no_docker_resources(&service, &deployment.application_deployment_id)
            .await?;
        drop(service);
        Ok(())
    }
    .await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

// Docker Desktop's VM does not route group bridge addresses, which the READY launch gate probes
// from the host.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_nvidia_group_units_receive_the_target_device_request() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    let result: Result<(), String> = async {
        resources.write_synthetic_application()?;
        resources.declare_gpu_device_on_application_target(NVIDIA_CDI_SELECTOR)?;
        let service = resources
            .composition_services()
            .await
            .map_err(|error| format!("start synthetic apps-only site: {error}"))?;
        let deployment = ApplicationServiceContract::create_application_deployment(
            &service,
            Request::new(CreateApplicationDeploymentRequest {
                application_id: "synthetic-two-service".to_owned(),
                placement: Some(support::exact_placement(
                    "docker-apps",
                    "docker-apps-target",
                )),
                inference_route_id: None,
                expected_inference_route_generation: 0,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            }),
        )
        .await
        .map_err(|error| format!("create synthetic application: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "create synthetic application returned no deployment".to_owned())?;
        service.start_observer();
        wait_for_endpoint(&service, &deployment.application_deployment_id).await?;
        resources.assert_group_units(&["dependency", "published"])?;
        for unit in ["dependency", "published"] {
            resources.assert_group_unit_device_request(unit, NVIDIA_CDI_SELECTOR)?;
        }

        ApplicationServiceContract::remove_application_deployment(
            &service,
            Request::new(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
            }),
        )
        .await
        .map_err(|error| format!("remove synthetic application: {error}"))?;
        resources
            .wait_for_no_docker_resources(&service, &deployment.application_deployment_id)
            .await?;
        drop(service);
        Ok(())
    }
    .await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

// Docker Desktop's VM does not route group bridge addresses, which the READY launch gate probes
// from the host.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_nvidia_group_units_report_attributed_accelerator_memory() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    let result: Result<(), String> = async {
        ensure_pinned_image(PYTHON_IMAGE, &resources.platform).await?;
        let gpu_uuid = command(
            "nvidia-smi",
            ["--query-gpu=uuid", "--format=csv,noheader", "--id=0"],
        )?
        .trim()
        .to_owned();
        resources.write_gpu_memory_application()?;
        resources.declare_gpu_device_on_application_target(NVIDIA_CDI_SELECTOR)?;
        let service = resources
            .composition_services()
            .await
            .map_err(|error| format!("start GPU memory apps-only site: {error}"))?;
        let deployment = ApplicationServiceContract::create_application_deployment(
            &service,
            Request::new(CreateApplicationDeploymentRequest {
                application_id: "synthetic-gpu-memory".to_owned(),
                placement: Some(support::exact_placement(
                    "docker-apps",
                    "docker-apps-target",
                )),
                inference_route_id: None,
                expected_inference_route_generation: 0,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            }),
        )
        .await
        .map_err(|error| format!("create GPU memory application: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "create GPU memory application returned no deployment".to_owned())?;
        service.start_observer();
        wait_for_endpoint(&service, &deployment.application_deployment_id).await?;

        // The node reports memory on its status cadence, and its first readings may predate the
        // allocation, so wait for the allocation to show on GPU 0.
        let deadline = Instant::now() + Duration::from_secs(120);
        let device = loop {
            let read = ApplicationServiceContract::get_application_deployment(
                &service,
                Request::new(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                }),
            )
            .await
            .map_err(|error| format!("read GPU memory application: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "read GPU memory application returned no deployment".to_owned())?;
            let memory = read
                .service_accelerator_memory
                .iter()
                .find(|unit| unit.service_name == "gpu")
                .and_then(|unit| unit.accelerator_memory.as_ref())
                .and_then(|memory| memory.result.as_ref());
            if let Some(accelerator_memory_observation::Result::Available(usage)) = memory {
                let [device] = usage.devices.as_slice() else {
                    return Err(format!(
                        "unit gpu memory spans devices: {:?}",
                        usage.devices
                    ));
                };
                if device.device_id != gpu_uuid {
                    return Err(format!(
                        "unit gpu memory is attributed to {}, not GPU 0 ({gpu_uuid})",
                        device.device_id
                    ));
                }
                if device.used_memory_bytes >= GPU_FIXTURE_ALLOCATION_BYTES {
                    break device.clone();
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "unit gpu never read its {GPU_FIXTURE_ALLOCATION_BYTES}-byte allocation: {:?}",
                    read.service_accelerator_memory
                ));
            }
            sleep(Duration::from_secs(2)).await;
        };
        eprintln!(
            "[docker-apps] operation=attributed_memory unit=gpu device={} used_memory_bytes={}",
            device.device_id, device.used_memory_bytes
        );

        ApplicationServiceContract::remove_application_deployment(
            &service,
            Request::new(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
            }),
        )
        .await
        .map_err(|error| format!("remove GPU memory application: {error}"))?;
        resources
            .wait_for_no_docker_resources(&service, &deployment.application_deployment_id)
            .await?;
        drop(service);
        Ok(())
    }
    .await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_group_units_report_growing_cpu_time_and_memory_until_stopped() {
    let resources = TestResources::new()
        .await
        .expect("Docker preflight and temporary site");
    let port = dotcore_domain::test_support::ports::reserve_port();
    let result: Result<(), String> = async {
        let runtime = DockerRuntimeFactory::new(
            resources.site_id.clone(),
            resources.root.clone(),
            resources.root.join("resources-data"),
            resources.root.join("resources-cache"),
        )
        .application_backend(
            DockerAcceleratorTransport::Cdi,
            RESOURCES_TARGET_ID.to_owned(),
        )
        .await
        .map_err(|error| format!("connect the Docker application runtime: {error}"))?;
        let group = busy_unit_group(&resources, port.port())?;
        runtime
            .create_workload_group(&group)
            .await
            .map_err(|error| format!("create the busy group: {error:#}"))?;

        let (first_cpu, first_memory) = busy_unit_resources(&*runtime, &group)
            .await?
            .ok_or_else(|| "the running busy unit reported no figures".to_owned())?;
        if first_memory == 0 {
            return Err("the running busy unit reported no memory".to_owned());
        }
        // The unit spins one CPU, so its CPU time grows between two readings.
        let deadline = Instant::now() + Duration::from_secs(30);
        let (grown_cpu, grown_memory) = loop {
            let reading = busy_unit_resources(&*runtime, &group)
                .await?
                .ok_or_else(|| "the running busy unit stopped reporting figures".to_owned())?;
            if reading.0 > first_cpu {
                break reading;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the busy unit's CPU time never grew past {first_cpu} ns: last {reading:?}"
                ));
            }
            sleep(Duration::from_millis(200)).await;
        };
        if grown_memory == 0 {
            return Err("the running busy unit reported no memory".to_owned());
        }
        eprintln!(
            "[docker-apps] operation=group_resources first_cpu_ns={first_cpu} grown_cpu_ns={grown_cpu} memory_bytes={grown_memory}"
        );

        runtime
            .stop_workload_group(&group)
            .await
            .map_err(|error| format!("stop the busy group: {error:#}"))?;
        if let Some(reading) = busy_unit_resources(&*runtime, &group).await? {
            return Err(format!("the stopped busy unit reported figures: {reading:?}"));
        }
        runtime
            .remove_workload_group(&group)
            .await
            .map_err(|error| format!("remove the busy group: {error:#}"))
    }
    .await;
    drop(port);
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_lightweight_application_logs_are_owned_filtered_and_cancellable() {
    let resources = TestResources::new_log_fixture()
        .await
        .expect("Docker preflight and temporary site");
    let result = async {
        resources.write_log_application()?;
        let service = resources.composition_services().await?;
        let deployment = ApplicationServiceContract::create_application_deployment(
            &service,
            Request::new(CreateApplicationDeploymentRequest {
                application_id: "log-fixture".to_owned(),
                placement: Some(support::exact_placement("docker-apps", "docker-apps-target")),
                inference_route_id: None,
                expected_inference_route_generation: 0,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            }),
        )
        .await
        .map_err(|error| format!("create log fixture application: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "create log fixture application returned no deployment".to_owned())?;
        service.start_observer();
        wait_for_endpoint(&service, &deployment.application_deployment_id).await?;
        resources.assert_group_units(&["alpha", "beta", "gamma", "delta", "epsilon"])?;
        let foreign = command_owned(
            "docker",
            &[
                "run".to_owned(),
                "--detach".to_owned(),
                "--pull=never".to_owned(),
                "--label".to_owned(),
                format!("com.dotcore.site={}", resources.site_id),
                BUSYBOX_IMAGE.to_owned(),
                "sh".to_owned(),
                "-ec".to_owned(),
                "trap 'exit 0' TERM; printf 'foreign-log-fixture\\n'; sleep 120 & wait $!".to_owned(),
            ],
        )?;
        if foreign.trim().is_empty() {
            return Err("start foreign Docker output fixture".to_owned());
        }

        let history = ApplicationServiceContract::read_deployment_logs(
            &service,
            Request::new(ReadDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["alpha".to_owned(), "beta".to_owned()],
                page_size: 1,
                before_timestamp: None,
            }),
        )
        .await
        .map_err(|error| format!("read owned log fixture history: {error}"))?
        .into_inner();
        if history.entries.len() != 1 {
            return Err(format!(
                "expected aggregate log limit of one entry, got {:?}",
                history.entries
            ));
        }
        let entry = &history.entries[0];
        let expected_entry = match entry.source_name.as_str() {
            "alpha" => entry.content == b"alpha stdout" || entry.content == b"alpha stderr",
            "beta" => entry.content == b"beta stdout" || entry.content == b"beta stderr",
            _ => false,
        };
        if !expected_entry || entry.timestamp.is_none() {
            return Err(format!(
                "unexpected filtered aggregate log entry: {entry:?}"
            ));
        }
        if history.entries.iter().any(|entry| {
            entry
                .content
                .windows(b"protected-value".len())
                .any(|chunk| chunk == b"protected-value")
        }) {
            return Err("owner service exposed protected log value".to_owned());
        }
        let source_names = history
            .sources
            .iter()
            .map(|source| source.source_name.as_str())
            .collect::<BTreeSet<_>>();
        if source_names != BTreeSet::from(["alpha", "beta"]) {
            return Err(format!(
                "history included unexpected sources: {source_names:?}"
            ));
        }

        // Docker records each stream separately, so the stdout and stderr lines written before
        // readiness may reach its history after the endpoint appears, in either order.
        let complete_alpha = |alpha: &ReadDeploymentLogsResponse| {
            alpha.entries.len() == 3
                && alpha.entries.iter().all(|entry| entry.source_name == "alpha")
                && [&b"alpha stdout"[..], b"alpha stderr"].iter().all(|content| {
                    alpha.entries.iter().any(|entry| entry.content == *content)
                })
        };
        let alpha = read_logs_until(
            &service,
            ReadDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["alpha".to_owned()],
                page_size: 3,
                before_timestamp: None,
            },
            complete_alpha,
        )
        .await
        .map_err(|error| format!("read source-filtered log fixture history: {error}"))?;
        if !complete_alpha(&alpha) {
            return Err(format!(
                "source filter did not preserve alpha stdout/stderr: {:?}",
                alpha.entries
            ));
        }

        // The large outputs are recorded progressively; wait until they exceed the response bound.
        let bounded = read_logs_until(
            &service,
            ReadDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["gamma".to_owned(), "delta".to_owned(), "epsilon".to_owned()],
                page_size: 200,
                before_timestamp: None,
            },
            |bounded| bounded.encoded_len() >= 4_000_000,
        )
        .await
        .map_err(|error| format!("read bounded log fixture history: {error}"))?;
        if bounded.entries.is_empty()
            || !(4_000_000..=4 * 1024 * 1024).contains(&bounded.encoded_len())
        {
            return Err(format!(
                "large history did not remain within the 4 MiB response bound: truncated={}, entries={}, encoded_bytes={}",
                bounded.response_truncated,
                bounded.entries.len(),
                bounded.encoded_len(),
            ));
        }

        let foreign_source = ApplicationServiceContract::follow_deployment_logs(
            &service,
            Request::new(FollowDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["foreign".to_owned()],
                resume_positions: Vec::new(),
            }),
        )
        .await;
        let foreign_source = match foreign_source {
            Ok(_) => return Err("foreign public source was accepted".to_owned()),
            Err(error) => error,
        };
        if foreign_source.code() != Code::InvalidArgument {
            return Err(format!(
                "foreign public source returned {:?}, expected INVALID_ARGUMENT",
                foreign_source.code()
            ));
        }

        let replacement_source = ApplicationServiceContract::follow_deployment_logs(
            &service,
            Request::new(FollowDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["beta".to_owned()],
                resume_positions: vec![
                    dotcore_proto::dotcore::application::v1::DeploymentLogResumePosition {
                        source_name: "replacement".to_owned(),
                        latest_timestamp: None,
                    },
                ],
            }),
        )
        .await;
        let replacement_source = match replacement_source {
            Ok(_) => return Err("replacement resume source was accepted".to_owned()),
            Err(error) => error,
        };
        if replacement_source.code() != Code::InvalidArgument {
            return Err(format!(
                "replacement resume source returned {:?}, expected INVALID_ARGUMENT",
                replacement_source.code()
            ));
        }

        let mut follow = ApplicationServiceContract::follow_deployment_logs(
            &service,
            Request::new(FollowDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["beta".to_owned()],
                resume_positions: Vec::new(),
            }),
        )
        .await
        .map_err(|error| format!("follow owned log fixture: {error}"))?
        .into_inner();
        let event = timeout(Duration::from_secs(10), follow.next())
            .await
            .map_err(|_| "timed out waiting for beta follow event".to_owned())?
            .ok_or_else(|| "beta follow stream ended before its first event".to_owned())?
            .map_err(|error| format!("beta follow stream failed: {error}"))?;
        let event = format!("{event:?}");
        if !event.contains("beta") || event.contains("foreign-log-fixture") {
            return Err(format!(
                "follow exposed wrong source or foreign output: {event}"
            ));
        }
        drop(follow);

        let mut admitted_follows = Vec::new();
        for _ in 0..8 {
            admitted_follows.push(
                ApplicationServiceContract::follow_deployment_logs(
                    &service,
                    Request::new(FollowDeploymentLogsRequest {
                        application_deployment_id: deployment.application_deployment_id.clone(),
                        source_names: vec!["beta".to_owned()],
                        resume_positions: Vec::new(),
                    }),
                )
                .await
                .map_err(|error| format!("admit follow stream: {error}"))?
                .into_inner(),
            );
        }
        let exhausted = ApplicationServiceContract::follow_deployment_logs(
            &service,
            Request::new(FollowDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["beta".to_owned()],
                resume_positions: Vec::new(),
            }),
        )
        .await;
        let exhausted = match exhausted {
            Ok(_) => return Err("ninth follow stream was admitted".to_owned()),
            Err(error) => error,
        };
        if exhausted.code() != Code::ResourceExhausted {
            return Err(format!(
                "ninth follow stream returned {:?}, expected RESOURCE_EXHAUSTED",
                exhausted.code()
            ));
        }
        drop(admitted_follows);
        let cleanup_follow = follow_once_admitted(
            &service,
            FollowDeploymentLogsRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
                source_names: vec!["beta".to_owned()],
                resume_positions: Vec::new(),
            },
        )
        .await
        .map_err(|error| {
            format!("dropped follow streams did not propagate cancellation and release admission: {error}")
        })?;
        drop(cleanup_follow);

        assert_follow_resumes_exactly_once(
            &service,
            &deployment.application_deployment_id,
            &resources.running_container_id_for_unit("beta")?,
        )
        .await?;

        command("docker", ["rm", "-f", foreign.trim()])?;

        ApplicationServiceContract::remove_application_deployment(
            &service,
            Request::new(RemoveApplicationDeploymentRequest {
                application_deployment_id: deployment.application_deployment_id.clone(),
            }),
        )
        .await
        .map_err(|error| format!("remove log fixture application: {error}"))?;
        resources
            .wait_for_no_docker_resources(&service, &deployment.application_deployment_id)
            .await?;
        Ok(())
    }
    .await;
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_secret_parameter_reaches_only_the_container_and_site_yaml() {
    // Debug-level records reach the site's retained logs, which the sentinel search covers.
    dotcore::logging::initialize_with_default_filter(
        "info,dotcore=debug,dotcore_runtime=debug,dotcore_runtime_backends=debug,dotcore_site=debug",
    );
    let resources = TestResources::new_log_fixture()
        .await
        .expect("Docker preflight and temporary site");
    resources
        .write_secret_application()
        .expect("secret application catalog declaration");
    let backend = support::RunningBackend::start_at_with_simulated_runtime_test_support(
        resources.configuration.clone(),
        resources.publication_host.into(),
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
    )
    .await;
    let result = async {
        let mut applications = backend.application_client();
        let serving =
            create_when_target_observed(&mut applications, secret_deployment_request("serve"))
                .await?;
        let serving_id = serving.application_deployment_id.clone();
        let failing =
            create_when_target_observed(&mut applications, secret_deployment_request("exit"))
                .await?;
        let failing_id = failing.application_deployment_id.clone();

        wait_for_endpoint_client(&mut applications, &serving_id).await?;
        let container = resources.running_container_id_for_group(&serving_id, "web")?;
        let environment = container_environment(&container)?;
        if !environment.contains(&format!("PASSWORD={SECRET_SENTINEL}"))
            || !environment.contains(&"USERNAME=admin".to_owned())
        {
            return Err(format!(
                "the container environment lacks the secret or the username: {} entries",
                environment.len()
            ));
        }
        // The serving unit prints the username and the secret; deployment logs redact every
        // launch value.
        let serving_logs = read_logs_containing(
            &mut applications,
            &serving_id,
            "login [REDACTED] [REDACTED]",
        )
        .await?;
        assert_no_sentinel("serving deployment logs", &serving_logs)?;

        // The failing unit prints the secret, then exits before it ever serves.
        let failing_logs =
            read_logs_containing(&mut applications, &failing_id, "stopping with [REDACTED]")
                .await?;
        assert_no_sentinel("failing deployment logs", &failing_logs)?;
        // An exiting unit leaves the deployment unavailable without a recorded failure.
        let exited = applications
            .get_application_deployment(GetApplicationDeploymentRequest {
                application_deployment_id: failing_id.clone(),
            })
            .await
            .map_err(|error| format!("read failing deployment: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "failing deployment read returned no deployment".to_owned())?;
        let state = exited
            .state
            .as_ref()
            .ok_or_else(|| "failing deployment has no state".to_owned())?;
        if state.lifecycle_state == ApplicationLifecycleState::Ready as i32 {
            return Err("the failing deployment became ready".to_owned());
        }
        assert_no_sentinel("failing deployment state", &format!("{state:?}"))?;
        Ok::<_, String>((serving_id, failing_id, container))
    }
    .await;
    backend.stop().await;
    if result.is_ok() {
        wait_for_backend_service_release(&resources.configuration)
            .await
            .expect("release backend repositories before the restart");
    }
    let result = match result {
        Ok((serving_id, failing_id, container)) => {
            let restarted = support::RunningBackend::start_at_with_simulated_runtime_test_support(
                resources.configuration.clone(),
                resources.publication_host.into(),
                dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0),
            )
            .await;
            let restart_result = async {
                let mut applications = restarted.application_client();
                let retained = wait_for_endpoint_client(&mut applications, &serving_id).await?;
                let password = retained
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .parameter_values
                            .iter()
                            .find(|value| value.parameter_id == "password")
                    })
                    .and_then(|value| value.value.clone());
                if password
                    != Some(
                        dotcore_proto::dotcore::catalog::v1::parameter_value::Value::String(
                            SECRET_SENTINEL.to_owned(),
                        ),
                    )
                {
                    return Err("the restart changed the retained secret".to_owned());
                }
                let restarted_container =
                    resources.running_container_id_for_group(&serving_id, "web")?;
                if restarted_container != container {
                    return Err(format!(
                        "the restart replaced the container {container} with {restarted_container}"
                    ));
                }
                // A launch refused on a taken published port records failure text and its cause.
                let port = endpoint_port(&endpoint_url(&retained)?)?;
                applications
                    .pause_deployment(PauseDeploymentRequest {
                        application_deployment_id: serving_id.clone(),
                    })
                    .await
                    .map_err(|error| format!("pause secret deployment: {error}"))?;
                wait_for_application_lifecycle_client(
                    &mut applications,
                    &serving_id,
                    ApplicationLifecycleState::Paused,
                )
                .await?;
                let holder = std::net::TcpListener::bind((resources.publication_host, port))
                    .map_err(|error| format!("hold published port {port}: {error}"))?;
                applications
                    .resume_deployment(ResumeDeploymentRequest {
                        application_deployment_id: serving_id.clone(),
                    })
                    .await
                    .map_err(|error| format!("resume secret deployment: {error}"))?;
                let refused =
                    wait_for_failure_code(&mut applications, &serving_id, "PublishedPortInUse")
                        .await?;
                drop(holder);
                let failure = refused
                    .state
                    .and_then(|state| state.failure)
                    .ok_or_else(|| "the refused launch recorded no failure".to_owned())?;
                if failure.message.is_empty() {
                    return Err("the refused launch recorded no failure text".to_owned());
                }
                assert_no_sentinel("refused launch failure", &format!("{failure:?}"))?;

                // Every retained site file but the declarations, durable debug logs included,
                // lacks the secret; the declarations keep it, which proves the search reads them.
                let debug_records = support::retained_json_records(&resources.root)
                    .iter()
                    .filter(|record| record["level"] == "DEBUG")
                    .count();
                if debug_records == 0 {
                    return Err("the site logs retained no debug records".to_owned());
                }
                let hits = files_containing(&resources.root, SECRET_SENTINEL.as_bytes())?;
                if hits != [resources.root.join("site.yaml")] {
                    return Err(format!(
                        "the secret sentinel is retained in {hits:?}, not only site.yaml"
                    ));
                }
                for deployment_id in [&serving_id, &failing_id] {
                    applications
                        .remove_application_deployment(RemoveApplicationDeploymentRequest {
                            application_deployment_id: deployment_id.clone(),
                        })
                        .await
                        .map_err(|error| format!("remove secret deployment: {error}"))?;
                }
                // The site-wide Docker check passes only once both groups are gone.
                for deployment_id in [&serving_id, &failing_id] {
                    wait_for_no_docker_resources_client(
                        &resources,
                        &mut applications,
                        deployment_id,
                    )
                    .await?;
                }
                Ok(())
            }
            .await;
            restarted.stop().await;
            restart_result
        }
        Err(error) => Err(error),
    };
    let cleanup = resources.cleanup();
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(primary), Ok(())) => panic!("{primary}"),
        (Ok(()), Err(cleanup)) => panic!("cleanup: {cleanup}"),
        (Err(primary), Err(cleanup)) => panic!("{primary}; cleanup: {cleanup}"),
    }
}

/// Configuration values the lifecycle application's container must render.
struct ConfigurationExpectation {
    inference_base_url: String,
    route_name: String,
}

fn lifecycle_deployment_request(
    route: Option<&dotcore_proto::dotcore::inference::v1::InferenceRoute>,
) -> CreateApplicationDeploymentRequest {
    CreateApplicationDeploymentRequest {
        application_id: LIFECYCLE_APPLICATION_ID.to_owned(),
        placement: Some(support::exact_placement(
            "docker-apps",
            "docker-apps-target",
        )),
        inference_route_id: route.map(|route| route.inference_route_id.clone()),
        expected_inference_route_generation: route.map_or(0, |route| route.generation),
        application_deployment_name: None,
        parameter_values: Vec::new(),
    }
}

fn storage_name(deployment: &ApplicationDeployment) -> Result<&str, String> {
    deployment
        .storage_attachments
        .first()
        .map(|attachment| attachment.storage_name.as_str())
        .ok_or_else(|| "Application deployment has no storage attachment".to_owned())
}

async fn create_when_target_observed(
    client: &mut ApplicationClient,
    request: CreateApplicationDeploymentRequest,
) -> Result<ApplicationDeployment, String> {
    timeout(TARGET_OBSERVATION_TIMEOUT, async {
        loop {
            match client.create_application_deployment(request.clone()).await {
                Ok(response) => {
                    return response
                        .into_inner()
                        .deployment
                        .ok_or_else(|| "create application returned no deployment".to_owned());
                }
                // The apps controller rejects placement until the execution target reports
                // its first platform observation.
                Err(status)
                    if status.code() == Code::FailedPrecondition
                        && (status.message().contains("has no platform observation")
                            || status
                                .message()
                                .contains("platform observation is unavailable")
                            || status.message().contains("platform observation is stale")) =>
                {
                    sleep(Duration::from_millis(250)).await;
                }
                Err(status) => return Err(format!("create application: {status}")),
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for the application target platform observation".to_owned())?
}

/// Requests a secret fixture deployment with the typed sentinel password and `outcome`.
fn secret_deployment_request(outcome: &str) -> CreateApplicationDeploymentRequest {
    let text = |id: &str, value: &str| dotcore_proto::dotcore::catalog::v1::ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(
            dotcore_proto::dotcore::catalog::v1::parameter_value::Value::String(value.to_owned()),
        ),
    };
    CreateApplicationDeploymentRequest {
        application_id: SECRET_APPLICATION_ID.to_owned(),
        placement: Some(support::exact_placement(
            "docker-apps",
            "docker-apps-target",
        )),
        inference_route_id: None,
        expected_inference_route_generation: 0,
        application_deployment_name: None,
        parameter_values: vec![text("password", SECRET_SENTINEL), text("outcome", outcome)],
    }
}

async fn wait_for_endpoint_client(
    client: &mut ApplicationClient,
    deployment_id: &str,
) -> Result<ApplicationDeployment, String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let deployment = client
            .get_application_deployment(GetApplicationDeploymentRequest {
                application_deployment_id: deployment_id.to_owned(),
            })
            .await
            .map_err(|error| format!("read application deployment: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "Application read returned no deployment".to_owned())?;
        if deployment
            .state
            .as_ref()
            .and_then(|state| state.endpoint.as_ref())
            .is_some()
        {
            return Ok(deployment);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for an application endpoint; last state {:?}",
                deployment.state
            ));
        }
        sleep(Duration::from_millis(250)).await;
    }
}

/// Reads a deployment's log history until one entry contains `expected`, and returns every entry
/// of that read joined by line breaks.
async fn read_logs_containing(
    client: &mut ApplicationClient,
    deployment_id: &str,
    expected: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + LOG_HISTORY_TIMEOUT;
    loop {
        let history = client
            .read_deployment_logs(ReadDeploymentLogsRequest {
                application_deployment_id: deployment_id.to_owned(),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("read deployment logs: {error}"))?
            .into_inner();
        let text = history
            .entries
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        if text.lines().any(|line| line.contains(expected)) {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for log line {expected:?}; last read {:?}",
                text.replace(SECRET_SENTINEL, "<sentinel>")
            ));
        }
        sleep(Duration::from_millis(250)).await;
    }
}

fn assert_no_sentinel(surface: &str, text: &str) -> Result<(), String> {
    if text.contains(SECRET_SENTINEL) {
        Err(format!("the secret sentinel appears in the {surface}"))
    } else {
        Ok(())
    }
}

/// Returns the environment of one container as `NAME=value` entries.
fn container_environment(container: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(&command(
        "docker",
        ["inspect", "--format", "{{json .Config.Env}}", container],
    )?)
    .map_err(|error| format!("decode container environment: {error}"))
}

/// Returns every file under `root` whose bytes contain `needle`.
fn files_containing(root: &Path, needle: &[u8]) -> Result<Vec<PathBuf>, String> {
    let mut hits = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("list {}: {error}", directory.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|error| format!("list {}: {error}", directory.display()))?
                .path();
            let file_type = fs::symlink_metadata(&path)
                .map_err(|error| format!("inspect {}: {error}", path.display()))?
                .file_type();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                let bytes =
                    fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
                if bytes.windows(needle.len()).any(|window| window == needle) {
                    hits.push(path);
                }
            }
        }
    }
    Ok(hits)
}

async fn wait_for_lifecycle_runtime(
    resources: &TestResources,
    client: &mut ApplicationClient,
    deployment_id: &str,
    data: &Path,
    expected: &ConfigurationExpectation,
) -> Result<ApplicationDeployment, String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let deployment = client
            .get_application_deployment(GetApplicationDeploymentRequest {
                application_deployment_id: deployment_id.to_owned(),
            })
            .await
            .map_err(|error| format!("read lifecycle application deployment: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "lifecycle application read returned no deployment".to_owned())?;
        let observed = endpoint_url(&deployment).and_then(|endpoint| {
            resources.assert_lifecycle_runtime(deployment_id, &endpoint, data, expected)
        });
        match observed {
            Ok(()) => return Ok(deployment),
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "timed out waiting for the lifecycle application runtime: {error}"
                ));
            }
            Err(_) => sleep(Duration::from_millis(250)).await,
        }
    }
}

async fn wait_for_application_stage<C>(
    service: &C,
    deployment_id: &str,
    stage: &str,
) -> Result<ApplicationDeployment, String>
where
    C: ApplicationServiceContract,
{
    timeout(READY_TIMEOUT, async {
        loop {
            let deployment = ApplicationServiceContract::get_application_deployment(
                service,
                Request::new(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                }),
            )
            .await
            .map_err(|error| format!("read application deployment: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "Application read returned no deployment".to_owned())?;
            if deployment
                .state
                .as_ref()
                .is_some_and(|state| state.stage == stage)
            {
                return Ok(deployment);
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for application stage {stage:?}"))?
}

async fn wait_for_endpoint<C>(
    service: &C,
    deployment_id: &str,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, String>
where
    C: ApplicationServiceContract,
{
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let deployment = ApplicationServiceContract::get_application_deployment(
            service,
            Request::new(GetApplicationDeploymentRequest {
                application_deployment_id: deployment_id.to_owned(),
            }),
        )
        .await
        .map_err(|error| format!("read application deployment: {error}"))?
        .into_inner()
        .deployment
        .ok_or_else(|| "Application read returned no deployment".to_owned())?;
        if deployment
            .state
            .as_ref()
            .and_then(|state| state.endpoint.as_ref())
            .is_some()
        {
            return Ok(deployment);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for an application endpoint; last state {:?}",
                deployment.state
            ));
        }
        sleep(Duration::from_millis(250)).await;
    }
}

/// Repeats one log history read until `accepted` holds or the log history deadline passes, and
/// returns the last read either way so the caller's exact checks report what was observed.
async fn read_logs_until<C>(
    service: &C,
    request: ReadDeploymentLogsRequest,
    accepted: impl Fn(&ReadDeploymentLogsResponse) -> bool,
) -> Result<ReadDeploymentLogsResponse, tonic::Status>
where
    C: ApplicationServiceContract,
{
    let deadline = Instant::now() + LOG_HISTORY_TIMEOUT;
    loop {
        let history = ApplicationServiceContract::read_deployment_logs(
            service,
            Request::new(request.clone()),
        )
        .await?
        .into_inner();
        if accepted(&history) || Instant::now() >= deadline {
            return Ok(history);
        }
        sleep(Duration::from_millis(250)).await;
    }
}

/// Opens a follow once the follows dropped before it released their admission, and returns the
/// last refusal if they did not within the admission release deadline.
///
/// A dropped follow releases its node's admission only once its cancellation reaches that node,
/// shortly after the drop, so a follow opened at once may still find the node at capacity.
async fn follow_once_admitted<C>(
    service: &C,
    request: FollowDeploymentLogsRequest,
) -> Result<C::FollowDeploymentLogsStream, tonic::Status>
where
    C: ApplicationServiceContract,
{
    let deadline = Instant::now() + ADMISSION_RELEASE_TIMEOUT;
    loop {
        match ApplicationServiceContract::follow_deployment_logs(
            service,
            Request::new(request.clone()),
        )
        .await
        {
            Ok(follow) => return Ok(follow.into_inner()),
            Err(status)
                if matches!(status.code(), Code::Unavailable | Code::ResourceExhausted)
                    && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(50)).await;
            }
            Err(status) => return Err(status),
        }
    }
}

/// Writes lines into a running container's output while no follow is attached, then checks that a
/// follow resumed at an earlier line delivers exactly the later lines once, including lines that
/// share the resume line's second, and then the live line written afterwards.
async fn assert_follow_resumes_exactly_once<C>(
    service: &C,
    deployment_id: &str,
    container_id: &str,
) -> Result<(), String>
where
    C: ApplicationServiceContract,
{
    // Start both writes right after a second boundary so the resume line and the burst share it.
    let into_second = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read clock: {error}"))?
        .subsec_millis();
    sleep(Duration::from_millis(u64::from(1_020 - into_second))).await;
    for line in ["resume-mark", "resume-1\nresume-2\nresume-3"] {
        command(
            "docker",
            [
                "exec",
                container_id,
                "sh",
                "-c",
                &format!("printf '{line}\\n' > /proc/1/fd/1"),
            ],
        )?;
    }

    let is_resume_line = |content: &[u8]| content.starts_with(b"resume-");
    let wanted = |history: &ReadDeploymentLogsResponse| {
        history
            .entries
            .iter()
            .filter(|entry| is_resume_line(&entry.content))
            .count()
            == 4
    };
    let history = read_logs_until(
        service,
        ReadDeploymentLogsRequest {
            application_deployment_id: deployment_id.to_owned(),
            source_names: vec!["beta".to_owned()],
            page_size: 20,
            before_timestamp: None,
        },
        wanted,
    )
    .await
    .map_err(|error| format!("read resume lines: {error}"))?;
    let mut written = history
        .entries
        .iter()
        .filter(|entry| is_resume_line(&entry.content))
        .collect::<Vec<_>>();
    written.sort_by_key(|entry| entry.timestamp.as_ref().map(|at| (at.seconds, at.nanos)));
    let contents = written
        .iter()
        .map(|entry| String::from_utf8_lossy(&entry.content).into_owned())
        .collect::<Vec<_>>();
    if contents != ["resume-mark", "resume-1", "resume-2", "resume-3"] {
        return Err(format!("unexpected written resume lines: {contents:?}"));
    }
    let mark = written[0].timestamp;
    let mark_second = mark.as_ref().map(|at| at.seconds);
    if written
        .iter()
        .any(|entry| entry.timestamp.as_ref().map(|at| at.seconds) != mark_second)
    {
        return Err(format!(
            "resume lines did not share one second: {:?}",
            written
                .iter()
                .map(|entry| &entry.timestamp)
                .collect::<Vec<_>>()
        ));
    }

    let follow = ApplicationServiceContract::follow_deployment_logs(
        service,
        Request::new(FollowDeploymentLogsRequest {
            application_deployment_id: deployment_id.to_owned(),
            source_names: vec!["beta".to_owned()],
            resume_positions: vec![
                dotcore_proto::dotcore::application::v1::DeploymentLogResumePosition {
                    source_name: "beta".to_owned(),
                    latest_timestamp: mark,
                },
            ],
        }),
    )
    .await
    .map_err(|error| format!("follow from resume position: {error}"))?
    .into_inner();
    let mut follow = std::pin::pin!(follow);
    command(
        "docker",
        [
            "exec",
            container_id,
            "sh",
            "-c",
            "printf 'resume-live\\n' > /proc/1/fd/1",
        ],
    )?;
    let mut delivered = Vec::new();
    while delivered.len() < 4 {
        let event = timeout(Duration::from_secs(10), follow.next())
            .await
            .map_err(|_| format!("timed out resuming; delivered so far: {delivered:?}"))?
            .ok_or_else(|| format!("resumed follow ended early; delivered: {delivered:?}"))?
            .map_err(|error| format!("resumed follow failed: {error}"))?;
        if let Some(
            dotcore_proto::dotcore::application::v1::follow_deployment_logs_response::Event::Entry(
                entry,
            ),
        ) = event.event
        {
            delivered.push(String::from_utf8_lossy(&entry.content).into_owned());
        }
    }
    if delivered != ["resume-1", "resume-2", "resume-3", "resume-live"] {
        return Err(format!("resumed follow delivered {delivered:?}"));
    }
    // Nothing further (such as a repeat of the earlier lines) follows the live line.
    if let Ok(Some(extra)) = timeout(Duration::from_millis(500), follow.next()).await {
        let extra = extra.map_err(|error| format!("resumed follow failed: {error}"))?;
        if let Some(
            dotcore_proto::dotcore::application::v1::follow_deployment_logs_response::Event::Entry(
                entry,
            ),
        ) = extra.event
        {
            return Err(format!("unexpected repeated line: {:?}", entry.content));
        }
    }
    Ok(())
}

async fn wait_for_no_docker_resources_client(
    resources: &TestResources,
    client: &mut ApplicationClient,
    deployment_id: &str,
) -> Result<(), String> {
    timeout(CLEANUP_TIMEOUT, async {
        loop {
            if resources.assert_no_docker_resources().is_ok() {
                match client
                    .get_application_deployment(GetApplicationDeploymentRequest {
                        application_deployment_id: deployment_id.to_owned(),
                    })
                    .await
                {
                    Err(status) if status.code() == Code::NotFound => return Ok(()),
                    Ok(_) => {}
                    Err(status) => {
                        return Err(format!(
                            "read desired TEARDOWN application deployment: {status}"
                        ));
                    }
                }
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for application Docker cleanup".to_owned())?
}

async fn wait_for_application_lifecycle_client(
    client: &mut ApplicationClient,
    deployment_id: &str,
    lifecycle: ApplicationLifecycleState,
) -> Result<ApplicationDeployment, String> {
    timeout(READY_TIMEOUT, async {
        loop {
            let deployment = client
                .get_application_deployment(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                })
                .await
                .map_err(|error| format!("read application deployment: {error}"))?
                .into_inner()
                .deployment
                .ok_or_else(|| "Application read returned no deployment".to_owned())?;
            if deployment
                .state
                .as_ref()
                .is_some_and(|state| state.lifecycle_state == lifecycle as i32)
            {
                return Ok(deployment);
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for application lifecycle {lifecycle:?}"))?
}

fn assert_suppressed_application(
    deployment: &ApplicationDeployment,
    lifecycle: ApplicationLifecycleState,
) -> Result<(), String> {
    if deployment.desired_state != ApplicationDeploymentDesiredState::Paused as i32 {
        return Err(format!(
            "suppressed application has unexpected desired state {}",
            deployment.desired_state
        ));
    }
    let state = deployment
        .state
        .as_ref()
        .ok_or_else(|| "suppressed application has no state".to_owned())?;
    if state.lifecycle_state != lifecycle as i32
        || state.readiness_state != ApplicationReadinessState::NotReady as i32
        || state.endpoint.is_some()
        || !state.services.is_empty()
    {
        return Err(format!(
            "suppressed application has lifecycle={}, readiness={}, endpoint={:?}, services={:?}",
            state.lifecycle_state, state.readiness_state, state.endpoint, state.services
        ));
    }
    Ok(())
}

fn assert_running_group(group: &[DockerGroupContainer]) -> Result<(), String> {
    if group.is_empty() {
        return Err("Application workload group has no Docker containers".to_owned());
    }
    for container in group {
        if !container.running || container.pid == 0 || container.status != "running" {
            return Err(format!(
                "Application workload container {} is not actively running: running={}, pid={}, status={}",
                container.id, container.running, container.pid, container.status
            ));
        }
    }
    Ok(())
}

fn assert_stopped_group(group: &[DockerGroupContainer]) -> Result<(), String> {
    if group.is_empty() {
        return Err(
            "Pause removed application workload containers instead of retaining them".to_owned(),
        );
    }
    for container in group {
        if container.running || container.pid != 0 || container.status != "exited" {
            return Err(format!(
                "paused application workload container {} still has active execution: running={}, pid={}, status={}",
                container.id, container.running, container.pid, container.status
            ));
        }
    }
    Ok(())
}

fn assert_retained_group(
    expected: &[DockerGroupContainer],
    actual: &[DockerGroupContainer],
) -> Result<(), String> {
    let expected_retained = expected
        .iter()
        .map(|container| (&container.id, &container.retained))
        .collect::<Vec<_>>();
    let actual_retained = actual
        .iter()
        .map(|container| (&container.id, &container.retained))
        .collect::<Vec<_>>();
    if expected_retained != actual_retained {
        return Err(format!(
            "Application workload group identity or retained Docker resources changed: expected {expected_retained:?}, got {actual_retained:?}"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn wait_for_replacement<C>(
    service: &C,
    deployment_id: &str,
    resources: &TestResources,
    group: &str,
    unit: &str,
    stopped_container: &str,
) -> Result<dotcore_proto::dotcore::application::v1::ApplicationDeployment, String>
where
    C: ApplicationServiceContract,
{
    timeout(READY_TIMEOUT, async {
        loop {
            let deployment = ApplicationServiceContract::get_application_deployment(
                service,
                Request::new(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                }),
            )
            .await
            .map_err(|error| format!("read recovered application deployment: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "recovered application read returned no deployment".to_owned())?;
            if let (Ok(replacement), Ok(endpoint), Ok(docker_endpoint)) = (
                resources.running_container_id_for_group(group, unit),
                endpoint_url(&deployment),
                resources.docker_endpoint_for_unit(unit),
            ) && replacement != stopped_container
                && endpoint == docker_endpoint
            {
                return Ok(deployment);
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for synthetic application replacement".to_owned())?
}

async fn wait_for_group_recovery(
    resources: &TestResources,
    group: &str,
    unit: &str,
    previous_container: Option<&str>,
) -> Result<String, String> {
    timeout(READY_TIMEOUT, async {
        loop {
            if let Ok(running) = resources.running_container_id_for_group(group, unit)
                && previous_container.is_none_or(|previous| running != previous)
            {
                return Ok(running);
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {group} {unit} recovery"))?
}

async fn wait_for_group_replacement(
    resources: &TestResources,
    group: &str,
    unit: &str,
    stopped_container: &str,
) -> Result<(), String> {
    wait_for_group_recovery(resources, group, unit, Some(stopped_container))
        .await
        .map(|_| ())
}

async fn wait_for_failure_code(
    client: &mut ApplicationClient,
    deployment_id: &str,
    code: &str,
) -> Result<ApplicationDeployment, String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let deployment = client
            .get_application_deployment(GetApplicationDeploymentRequest {
                application_deployment_id: deployment_id.to_owned(),
            })
            .await
            .map_err(|error| format!("read application deployment: {error}"))?
            .into_inner()
            .deployment
            .ok_or_else(|| "Application read returned no deployment".to_owned())?;
        let failure = deployment
            .state
            .as_ref()
            .and_then(|state| state.failure.as_ref());
        if failure.is_some_and(|failure| failure.code == code) {
            return Ok(deployment);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for failure {code}; last state {:?}",
                deployment.state
            ));
        }
        sleep(Duration::from_millis(250)).await;
    }
}

fn endpoint_port(endpoint: &str) -> Result<u16, String> {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.port())
        .ok_or_else(|| format!("application endpoint {endpoint:?} has no port"))
}

fn endpoint_url(
    deployment: &dotcore_proto::dotcore::application::v1::ApplicationDeployment,
) -> Result<String, String> {
    deployment
        .state
        .as_ref()
        .and_then(|state| state.endpoint.as_ref())
        .map(|endpoint| endpoint.url.clone())
        .ok_or_else(|| "ready application has no controller endpoint".to_owned())
}

async fn wait_for_http_body(endpoint: &str, expected_body: &str) -> Result<(), String> {
    timeout(READY_TIMEOUT, async {
        loop {
            if let Ok(response) = http_get(endpoint).await
                && response.contains(expected_body)
            {
                return Ok(());
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {expected_body:?} at {endpoint}"))?
}

async fn http_get(endpoint: &str) -> Result<String, String> {
    let authority = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("endpoint is not HTTP: {endpoint}"))?;
    let (host, port) = authority
        .split_once(':')
        .ok_or_else(|| format!("endpoint has no IPv4 port: {endpoint}"))?;
    let port = port
        .parse::<u16>()
        .map_err(|error| format!("endpoint port is invalid: {error}"))?;
    let mut stream = timeout(Duration::from_secs(5), TcpStream::connect((host, port)))
        .await
        .map_err(|_| format!("connect to {endpoint} timed out"))?
        .map_err(|error| format!("connect to {endpoint}: {error}"))?;
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .map_err(|error| format!("write HTTP request to {endpoint}: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .map_err(|error| format!("read HTTP response from {endpoint}: {error}"))?;
    Ok(response)
}

async fn wait_for_fixture_route(
    client: &mut dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient<
        tonic::transport::Channel,
    >,
) -> Result<dotcore_proto::dotcore::inference::v1::InferenceRoute, String> {
    timeout(READY_TIMEOUT, async {
        loop {
            if let Ok(response) = client.list_routes(ListRoutesRequest {}).await
                && let Some(route) = response
                    .into_inner()
                    .routes
                    .into_iter()
                    .filter_map(|view| view.route)
                    .find(|route| route.name == "fixture-route")
            {
                return Ok(route);
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for fixture inference route".to_owned())?
}

async fn wait_for_backend_service_release(configuration: &SitePaths) -> Result<(), String> {
    let lock_path = configuration.root.join("state.pb.lock");
    timeout(Duration::from_secs(10), async {
        loop {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|error| format!("open current-state lock probe: {error}"))?;
            match lock.try_lock() {
                Ok(()) => {
                    drop(lock);
                    return Ok(());
                }
                Err(TryLockError::WouldBlock) => sleep(Duration::from_millis(25)).await,
                Err(TryLockError::Error(error)) => {
                    return Err(format!("acquire current-state lock probe: {error}"));
                }
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for backend repositories to release".to_owned())?
}

#[derive(Debug, PartialEq)]
struct DockerGroupContainer {
    id: String,
    running: bool,
    pid: u64,
    status: String,
    retained: DockerRetainedResources,
}

#[derive(Debug, PartialEq)]
struct DockerRetainedResources {
    network_ids: Vec<String>,
    mounts: Value,
    cpu_limit: Value,
    memory_limit: Value,
    gpu_devices: Value,
}

/// A one-unit group whose unit keeps one CPU busy until it is stopped, on the resources test's
/// own execution target.
fn busy_unit_group(
    resources: &TestResources,
    host_port: u16,
) -> Result<WorkloadGroupLaunch, String> {
    let unit = WorkloadUnitName("busy".to_owned());
    Ok(WorkloadGroupLaunch {
        group_id: WorkloadGroupId("busy-resources".to_owned()),
        correlation: WorkloadCorrelation {
            owner_id: "busy-resources".to_owned(),
            reconcile_generation: 1,
        },
        execution_target: dotcore_runtime::ExecutionTarget {
            managed_node_id: dotcore_domain::SiteNodeId::parse("docker-apps")
                .map_err(|error| format!("resources test node: {error}"))?,
            accelerator_device_ids: Vec::new(),
        },
        execution_target_id: RESOURCES_TARGET_ID.to_owned(),
        required_platform: dotcore_domain::ExecutionPlatform::try_from(&resources.platform)
            .map_err(|error| format!("resources test platform: {error}"))?,
        units: std::collections::BTreeMap::from([(
            unit.clone(),
            WorkloadUnitLaunch {
                image: BUSYBOX_IMAGE.to_owned(),
                // A shell PID 1 that traps SIGTERM stops at once while its child spins.
                command: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "trap 'exit 0' TERM; while :; do :; done & wait $!".to_owned(),
                ],
                arguments: Vec::new(),
                environment: Default::default(),
                reconfigurable_environment: Default::default(),
                storage: Vec::new(),
                ports: vec![WorkloadPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                }],
                dependencies: Vec::new(),
                ready_launch_gate: None,
            },
        )]),
        publication: WorkloadPublicationRequest {
            unit_name: unit,
            port_name: "http".to_owned(),
            configured_host: resources.publication_host,
            host_port: std::num::NonZeroU16::new(host_port)
                .ok_or_else(|| "reserved port is zero".to_owned())?,
        },
        host_aliases: Default::default(),
    })
}

/// The busy unit's CPU time and memory, or none when it is not running; any other answer fails.
async fn busy_unit_resources(
    runtime: &dyn WorkloadGroupBackend,
    group: &WorkloadGroupLaunch,
) -> Result<Option<(u64, u64)>, String> {
    let mut observations = runtime
        .observe_group_resources(group)
        .await
        .map_err(|error| format!("observe the busy group's resources: {error:#}"))?;
    let observation = observations
        .remove(&WorkloadUnitName("busy".to_owned()))
        .ok_or_else(|| "the busy unit has no resource observation".to_owned())?;
    match observation.resources {
        WorkloadUnitResources::Available {
            cpu_time_ns,
            memory_bytes,
        } => Ok(Some((cpu_time_ns, memory_bytes))),
        WorkloadUnitResources::Unavailable(WorkloadUnitResourcesUnavailable::NotRunning) => {
            Ok(None)
        }
        other => Err(format!("the busy unit has unexpected resources: {other:?}")),
    }
}

struct TestResources {
    root: PathBuf,
    site_id: String,
    configuration: SitePaths,
    publication_host: Ipv4Addr,
    platform: OciPlatform,
}

impl TestResources {
    async fn new() -> Result<Self, String> {
        // Docker Desktop publishes ports on the Mac, where its VM's bridge gateway is no address.
        if cfg!(target_os = "macos") {
            Self::new_with_publication_host(|| Ok(Ipv4Addr::LOCALHOST)).await
        } else {
            Self::new_with_publication_host(docker_bridge_gateway).await
        }
    }

    async fn new_log_fixture() -> Result<Self, String> {
        Self::new_with_publication_host(|| Ok(Ipv4Addr::LOCALHOST)).await
    }

    async fn new_with_publication_host(
        publication_host: fn() -> Result<Ipv4Addr, String>,
    ) -> Result<Self, String> {
        command("docker", ["info"])?;
        let platform = native_docker_platform()?;
        eprintln!(
            "[docker-apps] operation=preflight docker_platform={}/{}",
            platform.operating_system, platform.architecture
        );
        let fixture_publication_host = publication_host()?;
        let docker_config = std::env::var_os("DOCKER_CONFIG")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".docker")));
        if let Some(config) = docker_config.filter(|path| path.exists()) {
            fs::read_dir(&config)
                .map_err(|error| format!("Docker config is unreadable: {error}"))?;
        }
        ensure_pinned_image(BUSYBOX_IMAGE, &platform).await?;
        let master_catalog = repository_catalog()?;
        let unique_id = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("read system clock: {error}"))?
                .as_nanos(),
        );
        let site_id = format!("docker-apps-{unique_id}");
        let root =
            crate::support::canonical_temp_dir().join(format!("dotcore-docker-apps-{unique_id}"));
        fs::create_dir(&root).map_err(|error| format!("create temporary site root: {error}"))?;
        let configuration = (|| {
            materialize_temporary_site_configuration(
                &root,
                &site_id,
                &fixture_publication_host.to_string(),
                &platform,
            )?;
            let configuration = SitePaths::from_values(
                Some(
                    root.to_str()
                        .ok_or_else(|| "temporary site path is not UTF-8".to_owned())?,
                ),
                Some(
                    master_catalog
                        .to_str()
                        .ok_or_else(|| "Master catalog path is not UTF-8".to_owned())?,
                ),
                Some(
                    root.join("cache")
                        .to_str()
                        .ok_or_else(|| "cache path is not UTF-8".to_owned())?,
                ),
            )
            .map_err(|error| format!("temporary site configuration: {error}"))?;
            let publication_host = configured_publication_host(&root)?;
            let listener = std::net::TcpListener::bind((publication_host, 0)).map_err(|error| {
                format!("configured node host {publication_host} cannot bind: {error}")
            })?;
            drop(listener);
            Ok((configuration, publication_host))
        })();
        let (configuration, publication_host) = match configuration {
            Ok(configuration) => configuration,
            Err(error) => {
                fs::remove_dir_all(&root)
                    .map_err(|cleanup| format!("{error}; remove temporary site root: {cleanup}"))?;
                return Err(error);
            }
        };
        Ok(Self {
            root,
            site_id,
            configuration,
            publication_host,
            platform,
        })
    }

    async fn composition_services(
        &self,
    ) -> Result<dotcore::composition::IsolatedApplicationService, String> {
        support::wait_for_site_store_release(&self.configuration).await;
        dotcore::composition::application_service_for(&self.configuration, 50052)
            .await
            .map_err(|error| format!("composition Docker site: {error}"))
    }

    fn write_fixture_inference_route(&self) -> Result<(), String> {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("fixtures/simulated-site");
        let catalog = self.root.join("catalog");
        fs::create_dir_all(catalog.join("models"))
            .map_err(|error| format!("create fixture models: {error}"))?;
        fs::create_dir_all(catalog.join("engines"))
            .map_err(|error| format!("create fixture engines: {error}"))?;
        fs::create_dir_all(catalog.join("hardware-classes"))
            .map_err(|error| format!("create fixture hardware classes: {error}"))?;
        fs::copy(
            fixture.join(support::FIXTURE_HARDWARE_CLASS),
            self.root.join(support::FIXTURE_HARDWARE_CLASS),
        )
        .map_err(|error| format!("copy fixture hardware class: {error}"))?;
        fs::copy(
            fixture.join("catalog/models/fixture-model.yaml"),
            catalog.join("models/fixture-model.yaml"),
        )
        .map_err(|error| format!("copy fixture model: {error}"))?;
        // The fixture engine approves both Linux architectures the Docker preflight accepts.
        fs::copy(
            fixture.join("catalog/engines/fixture.yaml"),
            catalog.join("engines/fixture.yaml"),
        )
        .map_err(|error| format!("copy fixture engine: {error}"))?;
        let site_path = self.root.join("site.yaml");
        let mut site: dotcore_proto::dotcore::site::v1::Site =
            dotcore_proto::yaml::from_file(&site_path)
                .map_err(|error| format!("read temporary site: {error}"))?;
        let simulated_adapter = dotcore_proto::dotcore::site::v1::SimulatedRuntimeAdapter {
            default_scenario: Some(dotcore_proto::dotcore::site::v1::SimulatedRuntimeScenario {
                ready_delay_millis: 0,
                stop_delay_millis: 0,
            }),
            deployment_scenarios: Vec::new(),
        };
        // A simulated model target runs on a declared machine: the node declares a simulated RTX
        // 5090 and the target holds its GPU, while applications stay on the real Docker host.
        let machine = crate::support::simulated_machine(&["rtx-5090"]);
        let node_path = self.root.join("node.yaml");
        let mut node: dotcore_proto::dotcore::node::v1::NodeConfiguration =
            dotcore_proto::yaml::from_file(&node_path)
                .map_err(|error| format!("read temporary node: {error}"))?;
        node.declared_machine = Some(crate::support::declared_machine(&["rtx-5090"]));
        let node = dotcore_proto::yaml::to_string(&node)
            .map_err(|error| format!("serialize temporary node: {error}"))?;
        fs::write(node_path, node).map_err(|error| format!("write temporary node: {error}"))?;
        site.execution_targets
            .push(dotcore_proto::dotcore::site::v1::ExecutionTarget {
                execution_target_id: "fixture-target".to_owned(),
                display_name: "fixture-target".to_owned(),
                site_node_id: "docker-apps".to_owned(),
                accelerator_device_ids: machine.gpu_ids().map(str::to_owned).collect(),
                platform: Some(self.platform.clone()),
                runtime_binding: Some(
                    dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(
                        dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                            adapter: Some(
                                dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter::SimulatedRuntime(
                                    simulated_adapter,
                                ),
                            ),
                        },
                    ),
                ),
            });
        let site = dotcore_proto::yaml::to_string(&site)
            .map_err(|error| format!("serialize fixture inference route site: {error}"))?;
        fs::write(site_path, site)
            .map_err(|error| format!("write fixture inference route site: {error}"))
    }

    /// Declares one accelerator device on the temporary site's application target.
    #[cfg(not(target_os = "macos"))]
    fn declare_gpu_device_on_application_target(&self, device: &str) -> Result<(), String> {
        let site_path = self.root.join("site.yaml");
        let mut site: dotcore_proto::dotcore::site::v1::Site =
            dotcore_proto::yaml::from_file(&site_path)
                .map_err(|error| format!("read temporary site: {error}"))?;
        let target = site
            .execution_targets
            .iter_mut()
            .find(|target| target.execution_target_id == "docker-apps-target")
            .ok_or_else(|| "temporary site did not declare its application target".to_owned())?;
        target.accelerator_device_ids = vec![device.to_owned()];
        let site = dotcore_proto::yaml::to_string(&site)
            .map_err(|error| format!("serialize temporary site: {error}"))?;
        fs::write(site_path, site).map_err(|error| format!("write temporary site: {error}"))
    }

    fn assert_lifecycle_runtime(
        &self,
        deployment_id: &str,
        endpoint: &str,
        data: &Path,
        expected: &ConfigurationExpectation,
    ) -> Result<(), String> {
        let container = self.running_container_id_for_group(deployment_id, "web")?;
        let inspection: Value = serde_json::from_str(&command(
            "docker",
            ["inspect", "--format", "{{json .}}", &container],
        )?)
        .map_err(|error| format!("decode lifecycle Docker inspection: {error}"))?;
        let environment: Vec<String> = serde_json::from_value(inspection["Config"]["Env"].clone())
            .map_err(|error| format!("decode lifecycle container environment: {error}"))?;
        for value in [
            "SYNTHETIC_LITERAL=literal-value".to_owned(),
            format!(
                "SYNTHETIC_INFERENCE_BASE_URL={}",
                expected.inference_base_url
            ),
            format!("SYNTHETIC_ROUTE_NAME={}", expected.route_name),
        ] {
            if !environment.contains(&value) {
                return Err(format!(
                    "lifecycle container environment is missing {value}: {environment:?}"
                ));
            }
        }
        if inspection["HostConfig"]["NetworkMode"].as_str() == Some("host") {
            return Err("lifecycle container uses host networking".to_owned());
        }
        if inspection["HostConfig"]["ExtraHosts"]
            .as_array()
            .is_some_and(|hosts| !hosts.is_empty())
        {
            return Err("lifecycle container declares host aliases".to_owned());
        }
        let mounts = inspection["Mounts"]
            .as_array()
            .ok_or_else(|| "lifecycle Docker mounts are not an array".to_owned())?;
        if mounts.len() != 1
            || mounts[0]["Destination"].as_str() != Some("/data")
            || mounts[0]["Source"].as_str() != data.to_str()
        {
            return Err(format!(
                "lifecycle container does not mount only its application data: {mounts:?}"
            ));
        }
        let expected_network = self.only_network_id_for_group(deployment_id)?;
        let networks = inspection["NetworkSettings"]["Networks"]
            .as_object()
            .ok_or_else(|| "lifecycle Docker networks are not an object".to_owned())?;
        if networks.len() != 1
            || networks.values().any(|attachment| {
                attachment["NetworkID"].as_str() != Some(expected_network.as_str())
                    || !attachment["Aliases"].as_array().is_some_and(|aliases| {
                        aliases.iter().any(|alias| alias.as_str() == Some("web"))
                    })
            })
        {
            return Err(format!(
                "lifecycle container is not attached only to its private network with a web DNS alias: {networks:?}"
            ));
        }
        let ports = inspection["NetworkSettings"]["Ports"]
            .as_object()
            .ok_or_else(|| "lifecycle Docker ports are not an object".to_owned())?;
        let binding = ports
            .get("8080/tcp")
            .and_then(Value::as_array)
            .filter(|bindings| bindings.len() == 1)
            .and_then(|bindings| bindings.first())
            .ok_or_else(|| {
                "lifecycle container does not have exactly one 8080/tcp binding".to_owned()
            })?;
        let publication_host = self.publication_host.to_string();
        let host_port = binding
            .get("HostPort")
            .and_then(Value::as_str)
            .filter(|port| !port.is_empty())
            .ok_or_else(|| "lifecycle publication has no host port".to_owned())?;
        if ports.len() != 1
            || binding.get("HostIp").and_then(Value::as_str) != Some(publication_host.as_str())
            || endpoint != format!("http://{publication_host}:{host_port}")
        {
            return Err(format!(
                "lifecycle endpoint {endpoint} is not its single publication on the configured node host: {ports:?}"
            ));
        }
        let marker = container_file(&container, &format!("/data/{STORAGE_MARKER_FILE}"))?;
        if marker != STORAGE_MARKER_CONTENT {
            return Err(format!(
                "lifecycle container sees a different storage marker: {marker:?}"
            ));
        }
        Ok(())
    }

    fn group_containers(&self, group: &str) -> Result<Vec<DockerGroupContainer>, String> {
        let mut containers = self
            .group_container_ids(true, group)?
            .into_iter()
            .map(|id| self.inspect_group_container(&id))
            .collect::<Result<Vec<_>, _>>()?;
        containers.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(containers)
    }

    fn inspect_group_container(&self, container: &str) -> Result<DockerGroupContainer, String> {
        let inspection: Value = serde_json::from_str(&command(
            "docker",
            ["inspect", "--format", "{{json .}}", container],
        )?)
        .map_err(|error| format!("decode Docker group container inspection: {error}"))?;
        let id = inspection["Id"]
            .as_str()
            .ok_or_else(|| "Docker group container inspection has no ID".to_owned())?
            .to_owned();
        let state = inspection["State"]
            .as_object()
            .ok_or_else(|| "Docker group container inspection has no state".to_owned())?;
        let running = state
            .get("Running")
            .and_then(Value::as_bool)
            .ok_or_else(|| "Docker group container state has no running flag".to_owned())?;
        let pid = state
            .get("Pid")
            .and_then(Value::as_u64)
            .ok_or_else(|| "Docker group container state has no process ID".to_owned())?;
        let status = state
            .get("Status")
            .and_then(Value::as_str)
            .ok_or_else(|| "Docker group container state has no status".to_owned())?
            .to_owned();
        let network_ids = inspection["NetworkSettings"]["Networks"]
            .as_object()
            .ok_or_else(|| "Docker group container inspection has no networks".to_owned())?
            .values()
            .map(|network| {
                network["NetworkID"]
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| "Docker group network attachment has no ID".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let host_config = inspection["HostConfig"]
            .as_object()
            .ok_or_else(|| "Docker group container inspection has no host config".to_owned())?;
        let mut network_ids = network_ids;
        network_ids.sort();
        Ok(DockerGroupContainer {
            id,
            running,
            pid,
            status,
            retained: DockerRetainedResources {
                network_ids,
                mounts: inspection["Mounts"].clone(),
                // Docker permits zero for an unbounded CPU or memory limit. Retention
                // compares the configured value without requiring a nonzero quota.
                cpu_limit: host_config.get("NanoCpus").cloned().unwrap_or(Value::Null),
                memory_limit: host_config.get("Memory").cloned().unwrap_or(Value::Null),
                gpu_devices: host_config
                    .get("DeviceRequests")
                    .cloned()
                    .unwrap_or(Value::Null),
            },
        })
    }

    fn container_id_for_unit(&self, unit: &str) -> Result<String, String> {
        let ids = self.container_ids(true, Some(unit))?;
        if ids.len() != 1 {
            return Err(format!(
                "expected one test-owned {unit} container, found {}",
                ids.len()
            ));
        }
        Ok(ids[0].to_owned())
    }

    fn running_container_id_for_unit(&self, unit: &str) -> Result<String, String> {
        let ids = self.container_ids(false, Some(unit))?;
        if ids.len() != 1 {
            return Err(format!(
                "expected one running test-owned {unit} container, found {}",
                ids.len()
            ));
        }
        Ok(ids[0].to_owned())
    }

    fn container_id_for_group(&self, group: &str, unit: &str) -> Result<String, String> {
        let ids = self.container_ids_for_group(true, group, unit)?;
        if ids.len() != 1 {
            return Err(format!(
                "expected one test-owned {group} {unit} container, found {}",
                ids.len()
            ));
        }
        Ok(ids[0].to_owned())
    }

    fn running_container_id_for_group(&self, group: &str, unit: &str) -> Result<String, String> {
        let ids = self.container_ids_for_group(false, group, unit)?;
        if ids.len() != 1 {
            return Err(format!(
                "expected one running test-owned {group} {unit} container, found {}",
                ids.len()
            ));
        }
        Ok(ids[0].to_owned())
    }

    #[cfg(not(target_os = "macos"))]
    fn docker_endpoint_for_unit(&self, unit: &str) -> Result<String, String> {
        let ports = self.docker_ports_for_unit(unit)?;
        docker_endpoint_from_ports(ports)
    }

    #[cfg(not(target_os = "macos"))]
    fn assert_one_configured_publication(
        &self,
        published_unit: &str,
        private_unit: &str,
    ) -> Result<(), String> {
        let published = self.docker_ports_for_unit(published_unit)?;
        let published = published
            .as_object()
            .ok_or_else(|| "Docker publication inspection is not an object".to_owned())?;
        let binding = published
            .get("8081/tcp")
            .and_then(Value::as_array)
            .filter(|bindings| bindings.len() == 1)
            .and_then(|bindings| bindings.first())
            .ok_or_else(|| {
                "published unit does not have exactly one dynamic host binding".to_owned()
            })?;
        let publication_host = self.publication_host.to_string();
        if published.len() != 1
            || binding.get("HostIp").and_then(Value::as_str) != Some(publication_host.as_str())
            || binding
                .get("HostPort")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(
                "published unit does not use the configured host and one dynamic port".to_owned(),
            );
        }
        let private = self.docker_ports_for_unit(private_unit)?;
        if private
            .as_object()
            .is_none_or(|ports| ports.values().any(|binding| binding.is_array()))
        {
            return Err("dependency unit unexpectedly has a host binding".to_owned());
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    fn docker_ports_for_unit(&self, unit: &str) -> Result<Value, String> {
        let container = self.container_id_for_unit(unit)?;
        let ports = command(
            "docker",
            [
                "inspect",
                "--format",
                "{{json .NetworkSettings.Ports}}",
                &container,
            ],
        )?;
        serde_json::from_str(&ports).map_err(|error| format!("decode Docker ports: {error}"))
    }

    // Fixture services run under a shell PID 1 that traps SIGTERM, so a stop ends at once;
    // `httpd` or `sleep` as PID 1 would ignore SIGTERM until the stop grace ends.
    fn write_lifecycle_application(&self, artifact: &str) -> Result<(), String> {
        self.write_site_catalog_application(
            "synthetic-lifecycle.yaml",
            &format!(
                r#"applicationId: "{LIFECYCLE_APPLICATION_ID}"
displayName: "Synthetic lifecycle proof"
version: "1.0.0"
graph:
  services:
    - serviceName: "web"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments:
        - "trap 'exit 0' TERM; mkdir -p /www; printf 'lifecycle-ready\\n' > /www/index.html; httpd -f -p 8080 -h /www & wait $!"
      ports:
        - portName: "http"
          portNumber: 8080
      configurationBindings:
        - bindingName: "literal"
          environmentVariable: "SYNTHETIC_LITERAL"
          literalValue: "literal-value"
        - bindingName: "inference-base-url"
          environmentVariable: "SYNTHETIC_INFERENCE_BASE_URL"
          resolvedValueName: "configured-inference-openai-base-url"
        - bindingName: "route-name"
          environmentVariable: "SYNTHETIC_ROUTE_NAME"
          literalValue: "{UNROUTED_VALUE}"
          input:
            inferenceRoute:
              projection: "APPLICATION_INFERENCE_ROUTE_PROJECTION_ROUTE_NAME"
      storageBindings:
        - bindingName: "data"
          mountPath: "/data"
          kind: "STORAGE_KIND_DATA"
          scope: "STORAGE_SCOPE_DEPLOYMENT"
      readiness:
        httpGet:
          portName: "http"
          path: "/"
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#
            ),
        )
    }

    fn write_storage_scopes_application(&self, artifact: &str) -> Result<(), String> {
        self.write_site_catalog_application(
            "synthetic-storage-scopes.yaml",
            &format!(
                r#"applicationId: "{STORAGE_SCOPES_APPLICATION_ID}"
displayName: "Synthetic storage scopes proof"
version: "1.0.0"
graph:
  services:
    - serviceName: "web"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments:
        - "trap 'exit 0' TERM; mkdir -p /www; printf 'ready\\n' > /www/index.html; httpd -f -p 8080 -h /www & wait $!"
      ports:
        - portName: "http"
          portNumber: 8080
      storageBindings:
        - bindingName: "data"
          mountPath: "/data"
          kind: "STORAGE_KIND_DATA"
          scope: "STORAGE_SCOPE_DEPLOYMENT"
        - bindingName: "models"
          mountPath: "/data/models"
          kind: "STORAGE_KIND_DATA"
          scope: "STORAGE_SCOPE_APPLICATION"
      readiness:
        httpGet:
          portName: "http"
          path: "/"
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#
            ),
        )
    }

    fn write_log_application(&self) -> Result<(), String> {
        self.write_site_catalog_application(
            "log-fixture.yaml",
            &format!(
                r#"applicationId: "log-fixture"
displayName: "Log fixture"
version: "1.0.0"
graph:
  services:
    - serviceName: "alpha"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments: ["trap 'exit 0' TERM; printf 'alpha stdout\\n'; printf 'alpha stderr\\n' >&2; printf '%s\\n' \"$PROTECTED_VALUE\"; mkdir -p /www; printf 'ready\\n' > /www/index.html; httpd -f -p 8080 -h /www & wait $!"]
      configurationBindings:
        - bindingName: "protected"
          environmentVariable: "PROTECTED_VALUE"
          literalValue: "protected-value"
      ports:
        - portName: "http"
          portNumber: 8080
      readiness:
        httpGet:
          portName: "http"
          path: "/"
    - serviceName: "gamma"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments: ["trap 'exit 0' TERM; i=0; while [ \"$i\" -lt 66 ]; do head -c 65530 /dev/zero | tr '\\0' x; printf '\\n'; i=$((i + 1)); done; sleep 120 & wait $!"]
    - serviceName: "delta"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments: ["trap 'exit 0' TERM; i=0; while [ \"$i\" -lt 66 ]; do head -c 65530 /dev/zero | tr '\\0' x; printf '\\n'; i=$((i + 1)); done; sleep 120 & wait $!"]
    - serviceName: "epsilon"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments: ["trap 'exit 0' TERM; i=0; while [ \"$i\" -lt 66 ]; do head -c 65530 /dev/zero | tr '\\0' x; printf '\\n'; i=$((i + 1)); done; sleep 120 & wait $!"]
    - serviceName: "beta"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments: ["trap 'exit 0' TERM; printf 'beta stdout\\n'; printf 'beta stderr\\n' >&2; sleep 120 & wait $!"]
  primaryEndpoint:
    serviceName: "alpha"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
                artifact = remote_image_artifact(),
            ),
        )
    }

    #[cfg(not(target_os = "macos"))]
    fn write_synthetic_application(&self) -> Result<(), String> {
        self.write_site_catalog_application(
            "synthetic-two-service.yaml",
            &format!(
                r#"applicationId: "synthetic-two-service"
displayName: "Synthetic two-service Docker proof"
version: "1.0.0"
graph:
  services:
    - serviceName: "dependency"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments:
        - "trap 'exit 0' TERM; mkdir -p /www; printf 'dependency-ready\\n' > /www/ready; sleep 2; httpd -f -p 8080 -h /www & wait $!"
      ports:
        - portName: "http"
          portNumber: 8080
      readiness:
        httpGet:
          portName: "http"
          path: "/ready"
    - serviceName: "published"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments:
        - "trap 'exit 0' TERM; wget -qO- http://dependency:8080/ready | grep -qx 'dependency-ready'; mkdir -p /www; printf 'private-dns-ok\\n' > /www/index.html; httpd -f -p 8081 -h /www & wait $!"
      ports:
        - portName: "http"
          portNumber: 8081
      dependencies:
        - serviceName: "dependency"
          condition: "APPLICATION_DEPENDENCY_CONDITION_READY"
      readiness:
        httpGet:
          portName: "http"
          path: "/"
  primaryEndpoint:
    serviceName: "published"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
                artifact = remote_image_artifact(),
            ),
        )
    }

    /// Writes a one-unit application whose unit holds a CUDA context and a fixed device
    /// allocation through the driver library CDI injects, and serves HTTP for readiness.
    #[cfg(not(target_os = "macos"))]
    fn write_gpu_memory_application(&self) -> Result<(), String> {
        self.write_site_catalog_application(
            "synthetic-gpu-memory.yaml",
            &format!(
                r#"applicationId: "synthetic-gpu-memory"
displayName: "Synthetic GPU memory proof"
version: "1.0.0"
graph:
  services:
    - serviceName: "gpu"
      artifact:
{artifact}      command: ["python", "-c"]
      arguments:
        - 'import ctypes, http.server; cuda = ctypes.CDLL("libcuda.so.1"); context, pointer = ctypes.c_void_p(), ctypes.c_uint64(); assert cuda.cuInit(0) == 0; assert cuda.cuCtxCreate_v2(ctypes.byref(context), 0, 0) == 0; assert cuda.cuMemAlloc_v2(ctypes.byref(pointer), ctypes.c_size_t({allocation})) == 0; http.server.HTTPServer(("", 8080), http.server.SimpleHTTPRequestHandler).serve_forever()'
      ports:
        - portName: "http"
          portNumber: 8080
      readiness:
        httpGet:
          portName: "http"
          path: "/"
  primaryEndpoint:
    serviceName: "gpu"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
                artifact = python_image_artifact(),
                allocation = GPU_FIXTURE_ALLOCATION_BYTES,
            ),
        )
    }

    /// Declares a one-service application with a text username, a secret password and an
    /// outcome choice. Its unit prints the password, then serves, or exits when told to.
    fn write_secret_application(&self) -> Result<(), String> {
        self.write_site_catalog_application(
            "secret-fixture.yaml",
            &format!(
                r#"applicationId: "{SECRET_APPLICATION_ID}"
displayName: "Secret fixture"
version: "1.0.0"
graph:
  parameters:
    - parameterId: "username"
      displayName: "Username"
      visibility: "PARAMETER_VISIBILITY_BASIC"
      text:
        minimumLength: 1
        maximumLength: 64
        defaultValue: "admin"
    - parameterId: "password"
      displayName: "Password"
      visibility: "PARAMETER_VISIBILITY_BASIC"
      secret:
        generatedLength: 32
        minimumLength: 12
    - parameterId: "outcome"
      displayName: "Outcome"
      visibility: "PARAMETER_VISIBILITY_ADVANCED"
      stringEnum:
        values: ["serve", "exit"]
        defaultValue: "serve"
  services:
    - serviceName: "web"
      artifact:
{artifact}      command: ["sh", "-ec"]
      arguments: ["trap 'exit 0' TERM; printf 'login %s %s\\n' \"$USERNAME\" \"$PASSWORD\"; if [ \"$OUTCOME\" = exit ]; then printf 'stopping with %s\\n' \"$PASSWORD\" >&2; exit 3; fi; mkdir -p /www; printf 'ready\\n' > /www/index.html; httpd -f -p 8080 -h /www & wait $!"]
      configurationBindings:
        - bindingName: "username"
          environmentVariable: "USERNAME"
          parameterId: "username"
        - bindingName: "outcome"
          environmentVariable: "OUTCOME"
          parameterId: "outcome"
      secretBindings:
        - bindingName: "password"
          environmentVariable: "PASSWORD"
          parameterId: "password"
          form: "APPLICATION_SECRET_FORM_PLAIN"
      ports:
        - portName: "http"
          portNumber: 8080
      readiness:
        httpGet:
          portName: "http"
          path: "/"
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
                artifact = remote_image_artifact(),
            ),
        )
    }

    fn write_site_catalog_application(
        &self,
        file_name: &str,
        declaration: &str,
    ) -> Result<(), String> {
        let catalog = self.root.join("catalog");
        for directory in ["applications", "models", "engines"] {
            fs::create_dir_all(catalog.join(directory))
                .map_err(|error| format!("create temporary site catalog {directory}: {error}"))?;
        }
        fs::write(catalog.join("applications").join(file_name), declaration)
            .map_err(|error| format!("write {file_name} application catalog declaration: {error}"))
    }

    /// Commits a test-owned local-development image with a unique image ID and returns its tag
    /// and image ID.
    fn create_local_image(&self) -> Result<(String, String), String> {
        let tag = self.local_image_tag();
        let container = command_owned(
            "docker",
            &[
                "create".to_owned(),
                "--label".to_owned(),
                format!("com.dotcore.site={}", self.site_id),
                BUSYBOX_IMAGE.to_owned(),
                "true".to_owned(),
            ],
        )?;
        let committed = command_owned(
            "docker",
            &[
                "commit".to_owned(),
                "--change".to_owned(),
                format!("LABEL com.dotcore.test.site={}", self.site_id),
                container.clone(),
                tag.clone(),
            ],
        );
        let removed = command("docker", ["rm", &container]);
        committed?;
        removed?;
        let image_id = command("docker", ["image", "inspect", "--format", "{{.Id}}", &tag])?;
        Ok((tag, image_id))
    }

    fn local_image_tag(&self) -> String {
        format!("{LOCAL_IMAGE_REPOSITORY}:{}", self.site_id)
    }

    fn prepare_application_data(
        &self,
        deployment_id: &str,
        storage_name: &str,
    ) -> Result<(), String> {
        let path = self.application_data_path(deployment_id, storage_name);
        fs::create_dir_all(&path)
            .map_err(|error| format!("create temporary application data directory: {error}"))?;
        if fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).is_err() {
            command(
                "docker",
                [
                    "run",
                    "--rm",
                    "--user",
                    "0",
                    "--mount",
                    &format!("type=bind,src={},dst=/opt/data", path.display()),
                    BUSYBOX_IMAGE,
                    "chmod",
                    "777",
                    "/opt/data",
                ],
            )
            .map_err(|error| format!("make application data removable: {error}"))?;
        }
        Ok(())
    }

    fn application_data_path(&self, deployment_id: &str, storage_name: &str) -> PathBuf {
        self.root
            .join("applications-data")
            .join("deployment")
            .join(deployment_id)
            .join(storage_name)
    }

    /// Waits until the hosting node launched the deployment with its own data directory, then
    /// makes that directory removable by the test and writes the storage marker. Data seeded
    /// before the node's first realization would be foreign data the node fails closed on.
    async fn seed_realized_application_data(
        &self,
        deployment_id: &str,
        storage_name: &str,
    ) -> Result<PathBuf, String> {
        let data = self.application_data_path(deployment_id, storage_name);
        timeout(READY_TIMEOUT, async {
            while !data.exists() {
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "timed out waiting for the node's application data directory".to_owned())?;
        self.prepare_application_data(deployment_id, storage_name)?;
        self.write_storage_marker(&data)?;
        Ok(data)
    }

    fn write_storage_marker(&self, data: &Path) -> Result<(), String> {
        fs::write(data.join(STORAGE_MARKER_FILE), STORAGE_MARKER_CONTENT)
            .map_err(|error| format!("write application storage marker: {error}"))
    }

    fn write_marker(&self, data: &Path, name: &str, content: &str) -> Result<(), String> {
        if fs::write(data.join(name), content).is_err() {
            command(
                "docker",
                [
                    "run",
                    "--rm",
                    "--user",
                    "0",
                    "--mount",
                    &format!("type=bind,src={},dst=/opt/data", data.display()),
                    BUSYBOX_IMAGE,
                    "sh",
                    "-c",
                    "printf '%s' \"$1\" > \"/opt/data/$2\"",
                    "--",
                    content,
                    name,
                ],
            )
            .map_err(|error| format!("write container-owned application-data marker: {error}"))?;
        }
        Ok(())
    }

    fn assert_storage_marker(&self, data: &Path) -> Result<(), String> {
        let marker = fs::read_to_string(data.join(STORAGE_MARKER_FILE))
            .map_err(|error| format!("read application storage marker: {error}"))?;
        if marker != STORAGE_MARKER_CONTENT {
            return Err(format!(
                "Application storage marker contents differ: expected {STORAGE_MARKER_CONTENT:?}, got {marker:?}"
            ));
        }
        Ok(())
    }

    /// Waits until removing the deployment deleted its whole application-data directory.
    async fn wait_for_application_data_removed(&self, data: &Path) -> Result<(), String> {
        let deployment_data = data
            .parent()
            .ok_or_else(|| "Application storage has no deployment directory".to_owned())?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while deployment_data.exists() {
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "removed deployment kept its application data at {}",
                    deployment_data.display()
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    fn assert_group_units(&self, expected: &[&str]) -> Result<(), String> {
        let observed = self.container_ids(true, None)?;
        if observed.len() != expected.len() {
            return Err(format!(
                "expected {} test-owned containers, found {}",
                expected.len(),
                observed.len()
            ));
        }
        for unit in expected {
            self.container_id_for_unit(unit)?;
        }
        Ok(())
    }

    /// Asserts one running group unit's container requests exactly the target's CDI device.
    #[cfg(not(target_os = "macos"))]
    fn assert_group_unit_device_request(&self, unit: &str, device: &str) -> Result<(), String> {
        let container = self.running_container_id_for_unit(unit)?;
        let requests: Value = serde_json::from_str(&command(
            "docker",
            [
                "inspect",
                "--format",
                "{{json .HostConfig.DeviceRequests}}",
                &container,
            ],
        )?)
        .map_err(|error| format!("decode {unit} container device requests: {error}"))?;
        let entries = requests
            .as_array()
            .ok_or_else(|| format!("{unit} container device requests are not an array"))?;
        if entries.len() != 1 {
            return Err(format!(
                "expected exactly one device request on {unit}, got {entries:?}"
            ));
        }
        let entry = &entries[0];
        if entry["Driver"].as_str() != Some("cdi") {
            return Err(format!("{unit} device request is not CDI: {entry:?}"));
        }
        if entry["DeviceIDs"] != Value::Array(vec![Value::String(device.to_owned())]) {
            return Err(format!(
                "{unit} device request selects the wrong devices: {entry:?}"
            ));
        }
        Ok(())
    }

    fn assert_no_docker_resources(&self) -> Result<(), String> {
        let containers = self.container_ids(true, None)?;
        let networks = self.network_ids()?;
        if containers.is_empty() && networks.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "desired TEARDOWN left {} test-owned containers and {} test-owned networks",
                containers.len(),
                networks.len()
            ))
        }
    }

    async fn wait_for_no_docker_resources<C>(
        &self,
        service: &C,
        deployment_id: &str,
    ) -> Result<(), String>
    where
        C: ApplicationServiceContract,
    {
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        loop {
            let deployment = ApplicationServiceContract::get_application_deployment(
                service,
                Request::new(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                }),
            )
            .await;
            match self.assert_no_docker_resources() {
                Ok(()) => match deployment {
                    Err(status) if status.code() == Code::NotFound => return Ok(()),
                    Ok(response) => {
                        if response.into_inner().deployment.is_some_and(|deployment| {
                            deployment.desired_state
                                == ApplicationDeploymentDesiredState::Teardown as i32
                        }) {
                            if Instant::now() >= deadline {
                                return Err(
                                    "Application cleanup completed but the controller did not delete the teardown declaration"
                                        .to_owned(),
                                );
                            }
                        } else {
                            return Err(
                                "Application cleanup completed but the controller did not return a teardown declaration"
                                    .to_owned(),
                            );
                        }
                    }
                    Err(status) => {
                        return Err(format!(
                            "read desired TEARDOWN application deployment after cleanup: {status}"
                        ));
                    }
                },
                Err(resources) if Instant::now() >= deadline => {
                    let state = deployment
                        .ok()
                        .and_then(|response| response.into_inner().deployment)
                        .map(|deployment| (deployment.desired_state, deployment.state));
                    return Err(format!(
                        "timed out waiting for desired TEARDOWN Docker cleanup: {resources}; controller state {state:?}",
                    ));
                }
                Err(_) => {}
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    fn container_ids(&self, all: bool, unit: Option<&str>) -> Result<Vec<String>, String> {
        let mut arguments = vec!["ps".to_owned(), if all { "-aq" } else { "-q" }.to_owned()];
        arguments.extend([
            "--filter".to_owned(),
            format!("label=com.dotcore.site={}", self.site_id),
        ]);
        if let Some(unit) = unit {
            arguments.extend([
                "--filter".to_owned(),
                format!("label=com.dotcore.runtime.unit={unit}"),
            ]);
        }
        Ok(output_lines(command_owned("docker", &arguments)?))
    }

    fn container_ids_for_group(
        &self,
        all: bool,
        group: &str,
        unit: &str,
    ) -> Result<Vec<String>, String> {
        let arguments = vec![
            "ps".to_owned(),
            if all { "-aq" } else { "-q" }.to_owned(),
            "--filter".to_owned(),
            format!("label=com.dotcore.site={}", self.site_id),
            "--filter".to_owned(),
            format!("label=com.dotcore.runtime.group={group}"),
            "--filter".to_owned(),
            format!("label=com.dotcore.runtime.unit={unit}"),
        ];
        Ok(output_lines(command_owned("docker", &arguments)?))
    }

    fn group_container_ids(&self, all: bool, group: &str) -> Result<Vec<String>, String> {
        let arguments = vec![
            "ps".to_owned(),
            if all { "-aq" } else { "-q" }.to_owned(),
            "--filter".to_owned(),
            format!("label=com.dotcore.site={}", self.site_id),
            "--filter".to_owned(),
            format!("label=com.dotcore.runtime.group={group}"),
        ];
        Ok(output_lines(command_owned("docker", &arguments)?))
    }

    fn network_ids(&self) -> Result<Vec<String>, String> {
        Ok(output_lines(command_owned(
            "docker",
            &[
                "network".to_owned(),
                "ls".to_owned(),
                "-q".to_owned(),
                "--filter".to_owned(),
                format!("label=com.dotcore.site={}", self.site_id),
            ],
        )?))
    }

    fn network_ids_for_group(&self, group: &str) -> Result<Vec<String>, String> {
        let networks = output_lines(command_owned(
            "docker",
            &[
                "network".to_owned(),
                "ls".to_owned(),
                "-q".to_owned(),
                "--filter".to_owned(),
                format!("label=com.dotcore.site={}", self.site_id),
                "--filter".to_owned(),
                format!("label=com.dotcore.runtime.group={group}"),
            ],
        )?);
        networks
            .iter()
            .map(|network| {
                command(
                    "docker",
                    ["network", "inspect", "--format", "{{.Id}}", network],
                )
                .map_err(|error| format!("inspect expected Docker network: {error}"))
            })
            .collect()
    }

    fn only_network_id_for_group(&self, group: &str) -> Result<String, String> {
        let networks = self.network_ids_for_group(group)?;
        if networks.len() != 1 {
            return Err(format!(
                "expected one test-owned {group} network, found {}",
                networks.len()
            ));
        }
        Ok(networks[0].clone())
    }

    fn remove_group_containers(&self, group: &str) -> Result<(), String> {
        let arguments = vec![
            "ps".to_owned(),
            "-aq".to_owned(),
            "--filter".to_owned(),
            format!("label=com.dotcore.site={}", self.site_id),
            "--filter".to_owned(),
            format!("label=com.dotcore.runtime.group={group}"),
        ];
        for container in output_lines(command_owned("docker", &arguments)?) {
            command("docker", ["rm", "-f", &container])?;
        }
        Ok(())
    }

    fn assert_exact_empty_group_network(
        &self,
        group: &str,
        expected_id: &str,
    ) -> Result<(), String> {
        let network = self.only_network_id_for_group(group)?;
        if network != expected_id {
            return Err(format!(
                "group network ID changed: expected {expected_id}, got {network}"
            ));
        }
        let containers = command(
            "docker",
            [
                "network",
                "inspect",
                "--format",
                "{{json .Containers}}",
                &network,
            ],
        )?;
        let containers: Value = serde_json::from_str(&containers)
            .map_err(|error| format!("decode group network attachments: {error}"))?;
        if containers
            .as_object()
            .is_none_or(|containers| !containers.is_empty())
        {
            return Err(format!("group network is not empty: {containers}"));
        }
        Ok(())
    }

    fn cleanup(&self) -> Result<(), String> {
        for container in self.container_ids(true, None)? {
            command("docker", ["rm", "-f", &container])?;
        }
        for network in self.network_ids()? {
            command("docker", ["network", "rm", &network])?;
        }
        let local_image = self.local_image_tag();
        if !command("docker", ["image", "ls", "--quiet", &local_image])?.is_empty() {
            command("docker", ["image", "rm", &local_image])?;
        }
        match fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                self.remove_container_owned_site_data()?;
                fs::remove_dir_all(&self.root).map_err(|error| {
                    format!("remove temporary site root after data cleanup: {error}")
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove temporary site root: {error}")),
        }
    }

    fn remove_container_owned_site_data(&self) -> Result<(), String> {
        command_owned(
            "docker",
            &[
                "run".to_owned(),
                "--pull=never".to_owned(),
                "--rm".to_owned(),
                "--network=none".to_owned(),
                "--label".to_owned(),
                format!("com.dotcore.site={}", self.site_id),
                "--mount".to_owned(),
                format!(
                    "type=bind,src={},dst=/site",
                    self.root
                        .to_str()
                        .ok_or_else(|| "temporary site root is not UTF-8".to_owned())?
                ),
                BUSYBOX_IMAGE.to_owned(),
                "sh".to_owned(),
                "-c".to_owned(),
                "rm -rf /site/* /site/.[!.]* /site/..?*".to_owned(),
            ],
        )
        .map(|_| ())
        .map_err(|error| format!("remove container-owned temporary site data: {error}"))
    }
}

impl Drop for TestResources {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
fn docker_bridge_gateway() -> Result<Ipv4Addr, String> {
    let gateway = command(
        "docker",
        [
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{(index .IPAM.Config 0).Gateway}}",
        ],
    )?
    .trim()
    .parse::<Ipv4Addr>()
    .map_err(|error| format!("Docker bridge gateway is invalid: {error}"))?;
    if gateway.is_unspecified() || gateway.is_loopback() {
        return Err(format!(
            "Docker bridge gateway is not bridge-reachable: {gateway}"
        ));
    }
    Ok(gateway)
}

fn materialize_temporary_site_configuration(
    root: &Path,
    site_id: &str,
    publication_host: &str,
    platform: &OciPlatform,
) -> Result<(), String> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("fixtures/docker-apps-site");
    let site = fs::read_to_string(fixture.join("site.yaml"))
        .map_err(|error| format!("read Docker site fixture: {error}"))?
        .replacen("docker-apps-site", site_id, 1)
        .replacen("host: 127.0.0.1", &format!("host: {publication_host}"), 1)
        .replacen(
            "platform: { operatingSystem: linux, architecture: amd64 }",
            &format!(
                "platform: {{ operatingSystem: {}, architecture: {} }}",
                platform.operating_system, platform.architecture
            ),
            1,
        );
    fs::write(root.join("site.yaml"), site)
        .map_err(|error| format!("write temporary Docker site fixture: {error}"))?;
    support::configure_observation_interval(root, 1);
    fs::copy(fixture.join("node.yaml"), root.join("node.yaml"))
        .map_err(|error| format!("copy Docker node fixture: {error}"))?;
    Ok(())
}

fn native_docker_platform() -> Result<OciPlatform, String> {
    let platform = command(
        "docker",
        ["info", "--format", "{{.OSType}}/{{.Architecture}}"],
    )?;
    let (operating_system, architecture) = platform
        .split_once('/')
        .ok_or_else(|| format!("Docker reported an invalid native platform {platform:?}"))?;
    let platform = normalize_docker_platform(operating_system, architecture)?;
    let host_architecture =
        normalize_docker_platform("linux", std::env::consts::ARCH)?.architecture;
    let daemon_operating_system = docker_daemon_operating_system()?;
    verify_native_docker_daemon(
        std::env::consts::OS,
        &daemon_operating_system,
        &platform.architecture,
        &host_architecture,
    )?;
    Ok(platform)
}

fn docker_daemon_operating_system() -> Result<String, String> {
    command("docker", ["info", "--format", "{{.OperatingSystem}}"])
}

fn is_docker_desktop(daemon_operating_system: &str) -> bool {
    daemon_operating_system
        .to_ascii_lowercase()
        .contains("docker desktop")
}

/// Accepts a native-architecture daemon: direct Docker on a Linux host, or Docker Desktop's
/// Linux VM on a Mac.
fn verify_native_docker_daemon(
    host_operating_system: &str,
    daemon_operating_system: &str,
    docker_architecture: &str,
    host_architecture: &str,
) -> Result<(), String> {
    let docker_desktop = is_docker_desktop(daemon_operating_system);
    match host_operating_system {
        "linux" if docker_desktop => {
            return Err(
                "Docker application integration does not run on Docker Desktop on Linux".to_owned(),
            );
        }
        "macos" if !docker_desktop => {
            return Err(
                "Docker application integration requires Docker Desktop on macOS".to_owned(),
            );
        }
        "linux" | "macos" => {}
        _ => {
            return Err(format!(
                "Docker application integration requires Linux or macOS, got {host_operating_system}"
            ));
        }
    }
    if docker_architecture != host_architecture {
        return Err(format!(
            "Docker application integration requires a native Docker daemon matching host architecture {host_architecture}, got {docker_architecture}"
        ));
    }
    Ok(())
}

fn normalize_docker_platform(
    operating_system: &str,
    architecture: &str,
) -> Result<OciPlatform, String> {
    if operating_system != "linux" {
        return Err(format!(
            "Docker application integration requires Docker operating system linux, got {operating_system}"
        ));
    }
    let architecture = match architecture {
        "amd64" | "x86_64" => "amd64",
        "arm64" | "aarch64" => "arm64",
        architecture => {
            return Err(format!(
                "Docker application integration requires Docker architecture amd64 or arm64, got {architecture}"
            ));
        }
    };
    Ok(OciPlatform {
        operating_system: "linux".to_owned(),
        architecture: architecture.to_owned(),
        variant: None,
    })
}

fn configured_publication_host(site_root: &Path) -> Result<Ipv4Addr, String> {
    let site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_root.join("site.yaml"))
            .map_err(|error| format!("load temporary site configuration: {error}"))?;
    let node = site
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .ok_or_else(|| "temporary site configuration has no site manager node".to_owned())?;
    let host = node.host.parse::<Ipv4Addr>().map_err(|error| {
        format!(
            "temporary node {:?} has invalid publication host {:?}: {error}",
            node.node_id, node.host
        )
    })?;
    if host.is_unspecified() {
        return Err(format!(
            "temporary node {:?} publication host cannot be wildcard",
            node.node_id
        ));
    }
    Ok(host)
}

// Gated hardware: Docker runs deploy applications from the real master catalog.
fn repository_catalog() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("catalog")
        .canonicalize()
        .map_err(|error| format!("resolve master catalog: {error}"))
}

/// Renders a synthetic service artifact for the pinned remote busybox image as catalog YAML.
fn remote_image_artifact() -> String {
    format!(
        "        oci:\n          repository:\n            name: \"busybox\"\n          digest:\n            algorithm: \"sha256\"\n            encoded: \"{BUSYBOX_DIGEST}\"\n{SUPPORTED_PLATFORMS}"
    )
}

/// Renders a synthetic service artifact for the pinned remote Python image as catalog YAML.
#[cfg(not(target_os = "macos"))]
fn python_image_artifact() -> String {
    format!(
        "        oci:\n          repository:\n            name: \"python\"\n          digest:\n            algorithm: \"sha256\"\n            encoded: \"{PYTHON_DIGEST}\"\n{SUPPORTED_PLATFORMS}"
    )
}

/// Renders a synthetic service artifact for a test-owned local-development tag as catalog YAML.
fn local_image_artifact(tag: &str) -> String {
    format!(
        "        oci:\n          repository:\n            name: \"{LOCAL_IMAGE_REPOSITORY}\"\n          tag: \"{tag}\"\n{SUPPORTED_PLATFORMS}"
    )
}

fn path_argument(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

async fn ensure_pinned_image(image: &str, platform: &OciPlatform) -> Result<(), String> {
    let docker = Docker::connect_with_local_defaults()
        .map_err(|error| format!("connect Docker for pinned image preflight: {error}"))?;
    ensure_pinned_image_with(
        image,
        platform,
        || inspect_pinned_image(&docker, image),
        |selected_platform| pull_pinned_image(&docker, image, selected_platform),
    )
    .await
}

async fn ensure_pinned_image_with<Inspect, InspectFuture, Pull, PullFuture>(
    image: &str,
    platform: &OciPlatform,
    mut inspect: Inspect,
    mut pull: Pull,
) -> Result<(), String>
where
    Inspect: FnMut() -> InspectFuture,
    InspectFuture: Future<Output = Result<Option<PinnedImageInspection>, String>>,
    Pull: FnMut(String) -> PullFuture,
    PullFuture: Future<Output = Result<(), String>>,
{
    let selected_platform = docker_platform(platform);
    eprintln!("[docker-apps] operation=image inspect phase=before image={image}");
    let initial = inspect().await?;
    if initial
        .as_ref()
        .is_some_and(|inspection| pinned_image_matches(image, platform, inspection))
    {
        eprintln!(
            "[docker-apps] operation=image inspect phase=after image={image} state=verified platform={selected_platform}"
        );
        return Ok(());
    }

    eprintln!(
        "[docker-apps] operation=image inspect phase=after image={image} state=missing-or-mismatched"
    );
    eprintln!(
        "[docker-apps] operation=image pull phase=before image={image} platform={selected_platform}"
    );
    pull(selected_platform.clone()).await?;
    eprintln!(
        "[docker-apps] operation=image pull phase=after image={image} platform={selected_platform} state=complete"
    );
    eprintln!("[docker-apps] operation=image inspect phase=before image={image}");
    let realized = inspect().await?;
    let Some(realized) = realized else {
        return Err(format!(
            "pinned image {image:?} is unavailable after native pull for {selected_platform}"
        ));
    };
    if !pinned_image_matches(image, platform, &realized) {
        return Err(pinned_image_verification_error(
            image,
            &selected_platform,
            platform.variant.as_deref(),
            &realized,
        ));
    }
    eprintln!(
        "[docker-apps] operation=image inspect phase=after image={image} state=verified platform={selected_platform}"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PinnedImageInspection {
    repository_digests: Vec<String>,
    operating_system: String,
    architecture: String,
    variant: Option<String>,
}

fn pinned_image_matches(
    image: &str,
    platform: &OciPlatform,
    inspection: &PinnedImageInspection,
) -> bool {
    inspection
        .repository_digests
        .iter()
        .any(|digest| digest == image)
        && inspection.operating_system == platform.operating_system
        && inspection.architecture == platform.architecture
        && (inspection.variant == platform.variant
            || is_default_arm64_variant(platform, inspection))
}

/// Mirrors the product rule: a variant-free arm64 platform accepts a realized arm64 `v8` image,
/// as containerd treats `v8` as the default arm64 variant.
fn is_default_arm64_variant(platform: &OciPlatform, inspection: &PinnedImageInspection) -> bool {
    platform.architecture == "arm64"
        && platform.variant.is_none()
        && inspection.variant.as_deref() == Some(ARM64_DEFAULT_VARIANT)
}

fn pinned_image_verification_error(
    image: &str,
    selected_platform: &str,
    expected_variant: Option<&str>,
    inspection: &PinnedImageInspection,
) -> String {
    format!(
        "pinned image {image:?} verification failed: expected identity {image:?}, platform {selected_platform}, and variant {expected_variant:?}; observed repository digests {:?}, platform {}/{}, and variant {:?}",
        inspection.repository_digests,
        inspection.operating_system,
        inspection.architecture,
        inspection.variant
    )
}

fn docker_platform(platform: &OciPlatform) -> String {
    let base = format!("{}/{}", platform.operating_system, platform.architecture);
    platform
        .variant
        .as_ref()
        .map_or(base.clone(), |variant| format!("{base}/{variant}"))
}

async fn inspect_pinned_image(
    docker: &Docker,
    image: &str,
) -> Result<Option<PinnedImageInspection>, String> {
    match timeout(IMAGE_INSPECT_TIMEOUT, docker.inspect_image(image)).await {
        Ok(Ok(inspection)) => Ok(Some(PinnedImageInspection {
            repository_digests: inspection.repo_digests.unwrap_or_default(),
            operating_system: inspection.os.unwrap_or_default(),
            architecture: inspection.architecture.unwrap_or_default(),
            variant: inspection.variant,
        })),
        Ok(Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        })) => Ok(None),
        Ok(Err(error)) => Err(format!(
            "pinned image inspect failed for {image:?}: {:?}",
            bounded_text(&error.to_string(), IMAGE_DIAGNOSTIC_MAX_CHARS)
        )),
        Err(_) => Err(format!(
            "pinned image inspect timed out after {:?} for {image:?}",
            IMAGE_INSPECT_TIMEOUT
        )),
    }
}

async fn pull_pinned_image(docker: &Docker, image: &str, platform: String) -> Result<(), String> {
    let options = CreateImageOptionsBuilder::default()
        .from_image(image)
        .platform(&platform)
        .build();
    let mut stream = docker.create_image(Some(options), None, None);
    let mut latest_status = None;
    let pull = async {
        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(error_detail) = info.error_detail {
                        let diagnostic = error_detail.message.as_deref().unwrap_or("<missing>");
                        return Err(format!(
                            "pinned image pull failed for {image:?}: error_detail={:?}",
                            bounded_text(diagnostic, IMAGE_DIAGNOSTIC_MAX_CHARS)
                        ));
                    }
                    if let Some(status) = info.status {
                        latest_status = Some(bounded_text(&status, IMAGE_DIAGNOSTIC_MAX_CHARS));
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "pinned image pull failed for {image:?}; status={:?}: error={:?}",
                        latest_status.as_deref().unwrap_or("<missing>"),
                        bounded_text(&error.to_string(), IMAGE_DIAGNOSTIC_MAX_CHARS)
                    ));
                }
            }
        }
        Ok(())
    };
    match timeout(IMAGE_PULL_TIMEOUT, pull).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "pinned image pull timed out after {:?} for {image:?}; status={:?}",
            IMAGE_PULL_TIMEOUT,
            latest_status.as_deref().unwrap_or("<missing>")
        )),
    }
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

fn output_lines(output: String) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn docker_endpoint_from_ports(ports: Value) -> Result<String, String> {
    let binding = ports
        .as_object()
        .and_then(|ports| ports.values().next())
        .and_then(Value::as_array)
        .and_then(|bindings| bindings.first())
        .ok_or_else(|| "Docker inspection has no published binding".to_owned())?;
    let host = binding
        .get("HostIp")
        .and_then(Value::as_str)
        .ok_or_else(|| "Docker binding has no host".to_owned())?;
    let port = binding
        .get("HostPort")
        .and_then(Value::as_str)
        .ok_or_else(|| "Docker binding has no port".to_owned())?;
    Ok(format!("http://{host}:{port}"))
}

/// Returns the storage name the deployment attached to one binding of its `web` service.
fn attached_storage(deployment: &ApplicationDeployment, binding: &str) -> Result<String, String> {
    deployment
        .storage_attachments
        .iter()
        .find(|attachment| attachment.binding_name == binding)
        .map(|attachment| attachment.storage_name.clone())
        .ok_or_else(|| format!("application deployment has no {binding:?} storage attachment"))
}

/// Waits until the deployment's `web` container accepts `script`, then returns the container.
async fn wait_for_container_exec(
    resources: &TestResources,
    deployment_id: &str,
    script: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let attempt = resources
            .container_id_for_group(deployment_id, "web")
            .and_then(|container| container_exec(&container, script).map(|()| container));
        match attempt {
            Ok(container) => return Ok(container),
            Err(error) if Instant::now() >= deadline => {
                return Err(format!(
                    "timed out waiting for the container to run a command: {error}"
                ));
            }
            Err(_) => sleep(Duration::from_millis(250)).await,
        }
    }
}

fn container_exec(container: &str, script: &str) -> Result<(), String> {
    command("docker", ["exec", container, "sh", "-c", script]).map(|_| ())
}

/// Checks the container mounts the deployment folder at `/data` and the application folder at
/// `/data/models`, and nothing else.
fn assert_nested_models_mount(container: &str, data: &Path, models: &Path) -> Result<(), String> {
    let inspection: Value = serde_json::from_str(&command(
        "docker",
        ["inspect", "--format", "{{json .Mounts}}", container],
    )?)
    .map_err(|error| format!("decode Docker mounts: {error}"))?;
    let mut mounts = inspection
        .as_array()
        .ok_or_else(|| "Docker mounts are not an array".to_owned())?
        .iter()
        .map(|mount| {
            (
                mount["Destination"].as_str().unwrap_or_default().to_owned(),
                mount["Source"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    mounts.sort();
    let expected = vec![
        ("/data".to_owned(), data.to_string_lossy().into_owned()),
        (
            "/data/models".to_owned(),
            models.to_string_lossy().into_owned(),
        ),
    ];
    if mounts != expected {
        return Err(format!(
            "container mounts {mounts:?} differ from {expected:?}"
        ));
    }
    Ok(())
}

fn container_file(container: &str, path: &str) -> Result<String, String> {
    let output = Command::new("docker")
        .args(["exec", container, "cat", path])
        .output()
        .map_err(|error| format!("start Docker to read container file {path}: {error}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("decode container file {path}: {error}"))
    } else {
        Err(format!(
            "read container file {path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn command<const N: usize>(program: &str, arguments: [&str; N]) -> Result<String, String> {
    command_owned(
        program,
        &arguments
            .into_iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>(),
    )
}

fn command_owned(program: &str, arguments: &[String]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("start {program}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    #[tokio::test]
    async fn cold_pinned_helper_image_pulls_native_amd64_and_arm64_then_verifies() {
        for architecture in ["amd64", "arm64"] {
            let platform = linux_platform(architecture);
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut inspections =
                [None, Some(image_inspection(BUSYBOX_IMAGE, &platform))].into_iter();
            let inspect_calls = Rc::clone(&calls);
            let pull_calls = Rc::clone(&calls);

            ensure_pinned_image_with(
                BUSYBOX_IMAGE,
                &platform,
                || {
                    inspect_calls.borrow_mut().push("inspect".to_owned());
                    let inspection = inspections.next().expect("expected two image inspections");
                    async move { Ok::<Option<PinnedImageInspection>, String>(inspection) }
                },
                |selected_platform| {
                    pull_calls
                        .borrow_mut()
                        .push(format!("pull:{selected_platform}"));
                    async { Ok::<(), String>(()) }
                },
            )
            .await
            .expect("cold pinned helper image is pulled and verified");

            let expected_pull = match architecture {
                "amd64" => "pull:linux/amd64",
                "arm64" => "pull:linux/arm64",
                _ => unreachable!("test covers supported helper architectures"),
            };
            assert_eq!(&*calls.borrow(), &["inspect", expected_pull, "inspect"]);
        }
    }

    #[tokio::test]
    async fn warm_pinned_helper_image_uses_existing_amd64_and_arm64_without_pull() {
        for architecture in ["amd64", "arm64"] {
            let platform = linux_platform(architecture);
            let calls = Rc::new(RefCell::new(Vec::new()));
            let inspect_calls = Rc::clone(&calls);
            let pull_calls = Rc::clone(&calls);

            ensure_pinned_image_with(
                BUSYBOX_IMAGE,
                &platform,
                || {
                    inspect_calls.borrow_mut().push("inspect".to_owned());
                    let inspection = image_inspection(BUSYBOX_IMAGE, &platform);
                    async move { Ok::<Option<PinnedImageInspection>, String>(Some(inspection)) }
                },
                |selected_platform| {
                    pull_calls
                        .borrow_mut()
                        .push(format!("pull:{selected_platform}"));
                    async { Ok::<(), String>(()) }
                },
            )
            .await
            .expect("warm pinned helper image is verified");

            assert_eq!(&*calls.borrow(), &["inspect"]);
        }
    }

    #[tokio::test]
    async fn pinned_helper_image_rejects_wrong_realized_platform_after_native_pull() {
        let platform = linux_platform("arm64");
        let wrong_platform = linux_platform("amd64");
        let mut inspections = [
            Some(image_inspection(BUSYBOX_IMAGE, &wrong_platform)),
            Some(image_inspection(BUSYBOX_IMAGE, &wrong_platform)),
        ]
        .into_iter();

        let error = ensure_pinned_image_with(
            BUSYBOX_IMAGE,
            &platform,
            || {
                let inspection = inspections.next().expect("expected two image inspections");
                async move { Ok::<Option<PinnedImageInspection>, String>(inspection) }
            },
            |selected_platform| async move {
                assert_eq!(selected_platform, "linux/arm64");
                Ok::<(), String>(())
            },
        )
        .await
        .expect_err("foreign helper image must fail before harness setup continues");

        assert_eq!(
            error,
            format!(
                "pinned image {BUSYBOX_IMAGE:?} verification failed: expected identity {BUSYBOX_IMAGE:?}, platform linux/arm64, and variant None; observed repository digests [{BUSYBOX_IMAGE:?}], platform linux/amd64, and variant None"
            )
        );
    }

    #[tokio::test]
    async fn pinned_helper_image_rejects_identity_and_variant_mismatches() {
        let wrong_identity =
            "busybox@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let cases = [
            (
                linux_platform("amd64"),
                PinnedImageInspection {
                    repository_digests: vec![wrong_identity.to_owned()],
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                },
                "linux/amd64",
                "None",
            ),
            (
                linux_platform("amd64"),
                PinnedImageInspection {
                    repository_digests: vec![BUSYBOX_IMAGE.to_owned()],
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: Some("v8".to_owned()),
                },
                "linux/amd64",
                "None",
            ),
            (
                linux_platform("arm64"),
                PinnedImageInspection {
                    repository_digests: vec![BUSYBOX_IMAGE.to_owned()],
                    operating_system: "linux".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: Some("v9".to_owned()),
                },
                "linux/arm64",
                "None",
            ),
            (
                linux_platform("arm64"),
                PinnedImageInspection {
                    repository_digests: vec![BUSYBOX_IMAGE.to_owned()],
                    operating_system: "linux".to_owned(),
                    architecture: "arm64".to_owned(),
                    variant: Some("v7".to_owned()),
                },
                "linux/arm64",
                "None",
            ),
        ];

        for (platform, realized, expected_platform, expected_variant) in cases {
            let mut inspections = [None, Some(realized.clone())].into_iter();
            let error = ensure_pinned_image_with(
                BUSYBOX_IMAGE,
                &platform,
                || {
                    let inspection = inspections.next().expect("expected two image inspections");
                    async move { Ok::<Option<PinnedImageInspection>, String>(inspection) }
                },
                |_| async { Ok::<(), String>(()) },
            )
            .await
            .expect_err("identity and variant mismatches must fail verification");

            assert_eq!(
                error,
                format!(
                    "pinned image {BUSYBOX_IMAGE:?} verification failed: expected identity {BUSYBOX_IMAGE:?}, platform {expected_platform}, and variant {expected_variant}; observed repository digests {:?}, platform {}/{}, and variant {:?}",
                    realized.repository_digests,
                    realized.operating_system,
                    realized.architecture,
                    realized.variant
                )
            );
        }
    }

    const REMOTE_APPLICATION_IMAGE: &str = "registry.example/application@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[tokio::test]
    async fn pinned_application_image_accepts_arm64_v8_as_the_default_variant() {
        let platform = linux_platform("arm64");
        let realized = PinnedImageInspection {
            repository_digests: vec![REMOTE_APPLICATION_IMAGE.to_owned()],
            operating_system: "linux".to_owned(),
            architecture: "arm64".to_owned(),
            variant: Some(ARM64_DEFAULT_VARIANT.to_owned()),
        };
        let mut inspections = [None, Some(realized)].into_iter();

        ensure_pinned_image_with(
            REMOTE_APPLICATION_IMAGE,
            &platform,
            || {
                let inspection = inspections.next().expect("expected two image inspections");
                async move { Ok::<Option<PinnedImageInspection>, String>(inspection) }
            },
            |selected_platform| async move {
                assert_eq!(selected_platform, "linux/arm64");
                Ok::<(), String>(())
            },
        )
        .await
        .expect("arm64 v8 satisfies the variant-free arm64 platform");
    }

    #[tokio::test]
    async fn pinned_helper_image_preserves_pull_failure() {
        let platform = linux_platform("amd64");
        let expected = format!(
            "pinned image pull failed for {BUSYBOX_IMAGE:?}: registry denied the exact digest"
        );

        let error = ensure_pinned_image_with(
            BUSYBOX_IMAGE,
            &platform,
            || async { Ok::<Option<PinnedImageInspection>, String>(None) },
            |selected_platform| {
                assert_eq!(selected_platform, "linux/amd64");
                let expected = expected.clone();
                async move { Err::<(), String>(expected) }
            },
        )
        .await
        .expect_err("pull failure must stop harness setup");

        assert_eq!(error, expected);
        assert!(!error.contains(":latest"));
    }

    #[tokio::test]
    async fn pinned_helper_image_preserves_pull_cancellation() {
        let platform = linux_platform("arm64");
        let cancellation =
            format!("pinned image pull was cancelled for {BUSYBOX_IMAGE:?} on linux/arm64");

        let error = ensure_pinned_image_with(
            BUSYBOX_IMAGE,
            &platform,
            || async { Ok::<Option<PinnedImageInspection>, String>(None) },
            |_| {
                let cancellation = cancellation.clone();
                async move { Err::<(), String>(cancellation) }
            },
        )
        .await
        .expect_err("pull cancellation must stop harness setup");

        assert_eq!(error, cancellation);
    }

    fn linux_platform(architecture: &str) -> OciPlatform {
        OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: architecture.to_owned(),
            variant: None,
        }
    }

    fn image_inspection(image: &str, platform: &OciPlatform) -> PinnedImageInspection {
        let variant = match platform.architecture.as_str() {
            "amd64" => None,
            "arm64" => Some(ARM64_DEFAULT_VARIANT.to_owned()),
            _ => unreachable!("test covers supported helper architectures"),
        };
        PinnedImageInspection {
            repository_digests: vec![image.to_owned()],
            operating_system: platform.operating_system.clone(),
            architecture: platform.architecture.clone(),
            variant,
        }
    }
}
