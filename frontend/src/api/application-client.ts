import { create } from "@bufbuild/protobuf";
import { type Client, createClient } from "@connectrpc/connect";

import { createConsoleTransport } from "@/api/console-transport";
import {
  createDeploymentLogOperations,
  type DeploymentLogOperations,
  toProtoTimestamp,
} from "@/api/deployment-log-client";
import { resolveManagementOrigin } from "@/api/management-origin";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import {
  ApplicationDeploymentService,
  type FollowDeploymentLogsRequest,
  FollowDeploymentLogsRequestSchema,
  type FollowDeploymentLogsResponse,
  type ReadDeploymentLogsRequest,
  ReadDeploymentLogsRequestSchema,
  type ReadDeploymentLogsResponse,
} from "@/gen/dotcore/application/v1/service_pb";
import {
  type ApplicationDeploymentState,
  ApplicationLifecycleState,
} from "@/gen/dotcore/application/v1/state_pb";
import { CatalogService } from "@/gen/dotcore/catalog/v1/service_pb";
import { SiteService } from "@/gen/dotcore/site/v1/service_pb";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/lib/deployment-metadata";
import { normalizeDeploymentName } from "@/lib/deployment-name";

export type ApplicationCatalogClient = Pick<Client<typeof CatalogService>, "listApplications">;
type GeneratedApplicationDeploymentServiceClient = Pick<
  Client<typeof ApplicationDeploymentService>,
  | "listApplicationDeployments"
  | "getApplicationDeployment"
  | "createApplicationDeployment"
  | "removeApplicationDeployment"
  | "pauseDeployment"
  | "resumeDeployment"
>;
export type ApplicationDeploymentLogClient = DeploymentLogOperations;

export type ApplicationDeploymentServiceClient = GeneratedApplicationDeploymentServiceClient & {
  updateApplicationDeploymentMetadata: (
    deploymentId: string,
    update: DeploymentMetadataUpdate,
  ) => Promise<DeploymentMetadata>;
};
export interface ApplicationClient {
  catalog: ApplicationCatalogClient;
  application: ApplicationDeploymentServiceClient;
  site: Pick<Client<typeof SiteService>, "listConfiguredExecutionTargets">;
  logs?: ApplicationDeploymentLogClient;
}

const applicationServerUrl = resolveManagementOrigin(
  import.meta.env.VITE_DOTCORE_MANAGEMENT_ORIGIN,
  {
    allowTestFallback: import.meta.env.MODE === "test",
    allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
  },
);

export function createApplicationClient(baseUrl = applicationServerUrl): ApplicationClient {
  const transport = createConsoleTransport(baseUrl);
  const application = createClient(ApplicationDeploymentService, transport);

  return {
    catalog: createClient(CatalogService, transport),
    application: {
      listApplicationDeployments: application.listApplicationDeployments,
      getApplicationDeployment: application.getApplicationDeployment,
      createApplicationDeployment: async (request) =>
        validateCreateApplicationResponse(
          request.applicationDeploymentName,
          await application.createApplicationDeployment(request),
        ),
      removeApplicationDeployment: application.removeApplicationDeployment,
      updateApplicationDeploymentMetadata: async (deploymentId, update) => {
        const requestedName = normalizeDeploymentName(update.metadata.displayName);
        const response = await application.updateApplicationDeploymentMetadata({
          applicationDeploymentId: deploymentId,
          metadata: { displayName: requestedName },
          updateMask: {
            paths: update.fields.flatMap((field) =>
              field === "displayName" ? ["display_name"] : [],
            ),
          },
        });
        return validateUpdateApplicationDeploymentMetadataResponse(
          deploymentId,
          { displayName: requestedName },
          response,
        );
      },
      pauseDeployment: async (request) =>
        validatePauseDeploymentResponse(
          request.applicationDeploymentId ?? "",
          await application.pauseDeployment(request),
        ),
      resumeDeployment: async (request) =>
        validateResumeDeploymentResponse(
          request.applicationDeploymentId ?? "",
          await application.resumeDeployment(request),
        ),
    },
    site: createClient(SiteService, transport),
    logs: createDeploymentLogOperations<
      ReadDeploymentLogsRequest,
      FollowDeploymentLogsRequest,
      ReadDeploymentLogsResponse,
      FollowDeploymentLogsResponse
    >(
      (request, options) => application.readDeploymentLogs(request, options),
      (request, options) => application.followDeploymentLogs(request, options),
      (request) =>
        create(ReadDeploymentLogsRequestSchema, {
          applicationDeploymentId: request.deploymentId,
          sourceNames: request.sourceNames ?? [],
          pageSize: request.pageSize,
          beforeTimestamp: toProtoTimestamp(request.beforeTimestamp),
        }),
      (request) =>
        create(FollowDeploymentLogsRequestSchema, {
          applicationDeploymentId: request.deploymentId,
          sourceNames: request.sourceNames ?? [],
          resumePositions: request.resumePositions ?? [],
        }),
    ),
  };
}

