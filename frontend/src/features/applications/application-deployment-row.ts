import { machineName, placementMachine } from "@/features/deployments/placement";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import { applicationDeploymentPresentationName } from "@/lib/application-deployment-name";
import type { StatusProjection } from "@/lib/status-projection";

import { applicationAcceleratorMemory } from "./application-accelerator-memory";
import {
  applicationDeploymentEndpoint,
  projectApplicationDeploymentStatus,
} from "./application-projection";
import type { ApplicationResources } from "./use-application-resources";

/** An application deployment flattened for the Apps table. A missing value is absent. */
export interface ApplicationDeploymentRow {
  /** The catalog application's name from the deployment's immutable snapshot. */
  applicationName?: string;
  /** Share of the hosting machine's CPUs in use, as a ratio. */
  cpu?: number;
  deploymentId: string;
  displayName: string;
  endpointUrl?: string;
  /** The machine's SiteNode ID, absent without a placement. */
  machineId?: string;
  machineName?: string;
  /** Host memory in use, in bytes. */
  memoryBytes?: number;
  routeName?: string;
  status: StatusProjection;
  version?: string;
  /** Accelerator memory attributed to the deployment's services, absent when none is measured. */
  vramBytes?: bigint;
}

/** What the Apps table combines with each deployment. */
export interface ApplicationDeploymentRowSources {
  machines: readonly Machine[] | undefined;
  /** CPU and memory use by deployment id. */
  resources: ReadonlyMap<string, ApplicationResources>;
}

export function projectApplicationDeploymentRows(
  deployments: readonly ApplicationDeployment[],
  { machines, resources }: ApplicationDeploymentRowSources,
): ApplicationDeploymentRow[] {
  return deployments.map((deployment) =>
    projectApplicationDeploymentRow(
      deployment,
      machines,
      resources.get(deployment.applicationDeploymentId),
    ),
  );
}

function projectApplicationDeploymentRow(
  deployment: ApplicationDeployment,
  machines: readonly Machine[] | undefined,
  resources: ApplicationResources | undefined,
): ApplicationDeploymentRow {
  const siteNodeId = deployment.placement?.siteNodeId;
  const machine = placementMachine(machines, siteNodeId);

  return {
    applicationName: deployment.snapshot?.displayName || undefined,
    cpu: resources?.cpu,
    deploymentId: deployment.applicationDeploymentId,
    displayName: applicationDeploymentPresentationName(deployment),
    endpointUrl: applicationDeploymentEndpoint(deployment),
    machineId: siteNodeId,
    machineName: siteNodeId ? machineName(siteNodeId, machine) : undefined,
    memoryBytes: resources?.memoryBytes,
    routeName: deployment.selectedInferenceRoute?.routeName || undefined,
    status: projectApplicationDeploymentStatus(deployment),
    version: deployment.snapshot?.version || undefined,
    vramBytes: deploymentVramBytes(deployment),
  };
}

// The sum over every measured device of every service; absent when no service reports a measure.
function deploymentVramBytes(deployment: ApplicationDeployment): bigint | undefined {
  const devices = applicationAcceleratorMemory(deployment).flatMap((unit) =>
    unit.kind === "available" ? (unit.devices ?? []) : [],
  );
  return devices.length ? devices.reduce((sum, device) => sum + device.bytes, 0n) : undefined;
}
