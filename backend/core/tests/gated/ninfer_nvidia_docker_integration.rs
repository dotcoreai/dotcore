//! Explicitly gated RTX 5090 Docker qualification for the master catalog Qwen3.8 27B ninfer profile.
//!
//! The suite deploys the profile on a temporary Docker site, checks the exact `ninfer-serve`
//! realization and served alias, routes one plain and one streaming chat completion, and removes
//! the deployment. The engine is the master catalog's development-source Docker image: the hosting
//! node evaluates it from the workspace, runs its loader, which builds the image on a cold host,
//! and launches the image it tags; the suite checks that pin and image, and gives preparation its
//! own deadline. It needs the `.ninfer` artifact in the shared cache and nearly the whole card, so it
//! is its own hardware class, run only during approved profile qualification. It skips on a platform the ninfer engine
//! release has no artifact for.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow};
use bollard::{Docker, query_parameters::ListContainersOptionsBuilder};
use dotcore_catalog::Catalog;
use dotcore_domain::{SitePaths, nix::DotcoreSources};
use dotcore_proto::dotcore::{
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        ListModelDeploymentsRequest, LocalModelDeploymentConfiguration, ModelDeployment,
        ModelDeploymentExposure, ModelDeploymentStatus, RemoveModelDeploymentRequest,
        ResolvedLocalModelDeploymentSnapshot, model_deployment_exposure,
        model_deployment_service_client::ModelDeploymentServiceClient, model_deployment_snapshot,
        portable_model_launch_argument_token::Value as ArgumentValue,
    },
    site::v1::Site,
};
use dotcore_runtime_backends::docker::OWNERSHIP_SCHEMA;
use nvidia_qualification::{EnginePin, QualificationSite};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tonic::{Request, transport::Channel};

use crate::{nvidia_qualification, support};

const MODEL_ID: &str = "qwen3-8-27b";
const PROFILE_ID: &str = "ninfer-nvfp4";
/// Direct route name, distinct from the model id the engine serves, so routing must map between them.
const ROUTE_NAME: &str = "qwen3-8-27b-ninfer-route";
const EXECUTION_TARGET_ID: &str = "nvidia-target";
const QUALIFICATION_NODE_ID: &str = "development-node";
const SERVER_BINARY: &str = "/usr/local/bin/ninfer-serve";
const CONTAINER_MODEL_PATH: &str = "/opt/dotcore/models/model.ninfer";
const CONTAINER_PORT: u16 = 8080;
const NVIDIA_CDI_SELECTOR: &str = "nvidia.com/gpu=0";
const LABEL_CORRELATION: &str = "com.dotcore.workload.correlation";
const LABEL_SITE: &str = "com.dotcore.site";
const LABEL_SCHEMA: &str = "com.dotcore.runtime.schema";
/// The profile defaults use about 31.5 GB of the RTX 5090's 32.6 GB.
const NVIDIA_GPU_MINIMUM_FREE_BYTES: u64 = 32_000_000_000;
/// Covers a cold read of the 23.7 GB weights file, well above the 51 s warm load.
const DEPLOYMENT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Covers a cold engine preparation, which builds the ninfer image and compiles its CUDA code.
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const QUALIFICATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

type ModelClient = ModelDeploymentServiceClient<Channel>;

#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
async fn hw_nvidia5090_ninfer_profile_qualification() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    if let Some(reason) = site.unsupported_platform_reason(MODEL_ID, PROFILE_ID)? {
        eprintln!("[ninfer-nvidia] skipped: {reason}");
        return site.cleanup().await;
    }
    let primary = qualify_on(&site).await;
    let cleanup = match tokio::time::timeout(CLEANUP_TIMEOUT, site.cleanup()).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "ninfer qualification cleanup exceeded its deadline"
        )),
    };

    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("ninfer qualification cleanup failed")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("cleanup also failed: {cleanup:#}")))
        }
    }
}

