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

/** An application deployment flattened for the Apps table. A missing value is absent. */
export interface ApplicationDeploymentRow {
  /** The catalog application's name from the deployment's immutable snapshot. */
  applicationName?: string;
  deploymentId: string;
  displayName: string;
  endpointUrl?: string;
  /** The machine's SiteNode ID, absent without a placement. */
  machineId?: string;
  machineName?: string;
  routeName?: string;
  status: StatusProjection;
  version?: string;
  /** Accelerator memory attributed to the deployment's services, absent when none is measured. */
  vramBytes?: bigint;
}

export function projectApplicationDeploymentRows(
  deployments: readonly ApplicationDeployment[],
  machines: readonly Machine[] | undefined,
): ApplicationDeploymentRow[] {
  return deployments.map((deployment) => projectApplicationDeploymentRow(deployment, machines));
}

function projectApplicationDeploymentRow(
  deployment: ApplicationDeployment,
  machines: readonly Machine[] | undefined,
): ApplicationDeploymentRow {
  const siteNodeId = deployment.placement?.siteNodeId;
  const machine = placementMachine(machines, siteNodeId);

  return {
    applicationName: deployment.snapshot?.displayName || undefined,
    deploymentId: deployment.applicationDeploymentId,
    displayName: applicationDeploymentPresentationName(deployment),
    endpointUrl: applicationDeploymentEndpoint(deployment),
    machineId: siteNodeId,
    machineName: siteNodeId ? machineName(siteNodeId, machine) : undefined,
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
