/**
 * Generated catalog, model, and site descriptor contract tests.
 *
 * These tests pin cross-domain service/message shapes consumed by the console;
 * breakage changes generated-client compatibility and declaration/current-state
 * interpretation. Runtime behavior, server transport, and visual feature
 * behavior are intentionally excluded and covered by API and feature suites.
 */
import { create, fromJson, toJson } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { file_dotcore_application_v1_service } from "@/gen/dotcore/application/v1/service_pb";
import {
  file_dotcore_catalog_v1_execution,
  ModelParallelShape,
} from "@/gen/dotcore/catalog/v1/execution_pb";
import { file_dotcore_catalog_v1_model } from "@/gen/dotcore/catalog/v1/model_pb";
import { file_dotcore_catalog_v1_service } from "@/gen/dotcore/catalog/v1/service_pb";
import { file_dotcore_catalog_v1_upstream_model_template } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import { file_dotcore_deployment_v1_accelerator_memory } from "@/gen/dotcore/deployment/v1/accelerator_memory_pb";
import { file_dotcore_deployment_v1_memory_fit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import { file_dotcore_model_v1_deployment } from "@/gen/dotcore/model/v1/deployment_pb";
import {
  file_dotcore_model_v1_service,
  UpdateModelDeploymentMetadataRequestSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import { file_dotcore_model_v1_state } from "@/gen/dotcore/model/v1/state_pb";
import { file_dotcore_observation_v1_observation } from "@/gen/dotcore/observation/v1/observation_pb";
import { file_dotcore_site_store_v1_snapshot } from "@/gen/dotcore/site/store/v1/snapshot_pb";
import { file_dotcore_site_v1_service } from "@/gen/dotcore/site/v1/service_pb";
import { file_dotcore_site_v1_site } from "@/gen/dotcore/site/v1/site_pb";

describe("separate catalog and model read contracts", () => {
  it("keeps current catalog releases separate from deployment state", () => {
    expect(file_dotcore_catalog_v1_service.services[0]?.methods.map(({ name }) => name)).toEqual([
      "ListModels",
      "ListEngineReleases",
      "ListApplications",
      "ListUpstreamModelTemplates",
      "ListHardwareClasses",
    ]);
    expect(
      file_dotcore_application_v1_service.services[0]?.methods.map(({ name }) => name),
    ).toEqual([
      "ListApplicationDeployments",
      "GetApplicationDeployment",
      "CreateApplicationDeployment",
      "RemoveApplicationDeployment",
      "PauseDeployment",
      "ResumeDeployment",
      "ReadDeploymentLogs",
      "FollowDeploymentLogs",
      "UpdateApplicationDeploymentMetadata",
    ]);
    expect(
      fieldNumbers(
        message(file_dotcore_application_v1_service, "UpdateApplicationDeploymentMetadataRequest"),
      ),
    ).toEqual([
      ["application_deployment_id", 1],
      ["metadata", 2],
      ["update_mask", 3],
    ]);
    expect(
      fieldNames(message(file_dotcore_application_v1_service, "ApplicationDeploymentMetadata")),
    ).toEqual(["display_name"]);
    expect(
      fieldNumbers(message(file_dotcore_application_v1_service, "ApplicationDeploymentMetadata")),
    ).toEqual([["display_name", 1]]);
    expect(
      fieldNumbers(
        message(file_dotcore_application_v1_service, "UpdateApplicationDeploymentMetadataResponse"),
      ),
    ).toEqual([["deployment", 1]]);
    expect(file_dotcore_application_v1_service.messages.map(({ name }) => name)).not.toContain(
      "RenameApplicationDeploymentRequest",
    );
    expect(file_dotcore_application_v1_service.messages.map(({ name }) => name)).not.toContain(
      "RenameApplicationDeploymentResponse",
    );
    expect(
      file_dotcore_application_v1_service.services[0]?.methods.map(({ name }) => name),
    ).not.toContain("RenameApplicationDeployment");
    expect(file_dotcore_model_v1_service.services[0]?.methods.map(({ name }) => name)).toEqual([
      "ListModelDeployments",
      "CreateModelDeployment",
      "PreviewLocalModelDeployment",
      "EstimateLocalModelResources",
      "ListModelProfileCompatibility",
      "ListModelRecommendations",
      "UpdateModelDeployment",
      "RemoveModelDeployment",
      "PauseModel",
      "ResumeModel",
      "ReadDeploymentLogs",
      "FollowDeploymentLogs",
      "UpdateModelDeploymentMetadata",
    ]);
    expect(
      method(file_dotcore_application_v1_service, "ReadDeploymentLogs").proto.serverStreaming,
    ).toBe(false);
    expect(
      method(file_dotcore_application_v1_service, "FollowDeploymentLogs").proto.serverStreaming,
    ).toBe(true);
    expect(method(file_dotcore_model_v1_service, "ReadDeploymentLogs").proto.serverStreaming).toBe(
      false,
    );
    expect(
      method(file_dotcore_model_v1_service, "FollowDeploymentLogs").proto.serverStreaming,
    ).toBe(true);
    expect(file_dotcore_model_v1_service.messages.map(({ name }) => name)).toEqual([
      "ListModelDeploymentsRequest",
      "ListModelDeploymentsResponse",
      "CreateModelDeploymentRequest",
      "CreateLocalModelDeploymentRequest",
      "LocalModelDeploymentConfiguration",
      "CreateUpstreamModelDeploymentRequest",
      "ModelDeploymentExposure",
      "DirectRouteExposure",
      "HiddenDeploymentExposure",
      "CreateModelDeploymentResponse",
      "PreviewLocalModelDeploymentRequest",
      "PreviewLocalModelDeploymentResponse",
      "EstimateLocalModelResourcesRequest",
      "EstimateLocalModelResourcesResponse",
      "HardwareClassResourceEstimate",
      "ScalarResourceEstimate",
      "ListModelProfileCompatibilityRequest",
      "SizingScenario",
      "ListModelProfileCompatibilityResponse",
      "ListModelRecommendationsRequest",
      "ListModelRecommendationsResponse",
      "UseCaseModelRecommendations",
      "RecommendedModelProfile",
      "RecommendedProfileTarget",
      "TargetCompatibility",
      "ModelProfileCompatibility",
      "ModelProfileSizing",
      "ModelCompatibilityReason",
      "UpdateModelDeploymentRequest",
      "UpdateModelDeploymentResponse",
      "RemoveModelDeploymentRequest",
      "RemoveModelDeploymentResponse",
      "PauseModelRequest",
      "PauseModelResponse",
      "ResumeModelRequest",
      "ResumeModelResponse",
      "ReadDeploymentLogsRequest",
      "ReadDeploymentLogsResponse",
      "FollowDeploymentLogsRequest",
      "FollowDeploymentLogsResponse",
      "DeploymentLogSource",
      "DeploymentLogEntry",
      "DeploymentLogResumePosition",
      "DeploymentLogSourceEvent",
      "UpdateModelDeploymentMetadataRequest",
      "ModelDeploymentMetadata",
      "UpdateModelDeploymentMetadataResponse",
    ]);
    expect(file_dotcore_model_v1_service.enums.map(({ name }) => name)).toEqual([
      "LocalModelDeploymentAdvisory",
      "ModelRecommendationFit",
      "ModelCompatibilityStatus",
      "ModelCompatibilityReasonCode",
      "SizingScenarioUnsupportedReason",
      "StartingTotalContextSource",
      "DeploymentLogHistoryState",
      "DeploymentLogSourceState",
      "DeploymentLogStream",
    ]);
    expect(file_dotcore_application_v1_service.messages.map(({ name }) => name).slice(-11)).toEqual(
      [
        "ReadDeploymentLogsRequest",
        "ReadDeploymentLogsResponse",
        "FollowDeploymentLogsRequest",
        "FollowDeploymentLogsResponse",
        "DeploymentLogSource",
        "DeploymentLogEntry",
        "DeploymentLogResumePosition",
        "DeploymentLogSourceEvent",
        "UpdateApplicationDeploymentMetadataRequest",
        "ApplicationDeploymentMetadata",
        "UpdateApplicationDeploymentMetadataResponse",
      ],
    );
    expect(file_dotcore_application_v1_service.enums.map(({ name }) => name)).toEqual([
      "DeploymentLogHistoryState",
      "DeploymentLogSourceState",
      "DeploymentLogStream",
    ]);
    expect(file_dotcore_site_v1_service.services[0]?.methods.map(({ name }) => name)).toEqual([
      "ListMachines",
      "ListConfiguredExecutionTargets",
      "ListHardwareSnapshots",
    ]);
    expect(file_dotcore_site_v1_service.messages.map(({ name }) => name)).toEqual([
      "ListMachinesRequest",
      "ListMachinesResponse",
      "ListConfiguredExecutionTargetsRequest",
      "ListConfiguredExecutionTargetsResponse",
      "ConfiguredExecutionTarget",
      "ExecutionTargetAvailability",
      "RuntimeTargetAvailability",
      "ListHardwareSnapshotsRequest",
      "ListHardwareSnapshotsResponse",
      "MachineHardwareSnapshot",
      "HardwareRefreshState",
      "Machine",
      "NodeObservationFreshness",
      "CurrentHardwareSnapshotAvailability",
    ]);
    expect(file_dotcore_site_v1_service.enums.map(({ name }) => name)).toEqual([
      "AcceleratorResolutionStatus",
      "ExecutionTargetWorkloadKind",
      "NodeObservationState",
    ]);
    expect(file_dotcore_observation_v1_observation.services).toEqual([]);
    expect(file_dotcore_observation_v1_observation.messages.map(({ name }) => name)).toEqual([
      "HardwareSnapshot",
      "RuntimeObservation",
      "HostObservation",
      "HostFacts",
      "FilesystemFacts",
      "AcceleratorObservation",
      "AcceleratorFactsSet",
      "AcceleratorFacts",
      "ObservationUnavailable",
    ]);
    expect(file_dotcore_observation_v1_observation.enums.map(({ name }) => name)).toEqual([
      "RuntimeKind",
      "AcceleratorSignal",
      "AcceleratorMemoryArchitecture",
      "AcceleratorVendor",
    ]);
    expect(file_dotcore_deployment_v1_accelerator_memory.messages.map(({ name }) => name)).toEqual([
      "AcceleratorMemoryObservation",
      "AcceleratorMemoryRefreshState",
      "AcceleratorMemoryUsageSet",
      "AcceleratorMemoryDeviceUsage",
      "AcceleratorMemoryUnavailable",
    ]);
    expect(file_dotcore_deployment_v1_memory_fit.enums.map(({ name }) => name)).toEqual([
      "MemoryFit",
    ]);
    expect(file_dotcore_model_v1_deployment.messages.map(({ name }) => name).sort()).toEqual([
      "ModelDeployment",
      "ModelDeploymentDeclaration",
      "ModelDeploymentIntent",
      "ModelDeploymentSnapshot",
      "OpenAiCompatibleEndpointConfiguration",
      "PortableModelArtifactBinding",
      "PortableModelLaunchArgumentToken",
      "PortableModelLaunchPlan",
      "ResolvedEngineArtifact",
      "ResolvedHardwareSupport",
      "ResolvedLocalModelDeploymentSnapshot",
      "ResolvedModelOption",
      "ResolvedSelectedTarget",
      "ResolvedSizing",
      "UpstreamModelDeploymentSnapshot",
      "UpstreamModelDesiredConfiguration",
      "UpstreamModelInputCapabilities",
    ]);
    expect(
      fieldNames(message(file_dotcore_catalog_v1_upstream_model_template, "UpstreamModelTemplate")),
    ).toEqual(["upstream_model_template_id", "display_name", "wire_protocol"]);
    expect(fieldNames(message(file_dotcore_catalog_v1_model, "Model"))).toEqual([
      "model_id",
      "display_name",
      "version",
      "maximum_context_tokens",
      "artifacts",
      "profiles",
      "auxiliary_artifacts",
      "configurations",
      "options",
      "intelligence",
      "license",
    ]);
    expect(fieldNames(message(file_dotcore_catalog_v1_model, "IntelligenceScore"))).toEqual([
      "overall",
    ]);
    expect(fieldNames(message(file_dotcore_catalog_v1_model, "License"))).toEqual([
      "id",
      "terms_url",
    ]);
    expect(fieldNames(message(file_dotcore_catalog_v1_model, "ModelEngineConfiguration"))).toEqual([
      "configuration_id",
      "engine_artifacts",
      "artifact_bindings",
      "hardware_support",
      "engine_configuration",
      "execution_capabilities",
      "input_capabilities",
      "request_kinds",
      "options",
      "maximum_context_tokens",
    ]);
    expect(file_dotcore_catalog_v1_execution.enums.map(({ name }) => name)).toEqual([
      "ModelParallelShape",
    ]);
    const remoteEndpointContract = JSON.stringify(
      file_dotcore_catalog_v1_upstream_model_template.proto,
    );
    expect(remoteEndpointContract).not.toContain("Health");
    expect(fieldNames(message(file_dotcore_model_v1_deployment, "ModelDeployment"))).toEqual([
      "model_deployment_id",
      "intent",
      "snapshot",
      "state",
      "desired_state",
      "upstream_desired_configuration",
      "model_deployment_name",
      "accelerator_memory",
      "node_resolved_launch_identities",
    ]);
    expect(
      fieldNames(message(file_dotcore_model_v1_deployment, "ModelDeploymentSnapshot")),
    ).toEqual(["local", "upstream"]);
    expect(
      fieldNames(message(file_dotcore_model_v1_deployment, "UpstreamModelDeploymentSnapshot")),
    ).toEqual(["upstream_model_template"]);
    expect(
      fieldNames(message(file_dotcore_model_v1_service, "CreateModelDeploymentRequest")),
    ).toEqual(["local", "upstream", "exposure", "model_deployment_name"]);
    expect(
      fieldNames(message(file_dotcore_model_v1_service, "CreateModelDeploymentResponse")),
    ).toEqual(["deployment"]);
    expect(
      fieldNames(message(file_dotcore_model_v1_service, "UpdateModelDeploymentRequest")),
    ).toEqual(["model_deployment_id", "upstream_desired_configuration"]);
    expectLogContract(file_dotcore_model_v1_service, "model_deployment_id");
    expectLogContract(file_dotcore_application_v1_service, "application_deployment_id");
    expect(
      fieldNumbers(message(file_dotcore_model_v1_service, "UpdateModelDeploymentMetadataRequest")),
    ).toEqual([
      ["model_deployment_id", 1],
      ["metadata", 2],
      ["update_mask", 3],
    ]);
    expect(fieldNames(message(file_dotcore_model_v1_service, "ModelDeploymentMetadata"))).toEqual([
      "display_name",
    ]);
    expect(fieldNumbers(message(file_dotcore_model_v1_service, "ModelDeploymentMetadata"))).toEqual(
      [["display_name", 1]],
    );
    expect(
      fieldNumbers(message(file_dotcore_model_v1_service, "UpdateModelDeploymentMetadataResponse")),
    ).toEqual([["deployment", 1]]);
    expect(file_dotcore_model_v1_service.messages.map(({ name }) => name)).not.toContain(
      "RenameModelDeploymentRequest",
    );
    expect(file_dotcore_model_v1_service.messages.map(({ name }) => name)).not.toContain(
      "RenameModelDeploymentResponse",
    );
    expect(
      file_dotcore_model_v1_service.services[0]?.methods.map(({ name }) => name),
    ).not.toContain("RenameModelDeployment");
    expect(
      JSON.stringify([file_dotcore_catalog_v1_model.proto, file_dotcore_model_v1_deployment.proto]),
    ).not.toContain("origin");
    expect(JSON.stringify(file_dotcore_model_v1_service.proto)).not.toContain("Projection");
  });
});

describe("desired site and current-state contracts", () => {
  it("keeps site configuration declarative and current state operational", () => {
    const site = message(file_dotcore_site_v1_site, "Site");
    const executionTarget = message(file_dotcore_site_v1_site, "ExecutionTarget");
    const siteNode = message(file_dotcore_site_v1_site, "SiteNode");
    const deployment = message(file_dotcore_model_v1_deployment, "ModelDeploymentDeclaration");
    const profile = message(file_dotcore_catalog_v1_model, "ModelProfile");
    const currentDeployment = message(file_dotcore_model_v1_state, "CurrentModelDeployment");
    const currentState = message(file_dotcore_site_store_v1_snapshot, "SiteCurrentState");

    expect(fieldNames(site)).toEqual([
      "site_id",
      "execution_targets",
      "model_deployments",
      "nodes",
      "application_deployments",
      "inference_routes",
      "hardware_observation",
      "model_deployment_identity_allocator",
    ]);
    expect(fieldNames(executionTarget)).toEqual([
      "execution_target_id",
      "site_node_id",
      "display_name",
      "platform",
      "accelerator_device_ids",
      "model_runtime",
      "application_runtime",
    ]);
    expect(executionTarget.proto.oneofDecl.map(({ name }) => name)).toEqual(["runtime_binding"]);
    expect(fieldNames(siteNode)).toEqual([
      "node_id",
      "display_name",
      "site_manager",
      "host",
      "manager_host",
    ]);
    expect(fieldNames(deployment)).toEqual([
      "model_deployment_id",
      "placement",
      "requested_parallel_shape",
      "snapshot",
      "desired_state",
      "upstream_desired_configuration",
      "model_deployment_name",
      "lifecycle_generation",
      "lifecycle_transition",
      "lifecycle_recovery_required",
      "node_resolved_launch_identities",
    ]);
    expect(fieldNames(profile)).toEqual([
      "profile_id",
      "configuration_id",
      "default_model_artifact_binding_id",
      "engine_parameter_defaults",
      "display_name",
      "description",
      "option_defaults",
      "exposed_parameter_ids",
    ]);
    expect(ModelParallelShape.ONE).toBe(1);

    expect(fieldNames(currentState)).toEqual([
      "format_version",
      "site_id",
      "storage_revision",
      "model_deployments",
      "application_deployments",
      "manager_model_lifecycle_records",
      "manager_application_lifecycle_records",
    ]);
    expect(fieldNames(currentDeployment)).toEqual([
      "model_deployment_id",
      "record_revision",
      "state",
      "reconciled_lifecycle_generation",
      "recovery_barrier",
    ]);

    const desiredContract = JSON.stringify([
      site.proto,
      executionTarget.proto,
      deployment.proto,
    ]).toLowerCase();
    expect(desiredContract).not.toContain("origin");
    expect(desiredContract).not.toContain("installation");
    expect(desiredContract).not.toContain("store_path");
    expect(desiredContract).toContain("modeldeploymentsnapshot");

    const currentContract = JSON.stringify([
      currentState.proto,
      currentDeployment.proto,
    ]).toLowerCase();
    expect(currentContract).not.toContain("desired");
    expect(currentContract).not.toContain("catalog");
    expect(currentContract).not.toContain("provenance");
    expect(currentContract).not.toContain("launch_intent");
    expect(currentContract).not.toContain("origin");
    expect(currentContract).not.toContain("installation");
    expect(currentContract).not.toContain("store_path");
    expect(currentContract).not.toContain("docker");
  });
});

type MessageDescriptor = {
  proto: {
    field: readonly { name: string; number: number }[];
    oneofDecl: readonly { name: string }[];
  };
};

function message(
  file: { messages: readonly { name: string; proto: unknown }[] },
  name: string,
): MessageDescriptor {
  const value = file.messages.find((candidate) => candidate.name === name);
  expect(value, `missing ${name} message`).toBeDefined();
  return value! as MessageDescriptor;
}

it("round-trips decoded FieldMask paths through ProtoJSON lower-camel encoding", () => {
  const request = create(UpdateModelDeploymentMetadataRequestSchema, {
    modelDeploymentId: "model-01",
    metadata: { displayName: "Research Model" },
    updateMask: { paths: ["display_name"] },
  });
  expect(toJson(UpdateModelDeploymentMetadataRequestSchema, request)).toEqual({
    modelDeploymentId: "model-01",
    metadata: { displayName: "Research Model" },
    updateMask: "displayName",
  });
  expect(
    fromJson(UpdateModelDeploymentMetadataRequestSchema, {
      modelDeploymentId: "model-01",
      metadata: { displayName: "Research Model" },
      updateMask: "displayName",
    }).updateMask?.paths,
  ).toEqual(["display_name"]);
});

function fieldNames(message: MessageDescriptor) {
  return message.proto.field.map(({ name }) => name);
}

function expectLogContract(
  file: { messages: readonly { name: string; proto: unknown }[]; proto: unknown },
  deploymentIdField: string,
) {
  expect(fieldNames(message(file, "ReadDeploymentLogsRequest"))).toEqual([
    deploymentIdField,
    "source_names",
    "page_size",
    "before_timestamp",
  ]);
  expect(fieldNames(message(file, "ReadDeploymentLogsResponse"))).toEqual([
    "sources",
    "entries",
    "next_before_timestamp",
    "history_state",
    "response_truncated",
    "latest_positions",
  ]);
  expect(fieldNames(message(file, "FollowDeploymentLogsRequest"))).toEqual([
    deploymentIdField,
    "source_names",
    "resume_positions",
  ]);
  expect(fieldNames(message(file, "FollowDeploymentLogsResponse"))).toEqual([
    "source_event",
    "entry",
  ]);
  expect(fieldNames(message(file, "DeploymentLogSource"))).toEqual([
    "source_name",
    "source_label",
    "state",
  ]);
  expect(fieldNames(message(file, "DeploymentLogEntry"))).toEqual([
    "source_name",
    "source_label",
    "stream",
    "timestamp",
    "content",
    "truncated",
  ]);
  expect(fieldNames(message(file, "DeploymentLogResumePosition"))).toEqual([
    "source_name",
    "latest_timestamp",
  ]);
  expect(fieldNames(message(file, "DeploymentLogSourceEvent"))).toEqual([
    "source_name",
    "source_label",
    "state",
  ]);
  const contract = JSON.stringify(file.proto).toLowerCase();
  expect(contract).not.toContain("container_id");
  expect(contract).not.toContain("host_path");
}

type MethodDescriptor = {
  proto: { serverStreaming: boolean };
};

function method(
  file: { services: readonly { methods: readonly { name: string; proto: unknown }[] }[] },
  name: string,
): MethodDescriptor {
  const value = file.services[0]?.methods.find((candidate) => candidate.name === name);
  expect(value, `missing ${name} method`).toBeDefined();
  return value! as MethodDescriptor;
}

function fieldNumbers(message: MessageDescriptor) {
  return message.proto.field.map(({ name, number }) => [name, number]);
}