/// Launches the ninfer profile at the largest fit the capacity component computes on the idle GPU
/// and compares the memory it holds with the estimate.
#[tokio::test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
async fn hw_nvidia5090_ninfer_loads_at_the_largest_fit() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    let primary = nvidia_qualification::qualify_largest_fit(
        &site,
        &nvidia_qualification::LargestFitCase {
            suite: "largest-fit-ninfer",
            model_id: "qwen3-8-27b",
            profile_id: "ninfer-nvfp4",
            node_id: "development-node",
            target_id: "nvidia-target",
        },
    )
    .await;
    let cleanup = site.cleanup().await;
    primary.and(cleanup)
}

/// Launches the Qwen3.8 27B ninfer profile at the largest fit on a GB10, whose GPU shares host
/// memory, and compares the host memory it holds with the estimate.
#[tokio::test]
#[ignore = "hardware: just backend::test-gb10"]
async fn hw_gb10_ninfer_loads_at_the_largest_fit() -> anyhow::Result<()> {
    dotcore::logging::initialize();

    let site = QualificationSite::new("ninfer-nvidia-site").await?;
    let primary = nvidia_qualification::qualify_largest_fit(
        &site,
        &nvidia_qualification::LargestFitCase {
            suite: "gb10-largest-fit-ninfer",
            model_id: "qwen3-8-27b",
            profile_id: "ninfer-nvfp4",
            node_id: "development-node",
            target_id: "nvidia-target",
        },
    )
    .await;
    let cleanup = site.cleanup().await;
    primary.and(cleanup)
}

/// Runs the qualification on a GPU with enough free memory within its work deadline.
async fn qualify_on(site: &QualificationSite) -> anyhow::Result<()> {
    nvidia_qualification::ensure_free_gpu_memory("ninfer-nvidia", NVIDIA_GPU_MINIMUM_FREE_BYTES)?;
    let configuration = site.configuration.clone();
    let dotcore_sources = site.dotcore_sources.clone();
    let profile = profile_defaults(&site.catalog)?;
    let mut qualification = tokio::spawn(async move {
        let backend = support::RunningBackend::start_at(
            configuration.clone(),
            "127.0.0.1".parse().expect("loopback bind address"),
            dotcore_sources.clone(),
        )
        .await;
        let result = qualify_profile(&backend, &configuration, &dotcore_sources, profile).await;
        backend.stop().await;
        result
    });
    // Engine preparation is bounded by its own deadline, so the work deadline adds it to the
    // serving work's.
    match tokio::time::timeout(
        QUALIFICATION_TIMEOUT + PREPARATION_TIMEOUT,
        &mut qualification,
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(anyhow!(
            "ninfer NVIDIA qualification task panicked: {error}"
        )),
        Err(_) => {
            qualification.abort();
            let _ = qualification.await;
            Err(anyhow!(
                "ninfer NVIDIA qualification exceeded its work deadline"
            ))
        }
    }
}

