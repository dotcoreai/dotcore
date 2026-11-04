//! Build script for the generated protobuf bindings that back every gRPC
//! call between the dotc client and the server.
//!
//! It compiles the repository's .proto files with the Nix-provided compiler
//! into message types plus gRPC client and server code, adds JSON
//! serialization to every message, and fails the build when the Nix-provided
//! compiler or its include directory is missing.

use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
};

const PROTO_FILES: &[&str] = &[
    "../../proto/dotcore/application/private/v1/controller.proto",
    "../../proto/dotcore/application/v1/deployment.proto",
    "../../proto/dotcore/application/v1/service.proto",
    "../../proto/dotcore/application/v1/state.proto",
    "../../proto/dotcore/capacity/v1/service.proto",
    "../../proto/dotcore/catalog/v1/application.proto",
    "../../proto/dotcore/deployment/private/v1/logs.proto",
    "../../proto/dotcore/deployment/v1/artifact.proto",
    "../../proto/dotcore/deployment/v1/placement.proto",
    "../../proto/dotcore/diagnostic/private/v1/service.proto",
    "../../proto/dotcore/diagnostic/v1/service.proto",
    "../../proto/dotcore/catalog/v1/engine_release.proto",
    "../../proto/dotcore/catalog/v1/model_capabilities.proto",
    "../../proto/dotcore/catalog/v1/execution.proto",
    "../../proto/dotcore/catalog/v1/model.proto",
    "../../proto/dotcore/catalog/v1/service.proto",
    "../../proto/dotcore/engine/llama_cpp/v1/model.proto",
    "../../proto/dotcore/engine/ninfer/v1/model.proto",
    "../../proto/dotcore/engine/vllm/v1/model.proto",
    "../../proto/dotcore/engine/v1/engine.proto",
    "../../proto/dotcore/inference/private/v1/peer.proto",
    "../../proto/dotcore/inference/v1/chat.proto",
    "../../proto/dotcore/inference/v1/models.proto",
    "../../proto/dotcore/inference/v1/routes.proto",
    "../../proto/dotcore/launcher/store/v1/workload.proto",
    "../../proto/dotcore/metrics/private/v1/bucket_stream.proto",
    "../../proto/dotcore/metrics/v1/metric.proto",
    "../../proto/dotcore/metrics/v1/service.proto",
    "../../proto/dotcore/model/v1/state.proto",
    "../../proto/dotcore/model/v1/deployment.proto",
    "../../proto/dotcore/model/v1/service.proto",
    "../../proto/dotcore/model/private/v1/controller.proto",
    "../../proto/dotcore/runtime/store/v1/native.proto",
    "../../proto/dotcore/site/v1/site.proto",
    "../../proto/dotcore/site/v1/service.proto",
    "../../proto/dotcore/site/private/v1/desired_view.proto",
    "../../proto/dotcore/site/private/v1/node_control.proto",
    "../../proto/dotcore/site/store/v1/snapshot.proto",
    "../../proto/dotcore/node/v1/config.proto",
    "../../proto/dotcore/node/v1/join.proto",
    "../../proto/dotcore/observation/v1/observation.proto",
    "../../proto/dotcore/telemetry/v1/workload_resource.proto",
    "../../proto/dotcore/watch/v1/service.proto",
];

fn main() -> Result<(), Box<dyn Error>> {
    // The listed files import others, so watch the whole contract tree rather than the roots.
    println!("cargo:rerun-if-changed=../../proto/dotcore");
    println!("cargo:rerun-if-env-changed=PROTOC");
    println!("cargo:rerun-if-env-changed=PROTOC_INCLUDE");

    let protoc = env::var("PROTOC").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "PROTOC must reference the Nix-provided protoc executable",
        )
    })?;
    let protoc_include = env::var("PROTOC_INCLUDE").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "PROTOC_INCLUDE must reference the Nix-provided protobuf include directory",
        )
    })?;

    if !Path::new(&protoc).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "PROTOC does not reference an executable file",
        )
        .into());
    }
    if !Path::new(&protoc_include).is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "PROTOC_INCLUDE does not reference a directory",
        )
        .into());
    }

    let descriptor_path = PathBuf::from(env::var("OUT_DIR")?).join("dotcore-descriptor.bin");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_well_known_types(true)
        .type_attribute(
            ".dotcore.model.v1.ModelDeploymentSnapshot.source",
            "#[allow(clippy::large_enum_variant)]",
        )
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(PROTO_FILES, &["../../proto", &protoc_include])?;
    pbjson_build::Builder::new()
        .register_descriptors(&fs::read(descriptor_path)?)?
        .build(&[
            ".dotcore.application.private.v1",
            ".dotcore.application.v1",
            ".dotcore.capacity.v1",
            ".dotcore.catalog.v1",
            ".dotcore.deployment.private.v1",
            ".dotcore.deployment.v1",
            ".dotcore.diagnostic.private.v1",
            ".dotcore.diagnostic.v1",
            ".dotcore.engine.llama_cpp.v1",
            ".dotcore.engine.ninfer.v1",
            ".dotcore.engine.vllm.v1",
            ".dotcore.engine.v1",
            ".dotcore.inference.private.v1",
            ".dotcore.inference.v1",
            ".dotcore.launcher.store.v1",
            ".dotcore.metrics.private.v1",
            ".dotcore.metrics.v1",
            ".dotcore.model.private.v1",
            ".dotcore.model.v1",
            ".dotcore.node.v1",
            ".dotcore.observation.v1",
            ".dotcore.runtime.store.v1",
            ".dotcore.site.private.v1",
            ".dotcore.site.v1",
            ".dotcore.site.store.v1",
            ".dotcore.telemetry.v1",
            ".dotcore.watch.v1",
            ".google.protobuf",
        ])?;

    Ok(())
}
