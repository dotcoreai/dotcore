//! Pinned no-network Docker fallback for root-owned site persistence.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
};

use anyhow::Context as _;

use bollard::{
    Docker,
    models::{ContainerCreateBody, HostConfig, Mount, MountType},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
        WaitContainerOptionsBuilder,
    },
};
use dotcore_domain::{docker::DOCKER_OPERATION_TIMEOUT, real_time};
use tokio_stream::StreamExt;

pub const SITE_DATA_CLEANUP_IMAGE: &str =
    "busybox@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662";

const CLEANUP_COMMAND: &str = "rm -rf /site/* /site/.[!.]* /site/..?*";

pub async fn cleanup_root_owned_site_data(site_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_unix(
        &dotcore_domain::docker::local_endpoint(),
        dotcore_domain::docker::DOCKER_CLIENT_TIMEOUT_SECONDS,
        bollard::API_DEFAULT_VERSION,
    )
    .map_err(anyhow::Error::new)
    .map_err(|error| error.context("connect to Docker for root-owned site cleanup"))?;
    let docker = bounded("Docker API negotiation", docker.negotiate_version()).await?;
    provision_cleanup_image(&docker).await?;

    let bind_source = site_root
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Site cleanup root is not valid UTF-8"))?;
    let created = bounded(
        "root-owned site cleanup container creation",
        docker.create_container(
            Some(CreateContainerOptionsBuilder::default().build()),
            cleanup_container_body(bind_source),
        ),
    )
    .await?;
    if created.id.is_empty() {
        anyhow::bail!("root-owned site cleanup container creation returned no identity");
    }
    if let Err(error) = bounded(
        "root-owned site cleanup container start",
        docker.start_container(&created.id, None),
    )
    .await
    {
        remove_failed_cleanup_container(&docker, &created.id).await;
        return Err(error);
    }

    let options = WaitContainerOptionsBuilder::default()
        .condition("not-running")
        .build();
    let mut waiting = docker.wait_container(&created.id, Some(options));
    let outcome = real_time::bound(DOCKER_OPERATION_TIMEOUT, waiting.next())
        .await
        .map_err(|source| anyhow::Error::new(source).context("root-owned site cleanup timed out"))
        .and_then(|outcome| {
            outcome
                .ok_or_else(|| anyhow::anyhow!("root-owned site cleanup returned no exit status"))
        })
        .and_then(|outcome| {
            outcome
                .map_err(anyhow::Error::new)
                .map_err(|error| error.context("root-owned site cleanup wait failed"))
        });
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            remove_failed_cleanup_container(&docker, &created.id).await;
            return Err(error);
        }
    };
    if outcome.status_code != 0 {
        let detail = outcome
            .error
            .and_then(|error| error.message)
            .unwrap_or_else(|| "cleanup command failed".to_owned());
        anyhow::bail!(
            "root-owned site cleanup exited with status {}: {detail}",
            outcome.status_code
        );
    }
    Ok(())
}

/// Removes one folder and everything below it without following links. Workloads run as root and
/// may leave files this process cannot delete, so on failure the pinned cleanup container empties
/// the folder first. An absent folder is already removed.
pub(super) async fn remove_folder_tree(folder: &Path) -> anyhow::Result<()> {
    if remove_on_host(folder.to_owned()).await.is_ok() {
        return Ok(());
    }
    cleanup_root_owned_site_data(folder)
        .await
        .context("empty root-owned content")?;
    remove_on_host(folder.to_owned())
        .await
        .with_context(|| format!("remove {}", folder.display()))
}

async fn remove_on_host(folder: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&folder) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    })
    .await
    .map_err(io::Error::other)?
}