async fn qualify_profile(
    backend: &support::RunningBackend,
    configuration: &SitePaths,
    dotcore_sources: &DotcoreSources,
    profile: LocalModelDeploymentConfiguration,
) -> anyhow::Result<()> {
    let mut service = backend.model_client();
    let created = create_deployment(&mut service, profile).await?;
    let deployment_id = created.model_deployment_id.clone();
    let engine = local_snapshot(&created)?
        .engine_artifact
        .as_ref()
        .context("ninfer deployment has no engine artifact")?;
    anyhow::ensure!(
        nvidia_qualification::development_source_docker_image(engine),
        "ninfer engine is not a development-source Docker-delivered Nix artifact: {engine:?}"
    );
    let pin = EnginePin::evaluate(dotcore_sources, engine).await?;
    eprintln!(
        "[ninfer-nvidia] phase=created deployment={deployment_id} store_path={} image={}",
        pin.store_path, pin.launch_reference
    );
    let preparation_started = Instant::now();
    let prepared = wait_for_preparation(&mut service, configuration, &deployment_id).await?;
    pin.check(&prepared)?;
    eprintln!(
        "[ninfer-nvidia] phase=prepared deployment={deployment_id} preparation_ms={}",
        preparation_started.elapsed().as_millis()
    );
    let deployment = wait_for_ready(&mut service, &deployment_id).await?;
    let snapshot = local_snapshot(&deployment)?;
    eprintln!("[ninfer-nvidia] phase=ready deployment={deployment_id}");

    let container = inspect_container(configuration, &deployment_id).await?;
    assert_docker_realization(&container, snapshot, &pin.launch_reference)?;
    assert_exact_alias(container.host_port).await?;

    let address = backend.inference_address();
    wait_for_route(backend, address).await?;
    assert_nonstreaming_chat(address).await?;
    assert_streaming_chat(address).await?;

    remove(&mut service, &deployment_id).await?;
    wait_for_absence(&mut service, &deployment_id).await?;
    let leaked = list_containers(
        configuration,
        vec![format!("{LABEL_CORRELATION}={deployment_id}")],
    )
    .await?;
    anyhow::ensure!(
        leaked.is_empty(),
        "removed ninfer deployment leaked Docker workloads: {leaked:?}"
    );
    eprintln!(
        "[ninfer-nvidia] phase=complete image={} argv={:?}",
        container.image, container.argv
    );
    Ok(())
}

async fn create_deployment(
    service: &mut ModelClient,
    profile: LocalModelDeploymentConfiguration,
) -> anyhow::Result<ModelDeployment> {
    service
        .create_model_deployment(Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    CreateLocalModelDeploymentRequest {
                        model_id: MODEL_ID.to_owned(),
                        profile_id: PROFILE_ID.to_owned(),
                        placement: Some(support::exact_placement(
                            QUALIFICATION_NODE_ID,
                            EXECUTION_TARGET_ID,
                        )),
                        configuration: Some(profile),
                    },
                ),
            ),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: ROUTE_NAME.to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        }))
        .await
        .context("create ninfer deployment")?
        .into_inner()
        .deployment
        .context("create ninfer deployment returned no deployment")
}

/// Selects the profile's binding and parameter defaults without overrides.
fn profile_defaults(catalog: &Catalog) -> anyhow::Result<LocalModelDeploymentConfiguration> {
    let records = catalog.protobuf_records_without_sizing();
    let profile = records
        .models
        .iter()
        .find(|model| model.model_id == MODEL_ID)
        .and_then(|model| {
            model
                .profiles
                .iter()
                .find(|profile| profile.profile_id == PROFILE_ID)
        })
        .context("Master catalog has no ninfer profile")?;
    Ok(LocalModelDeploymentConfiguration {
        model_artifact_binding_id: profile.default_model_artifact_binding_id.clone(),
        engine_parameter_values: profile.engine_parameter_defaults.clone(),
        custom_argument_tokens: Vec::new(),
        option_overrides: Vec::new(),
    })
}

fn local_snapshot(
    deployment: &ModelDeployment,
) -> anyhow::Result<&ResolvedLocalModelDeploymentSnapshot> {
    match deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    {
        Some(model_deployment_snapshot::Source::Local(local)) => Ok(local),
        _ => anyhow::bail!("ninfer deployment has no local snapshot"),
    }
}