export const applicationClient = createApplicationClient();

export class ApplicationResponseProtocolError extends Error {
  constructor(action: "Create" | "Update metadata" | "Pause" | "Resume", message: string) {
    super(`${action} response protocol error: ${message}`);
    this.name = "ApplicationResponseProtocolError";
  }
}

export function validateCreateApplicationResponse(
  requestedName: string | undefined,
  response: Awaited<
    ReturnType<Client<typeof ApplicationDeploymentService>["createApplicationDeployment"]>
  >,
): typeof response {
  const deployment = response.deployment;
  if (!deployment?.applicationDeploymentId) {
    throw new ApplicationResponseProtocolError("Create", "missing accepted deployment ID.");
  }
  if (!deployment.applicationDeploymentName) {
    throw new ApplicationResponseProtocolError("Create", "missing accepted deployment name.");
  }
  if (
    requestedName !== undefined &&
    deployment.applicationDeploymentName !== normalizeDeploymentName(requestedName)
  ) {
    throw new ApplicationResponseProtocolError(
      "Create",
      "accepted deployment name does not match the requested normalized name.",
    );
  }
  return response;
}

export function validateUpdateApplicationDeploymentMetadataResponse(
  applicationDeploymentId: string,
  requestedMetadata: DeploymentMetadata,
  response: Awaited<
    ReturnType<Client<typeof ApplicationDeploymentService>["updateApplicationDeploymentMetadata"]>
  >,
): DeploymentMetadata {
  const deployment = response.deployment;
  if (!deployment || deployment.applicationDeploymentId !== applicationDeploymentId) {
    throw new ApplicationResponseProtocolError(
      "Update metadata",
      "missing or mismatched accepted deployment ID.",
    );
  }
  if (
    !deployment.applicationDeploymentName ||
    deployment.applicationDeploymentName !== requestedMetadata.displayName
  ) {
    throw new ApplicationResponseProtocolError(
      "Update metadata",
      "accepted deployment name does not match the requested normalized name.",
    );
  }
  return { displayName: deployment.applicationDeploymentName };
}

export function validatePauseDeploymentResponse(
  applicationDeploymentId: string,
  response: Awaited<ReturnType<Client<typeof ApplicationDeploymentService>["pauseDeployment"]>>,
): typeof response {
  return validateAcceptedResponse(
    "Pause",
    applicationDeploymentId,
    response,
    ApplicationDeploymentDesiredState.PAUSED,
    new Set([
      ApplicationLifecycleState.PAUSING,
      ApplicationLifecycleState.PAUSED,
      ApplicationLifecycleState.UNAVAILABLE,
    ]),
  );
}

export function validateResumeDeploymentResponse(
  applicationDeploymentId: string,
  response: Awaited<ReturnType<Client<typeof ApplicationDeploymentService>["resumeDeployment"]>>,
): typeof response {
  return validateAcceptedResponse(
    "Resume",
    applicationDeploymentId,
    response,
    ApplicationDeploymentDesiredState.PRESENT,
    new Set([
      ApplicationLifecycleState.QUEUED,
      ApplicationLifecycleState.PREPARING,
      ApplicationLifecycleState.STARTING,
      ApplicationLifecycleState.RUNNING,
      ApplicationLifecycleState.READY,
      ApplicationLifecycleState.FAILED,
      ApplicationLifecycleState.UNAVAILABLE,
    ]),
  );
}

function validateAcceptedResponse<T extends { deployment?: ApplicationDeployment }>(
  action: "Pause" | "Resume",
  applicationDeploymentId: string,
  response: T,
  desiredState: ApplicationDeploymentDesiredState,
  acceptedLifecycles: ReadonlySet<ApplicationLifecycleState>,
): T {
  const deployment = response.deployment;
  if (!deployment || deployment.applicationDeploymentId !== applicationDeploymentId) {
    throw new ApplicationResponseProtocolError(
      action,
      "missing or mismatched accepted deployment ID.",
    );
  }

  if (deployment.desiredState !== desiredState) {
    throw new ApplicationResponseProtocolError(
      action,
      `accepted deployment is not ${ApplicationDeploymentDesiredState[desiredState]}.`,
    );
  }

  if (!isAcceptedLifecycle(deployment.state, acceptedLifecycles)) {
    throw new ApplicationResponseProtocolError(
      action,
      "accepted deployment has an invalid lifecycle state.",
    );
  }

  return response;
}

function isAcceptedLifecycle(
  state: ApplicationDeploymentState | undefined,
  acceptedLifecycles: ReadonlySet<ApplicationLifecycleState>,
): boolean {
  return state !== undefined && acceptedLifecycles.has(state.lifecycleState);
}
