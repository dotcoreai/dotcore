import { create } from "@bufbuild/protobuf";
import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import {
  createDeploymentLogOperations,
  type DeploymentLogOperations,
  toProtoTimestamp,
} from "@/api/deployment-log-client";
import { resolveManagementOrigin } from "@/api/management-origin";
import { CatalogService } from "@/gen/dotcore/catalog/v1/service_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type CreateLocalModelDeploymentRequest,
  type CreateModelDeploymentResponse,
  type EstimateLocalModelResourcesRequest,
  type EstimateLocalModelResourcesResponse,
  type FollowDeploymentLogsRequest,
  FollowDeploymentLogsRequestSchema,
  type FollowDeploymentLogsResponse,
  type ModelDeploymentExposure,
  ModelDeploymentService,
  PreviewLocalModelDeploymentRequestSchema,
  type PreviewLocalModelDeploymentResponse,
  type ReadDeploymentLogsRequest,
  ReadDeploymentLogsRequestSchema,
  type ReadDeploymentLogsResponse,
} from "@/gen/dotcore/model/v1/service_pb";
import { type ModelDeploymentState, ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/lib/deployment-metadata";
import { normalizeDeploymentName } from "@/lib/deployment-name";

export type CatalogClient = Pick<
  Client<typeof CatalogService>,
  "listEngineReleases" | "listHardwareClasses" | "listModels" | "listUpstreamModelTemplates"
>;
type GeneratedModelDeploymentServiceClient = Pick<
  Client<typeof ModelDeploymentService>,
  | "listModelDeployments"
  | "createModelDeployment"
  | "updateModelDeployment"
  | "removeModelDeployment"
  | "pauseModel"
  | "resumeModel"
> & {
  /** Creates a local deployment from explicit model, profile, pool, and exposure selections. */
  createLocalModelDeployment?: (
    request: CreateLocalModelDeploymentRequest & {
      exposure?: ModelDeploymentExposure;
      modelDeploymentName?: string;
    },
  ) => Promise<CreateModelDeploymentResponse>;
  previewLocalModelDeployment?: (
    request: CreateLocalModelDeploymentRequest,
  ) => Promise<PreviewLocalModelDeploymentResponse>;
  estimateLocalModelResources?: (
    request: EstimateLocalModelResourcesRequest,
  ) => Promise<EstimateLocalModelResourcesResponse>;
  /** Reads which catalog model profiles can run on which configured model execution targets. */
  listModelProfileCompatibility?: Client<
    typeof ModelDeploymentService
  >["listModelProfileCompatibility"];
};
export type ModelDeploymentLogClient = DeploymentLogOperations;

export type ModelDeploymentServiceClient = GeneratedModelDeploymentServiceClient & {
  updateModelDeploymentMetadata: (
    deploymentId: string,
    update: DeploymentMetadataUpdate,
  ) => Promise<DeploymentMetadata>;
};

export interface ModelClient {
  catalog: CatalogClient;
  model: ModelDeploymentServiceClient;
  logs?: ModelDeploymentLogClient;
}

const modelServerUrl = resolveManagementOrigin(import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN, {
  allowTestFallback: import.meta.env.MODE === "test",
  allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
});

export function createModelClient(baseUrl = modelServerUrl): ModelClient {
  const transport = createConsoleTransport(baseUrl);
  const model = createClient(ModelDeploymentService, transport);

  return {
    catalog: createClient(CatalogService, transport),
    model: {
      listModelDeployments: model.listModelDeployments,
      previewLocalModelDeployment: (local) =>
        model.previewLocalModelDeployment(
          create(PreviewLocalModelDeploymentRequestSchema, { local }),
        ),
      estimateLocalModelResources: model.estimateLocalModelResources,
      listModelProfileCompatibility: model.listModelProfileCompatibility,
      createLocalModelDeployment: ({ exposure, modelDeploymentName, ...local }) =>
        model.createModelDeployment({
          source: { case: "local", value: local },
          exposure,
          modelDeploymentName,
        }),
      createModelDeployment: async (request) =>
        validateCreateModelResponse(
          request.modelDeploymentName,
          await model.createModelDeployment(request),
        ),
      updateModelDeployment: model.updateModelDeployment,
      updateModelDeploymentMetadata: async (deploymentId, update) => {
        const requestedName = normalizeDeploymentName(update.metadata.displayName);
        const response = await model.updateModelDeploymentMetadata({
          modelDeploymentId: deploymentId,
          metadata: { displayName: requestedName },
          updateMask: {
            paths: update.fields.flatMap((field) =>
              field === "displayName" ? ["display_name"] : [],
            ),
          },
        });
        return validateUpdateModelDeploymentMetadataResponse(
          deploymentId,
          { displayName: requestedName },
          response,
        );
      },
      removeModelDeployment: model.removeModelDeployment,
      pauseModel: async (request) =>
        validatePauseModelResponse(
          request.modelDeploymentId ?? "",
          await model.pauseModel(request),
        ),
      resumeModel: async (request) =>
        validateResumeModelResponse(
          request.modelDeploymentId ?? "",
          await model.resumeModel(request),
        ),
    },
    logs: createDeploymentLogOperations<
      ReadDeploymentLogsRequest,
      FollowDeploymentLogsRequest,
      ReadDeploymentLogsResponse,
      FollowDeploymentLogsResponse
    >(
      (request, options) => model.readDeploymentLogs(request, options),
      (request, options) => model.followDeploymentLogs(request, options),
      (request) =>
        create(ReadDeploymentLogsRequestSchema, {
          modelDeploymentId: request.deploymentId,
          sourceNames: request.sourceNames ?? [],
          pageSize: request.pageSize,
          beforeTimestamp: toProtoTimestamp(request.beforeTimestamp),
        }),
      (request) =>
        create(FollowDeploymentLogsRequestSchema, {
          modelDeploymentId: request.deploymentId,
          sourceNames: request.sourceNames ?? [],
          resumePositions: request.resumePositions ?? [],
        }),
    ),
  };
}

export const modelClient = createModelClient();

export class ModelResponseProtocolError extends Error {
  constructor(action: "Create" | "Update metadata" | "Pause" | "Resume", message: string) {
    super(`${action} response protocol error: ${message}`);
    this.name = "ModelResponseProtocolError";
  }
}

export function validateCreateModelResponse(
  requestedName: string | undefined,
  response: Awaited<ReturnType<Client<typeof ModelDeploymentService>["createModelDeployment"]>>,
): typeof response {
  const deployment = response.deployment;
  if (!deployment?.modelDeploymentId) {
    throw new ModelResponseProtocolError("Create", "missing accepted deployment ID.");
  }
  if (!deployment.modelDeploymentName) {
    throw new ModelResponseProtocolError("Create", "missing accepted deployment name.");
  }
  if (
    requestedName !== undefined &&
    deployment.modelDeploymentName !== normalizeDeploymentName(requestedName)
  ) {
    throw new ModelResponseProtocolError(
      "Create",
      "accepted deployment name does not match the requested normalized name.",
    );
  }
  return response;
}

export function validateUpdateModelDeploymentMetadataResponse(
  modelDeploymentId: string,
  requestedMetadata: DeploymentMetadata,
  response: Awaited<
    ReturnType<Client<typeof ModelDeploymentService>["updateModelDeploymentMetadata"]>
  >,
): DeploymentMetadata {
  const deployment = response.deployment;
  if (!deployment || deployment.modelDeploymentId !== modelDeploymentId) {
    throw new ModelResponseProtocolError(
      "Update metadata",
      "missing or mismatched accepted deployment ID.",
    );
  }
  if (
    !deployment.modelDeploymentName ||
    deployment.modelDeploymentName !== requestedMetadata.displayName
  ) {
    throw new ModelResponseProtocolError(
      "Update metadata",
      "accepted deployment name does not match the requested normalized name.",
    );
  }
  return { displayName: deployment.modelDeploymentName };
}

export function validatePauseModelResponse(
  modelDeploymentId: string,
  response: Awaited<ReturnType<Client<typeof ModelDeploymentService>["pauseModel"]>>,
): typeof response {
  return validateAcceptedResponse(
    "Pause",
    modelDeploymentId,
    response,
    ModelDeploymentDesiredState.PAUSED,
    new Set([
      ModelDeploymentStatus.PAUSING,
      ModelDeploymentStatus.PAUSED,
      ModelDeploymentStatus.UNAVAILABLE,
    ]),
  );
}

export function validateResumeModelResponse(
  modelDeploymentId: string,
  response: Awaited<ReturnType<Client<typeof ModelDeploymentService>["resumeModel"]>>,
): typeof response {
  return validateAcceptedResponse(
    "Resume",
    modelDeploymentId,
    response,
    ModelDeploymentDesiredState.PRESENT,
    new Set([
      ModelDeploymentStatus.QUEUED,
      ModelDeploymentStatus.PREPARING,
      ModelDeploymentStatus.DOWNLOADING,
      ModelDeploymentStatus.LOADING,
      ModelDeploymentStatus.WARMING_UP,
      ModelDeploymentStatus.READY,
      ModelDeploymentStatus.FAILED,
      ModelDeploymentStatus.UNAVAILABLE,
    ]),
  );
}

function validateAcceptedResponse<T extends { deployment?: ModelDeployment }>(
  action: "Pause" | "Resume",
  modelDeploymentId: string,
  response: T,
  desiredState: ModelDeploymentDesiredState,
  acceptedStatuses: ReadonlySet<ModelDeploymentStatus>,
): T {
  const deployment = response.deployment;
  if (!deployment || deployment.modelDeploymentId !== modelDeploymentId) {
    throw new ModelResponseProtocolError(action, "missing or mismatched accepted deployment ID.");
  }

  if (deployment.desiredState !== desiredState) {
    throw new ModelResponseProtocolError(
      action,
      `accepted deployment is not ${ModelDeploymentDesiredState[desiredState]}.`,
    );
  }

  if (!isAcceptedStatus(deployment.state, acceptedStatuses)) {
    throw new ModelResponseProtocolError(action, "accepted deployment has an invalid status.");
  }

  return response;
}

function isAcceptedStatus(
  state: ModelDeploymentState | undefined,
  acceptedStatuses: ReadonlySet<ModelDeploymentStatus>,
): boolean {
  return state !== undefined && acceptedStatuses.has(state.status);
}