/// Checks the container against the retained plan and pinned image: exact argv, image identity,
/// one CDI GPU, read-only weights, isolated IPC, and a loopback-only service port.
fn assert_docker_realization(
    container: &Container,
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    image: &str,
) -> anyhow::Result<()> {
    let expected = snapshot
        .desired_launch_plan
        .as_ref()
        .context("ninfer deployment has no launch plan")?
        .arguments
        .iter()
        .map(|argument| match argument.value.as_ref() {
            Some(ArgumentValue::Literal(value)) => Ok(value.clone()),
            Some(ArgumentValue::ArtifactBindingId(id)) if id == "model" => {
                Ok(CONTAINER_MODEL_PATH.to_owned())
            }
            _ => Err(anyhow!("unexpected ninfer plan token")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(
        container.entrypoint == [SERVER_BINARY] && container.argv == expected[1..],
        "ninfer container command differs from the plan: entrypoint={:?} argv={:?} expected={expected:?}",
        container.entrypoint,
        container.argv
    );
    for pair in [["--model-id", MODEL_ID], ["--device", "0"]] {
        anyhow::ensure!(
            container.argv.windows(2).any(|window| window == pair),
            "ninfer argv lacks {pair:?}: {:?}",
            container.argv
        );
    }
    anyhow::ensure!(
        container.image == image,
        "ninfer container image {} is not the loader's image {image}",
        container.image
    );
    anyhow::ensure!(
        container.device_driver.as_deref() == Some("cdi")
            && container.device_ids == [NVIDIA_CDI_SELECTOR],
        "ninfer device request differs: driver={:?} devices={:?}",
        container.device_driver,
        container.device_ids
    );
    anyhow::ensure!(
        container.mounts == [(CONTAINER_MODEL_PATH.to_owned(), true)],
        "ninfer mounts differ from one read-only weights file: {:?}",
        container.mounts
    );
    anyhow::ensure!(
        container.ipc_mode.as_deref() != Some("host") && !container.privileged,
        "ninfer container is not isolated: ipc={:?} privileged={}",
        container.ipc_mode,
        container.privileged
    );
    anyhow::ensure!(
        container.host_ip.as_deref() == Some("127.0.0.1"),
        "ninfer host port is not loopback-only: {:?}",
        container.host_ip
    );
    Ok(())
}

#[derive(Debug)]
struct Container {
    image: String,
    entrypoint: Vec<String>,
    argv: Vec<String>,
    mounts: Vec<(String, bool)>,
    ipc_mode: Option<String>,
    privileged: bool,
    device_driver: Option<String>,
    device_ids: Vec<String>,
    host_ip: Option<String>,
    host_port: u16,
}

async fn inspect_container(
    configuration: &SitePaths,
    deployment_id: &str,
) -> anyhow::Result<Container> {
    let ids = list_containers(
        configuration,
        vec![format!("{LABEL_CORRELATION}={deployment_id}")],
    )
    .await?;
    let [id] = ids.as_slice() else {
        anyhow::bail!("expected exactly one ninfer Docker workload, found {ids:?}");
    };
    let inspection = Docker::connect_with_local_defaults()
        .context("connect Docker for ninfer inspection")?
        .inspect_container(id, None)
        .await
        .context("inspect ninfer Docker workload")?;
    let config = inspection.config.context("inspection has no Config")?;
    let host = inspection
        .host_config
        .context("inspection has no HostConfig")?;
    let request = host
        .device_requests
        .as_deref()
        .and_then(|requests| requests.first())
        .context("inspection has no accelerator device request")?;
    let binding = inspection
        .network_settings
        .as_ref()
        .and_then(|settings| settings.ports.as_ref())
        .and_then(|ports| ports.get(&format!("{CONTAINER_PORT}/tcp")))
        .and_then(Option::as_ref)
        .and_then(|bindings| bindings.first())
        .context("inspection has no service port binding")?;
    Ok(Container {
        image: config.image.unwrap_or_default(),
        entrypoint: config.entrypoint.unwrap_or_default(),
        argv: config.cmd.unwrap_or_default(),
        mounts: inspection
            .mounts
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mount| Some((mount.destination?, mount.rw == Some(false))))
            .collect(),
        ipc_mode: host.ipc_mode,
        privileged: host.privileged.unwrap_or(false),
        device_driver: request.driver.clone(),
        device_ids: request.device_ids.clone().unwrap_or_default(),
        host_ip: binding.host_ip.clone(),
        host_port: binding
            .host_port
            .as_deref()
            .context("service port binding has no host port")?
            .parse()
            .context("service host port is malformed")?,
    })
}

/// Lists this site's managed containers matching the extra labels.
async fn list_containers(
    configuration: &SitePaths,
    labels: Vec<String>,
) -> anyhow::Result<Vec<String>> {
    let site: Site = dotcore_proto::yaml::from_file(&configuration.root.join("site.yaml"))?;
    let mut filter = vec![
        format!("{LABEL_SITE}={}", site.site_id),
        format!("{LABEL_SCHEMA}={OWNERSHIP_SCHEMA}"),
    ];
    filter.extend(labels);
    let filters = HashMap::from([("label", filter)]);
    Ok(Docker::connect_with_local_defaults()
        .context("connect Docker")?
        .list_containers(Some(
            ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .context("list ninfer Docker workloads")?
        .into_iter()
        .filter_map(|container| container.id)
        .collect())
}

/// Waits until the hosting node prepared the engine and returns the deployment. Fails on a failed
/// deployment or once the preparation deadline passes.
async fn wait_for_preparation(
    service: &mut ModelClient,
    configuration: &SitePaths,
    deployment_id: &str,
) -> anyhow::Result<ModelDeployment> {
    tokio::time::timeout(PREPARATION_TIMEOUT, async {
        loop {
            let deployment = find_deployment(service, deployment_id)
                .await?
                .context("ninfer deployment disappeared")?;
            let state = deployment
                .state
                .as_ref()
                .context("ninfer deployment has no state")?;
            if nvidia_qualification::engine_prepared(configuration, &deployment).await? {
                return Ok(deployment);
            }
            anyhow::ensure!(
                state.status != ModelDeploymentStatus::Failed as i32,
                "ninfer deployment failed during preparation: {:?}",
                state.failure
            );
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("ninfer engine preparation exceeded its deadline")?
}

/// Waits for Ready while the deployment is still starting and returns the Ready deployment. Fails
/// on any other status, such as Failed or an Unavailable workload that exited during load.
async fn wait_for_ready(
    service: &mut ModelClient,
    deployment_id: &str,
) -> anyhow::Result<ModelDeployment> {
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            let deployment = find_deployment(service, deployment_id)
                .await?
                .context("ninfer deployment disappeared")?;
            let state = deployment
                .state
                .as_ref()
                .context("ninfer deployment has no state")?;
            match ModelDeploymentStatus::try_from(state.status) {
                Ok(ModelDeploymentStatus::Ready) => return Ok(deployment),
                Ok(
                    ModelDeploymentStatus::Unspecified
                    | ModelDeploymentStatus::Queued
                    | ModelDeploymentStatus::Preparing
                    | ModelDeploymentStatus::Downloading
                    | ModelDeploymentStatus::Loading
                    | ModelDeploymentStatus::WarmingUp,
                ) => {}
                status => anyhow::bail!(
                    "ninfer deployment left startup as {status:?}: {:?}",
                    state.failure
                ),
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("ninfer deployment never reached Ready")?
}

async fn wait_for_absence(service: &mut ModelClient, deployment_id: &str) -> anyhow::Result<()> {
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        while find_deployment(service, deployment_id).await?.is_some() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Ok(())
    })
    .await
    .context("removed ninfer deployment remained in controller state")?
}

async fn find_deployment(
    service: &mut ModelClient,
    deployment_id: &str,
) -> anyhow::Result<Option<ModelDeployment>> {
    Ok(service
        .list_model_deployments(Request::new(ListModelDeploymentsRequest {}))
        .await
        .context("read ninfer deployments")?
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id))
}

async fn remove(service: &mut ModelClient, deployment_id: &str) -> anyhow::Result<()> {
    service
        .remove_model_deployment(Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.to_owned(),
        }))
        .await
        .context("remove ninfer deployment")?;
    Ok(())
}

async fn wait_for_route(
    backend: &support::RunningBackend,
    address: std::net::SocketAddr,
) -> anyhow::Result<()> {
    tokio::time::timeout(DEPLOYMENT_TIMEOUT, async {
        loop {
            anyhow::ensure!(
                !backend.is_finished(),
                "routed backend stopped before exposing ninfer"
            );
            if let Ok(response) = raw_request(
                address,
                b"GET /v1/models HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
            )
            .await
                && response
                    .windows(ROUTE_NAME.len())
                    .any(|part| part == ROUTE_NAME.as_bytes())
            {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .context("route never exposed the ready ninfer deployment")?
}

async fn assert_exact_alias(port: u16) -> anyhow::Result<()> {
    let value = response_json(
        &raw_request(
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            b"GET /v1/models HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n",
        )
        .await?,
    )?;
    let ids = value
        .get("data")
        .and_then(Value::as_array)
        .context("ninfer /v1/models has no data array")?
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        ids == [MODEL_ID],
        "ninfer served the wrong alias: expected only {MODEL_ID:?}, found {ids:?}"
    );
    Ok(())
}

async fn assert_nonstreaming_chat(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let response = response_json(&post(address, false).await?)?;
    anyhow::ensure!(
        response.get("model").and_then(Value::as_str) == Some(ROUTE_NAME),
        "ninfer Chat response does not echo the route name: {response}"
    );
    let message = response
        .pointer("/choices/0/message")
        .context("ninfer Chat response has no assistant message")?;
    anyhow::ensure!(
        message
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            == Some("42"),
        "ninfer Chat returned unexpected content: {response}"
    );
    anyhow::ensure!(
        message
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|reasoning| !reasoning.trim().is_empty()),
        "ninfer Chat returned no separate reasoning: {response}"
    );
    Ok(())
}

async fn assert_streaming_chat(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let response = post(address, true).await?;
    let text = String::from_utf8_lossy(&response);
    anyhow::ensure!(
        text.starts_with("HTTP/1.1 200") && text.contains("data: [DONE]"),
        "ninfer streaming Chat did not complete: {}",
        text.chars().take(1_024).collect::<String>()
    );
    let content = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .context("decode streaming Chat frames")?
        .iter()
        .filter_map(|frame| {
            frame
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
        })
        .collect::<String>();
    anyhow::ensure!(
        content.trim() == "42",
        "ninfer streaming Chat returned unexpected content {content:?}"
    );
    Ok(())
}

async fn post(address: std::net::SocketAddr, stream: bool) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(&serde_json::json!({
        "model": ROUTE_NAME,
        "messages": [{"role":"user","content":"Compute 17 + 25. Reply with only the number."}],
        "temperature": 0,
        "max_completion_tokens": 4096,
        "stream": stream
    }))?;
    let mut request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend(body);
    raw_request(address, &request).await
}

async fn raw_request(address: std::net::SocketAddr, request: &[u8]) -> anyhow::Result<Vec<u8>> {
    tokio::time::timeout(HTTP_TIMEOUT, async {
        let mut stream = TcpStream::connect(address)
            .await
            .context("connect inference HTTP")?;
        stream
            .write_all(request)
            .await
            .context("write inference HTTP request")?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .context("read inference HTTP response")?;
        Ok::<_, anyhow::Error>(response)
    })
    .await
    .context("inference HTTP request timed out")?
}

fn response_json(response: &[u8]) -> anyhow::Result<Value> {
    let header_end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("response has no complete HTTP headers")?;
    anyhow::ensure!(
        response.starts_with(b"HTTP/1.1 200"),
        "request failed: {}",
        String::from_utf8_lossy(response)
            .chars()
            .take(1_024)
            .collect::<String>()
    );
    serde_json::from_slice(&response[header_end + 4..]).context("decode JSON response")
}