async fn provision_cleanup_image(docker: &Docker) -> anyhow::Result<()> {
    match bounded(
        "pinned site cleanup image inspection",
        docker.inspect_image(SITE_DATA_CLEANUP_IMAGE),
    )
    .await
    {
        Ok(_) => return Ok(()),
        Err(error) if is_not_found(&error) => {}
        Err(error) => return Err(error),
    }

    let options = CreateImageOptionsBuilder::default()
        .from_image(SITE_DATA_CLEANUP_IMAGE)
        .build();
    let mut pull = docker.create_image(Some(options), None, None);
    let pulling = async {
        while let Some(event) = pull.next().await {
            let event = event.map_err(anyhow::Error::new)?;
            if let Some(error) = event.error_detail {
                anyhow::bail!(
                    "pinned site cleanup image pull failed: {}",
                    error
                        .message
                        .unwrap_or_else(|| "Docker returned no detail".to_owned())
                );
            }
        }
        Ok(())
    };
    real_time::bound(DOCKER_OPERATION_TIMEOUT, pulling)
        .await
        .map_err(|source| {
            anyhow::Error::new(source).context("pinned site cleanup image pull timed out")
        })??;
    bounded(
        "pinned site cleanup image confirmation",
        docker.inspect_image(SITE_DATA_CLEANUP_IMAGE),
    )
    .await
    .map(|_| ())
}

fn cleanup_container_body(bind_source: &str) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(SITE_DATA_CLEANUP_IMAGE.to_owned()),
        cmd: Some(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            CLEANUP_COMMAND.to_owned(),
        ]),
        host_config: Some(HostConfig {
            auto_remove: Some(true),
            network_mode: Some("none".to_owned()),
            mounts: Some(vec![Mount {
                target: Some("/site".to_owned()),
                source: Some(bind_source.to_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Mount::default()
            }]),
            privileged: Some(false),
            publish_all_ports: Some(false),
            binds: Some(Vec::new()),
            cap_add: Some(Vec::new()),
            devices: Some(Vec::new()),
            security_opt: Some(Vec::new()),
            volumes_from: Some(Vec::new()),
            ..HostConfig::default()
        }),
        shell: Some(Vec::new()),
        ..ContainerCreateBody::default()
    }
}

async fn remove_failed_cleanup_container(docker: &Docker, container_id: &str) {
    let options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = bounded(
        "failed root-owned site cleanup container removal",
        docker.remove_container(container_id, Some(options)),
    )
    .await;
}

async fn bounded<T>(
    operation: &'static str,
    future: impl Future<Output = Result<T, bollard::errors::Error>>,
) -> anyhow::Result<T> {
    real_time::bound(DOCKER_OPERATION_TIMEOUT, future)
        .await
        .map_err(|source| anyhow::Error::new(source).context(format!("{operation} timed out")))?
        .map_err(anyhow::Error::new)
        .map_err(|error| error.context(format!("{operation} failed")))
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        matches!(
            source.downcast_ref::<bollard::errors::Error>(),
            Some(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_container_has_one_exact_bind_no_network_and_pinned_image() {
        let body = cleanup_container_body("/tmp/exact-site");
        let host = body.host_config.expect("host configuration");

        assert_eq!(body.image.as_deref(), Some(SITE_DATA_CLEANUP_IMAGE));
        assert_eq!(
            body.cmd,
            Some(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                CLEANUP_COMMAND.to_owned(),
            ])
        );
        assert_eq!(host.network_mode.as_deref(), Some("none"));
        assert_eq!(host.auto_remove, Some(true));
        assert_eq!(host.privileged, Some(false));
        assert_eq!(host.mounts.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            host.mounts.expect("exact bind mount"),
            [Mount {
                target: Some("/site".to_owned()),
                source: Some("/tmp/exact-site".to_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Mount::default()
            }]
        );
    }

    #[test]
    fn cleanup_source_has_no_image_or_cache_deletion_capability() {
        let source = include_str!("site_data.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source");

        for forbidden in ["remove_image", "prune", "list_images", "artifact_cache"] {
            assert!(!source.contains(forbidden), "cleanup contains {forbidden}");
        }
        assert!(source.contains("create_image"));
        assert!(source.contains("inspect_image"));
    }
}
