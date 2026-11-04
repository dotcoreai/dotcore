import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import type { Application, ApplicationService } from "@/gen/dotcore/catalog/v1/application_pb";
import type { OciPlatform } from "@/gen/dotcore/catalog/v1/oci_pb";
import type { StatusProjection } from "@/lib/status-projection";

/**
 * What an application uses when it runs: an inference route input, which the deploy dialog offers
 * to link, or persistent storage, which the apps controller provides.
 */
export type ApplicationUse = "inference-route" | "persistent-storage";

/** A catalog application flattened for the Application Catalog table. A missing value is absent. */
export interface CatalogApplicationRow {
  applicationId: string;
  colorKey: string;
  description?: string;
  displayName: string;
  /** Platforms every service supports, such as "linux/arm64/v8", in the first service's order. */
  platforms: string[];
  /** What any service uses, in `applicationUses` order. */
  uses: ApplicationUse[];
  /** Application deployments of this application that are not being torn down. */
  runningCount: number;
  runningStatus: StatusProjection | undefined;
  serviceCount: number;
  version?: string;
}

export interface CatalogApplicationRowInput {
  applications: readonly Application[];
  deployments: readonly ApplicationDeployment[];
}

/** Every use in display order. */
export const applicationUses: readonly ApplicationUse[] = ["inference-route", "persistent-storage"];

export function projectCatalogApplicationRows(
  input: CatalogApplicationRowInput,
): CatalogApplicationRow[] {
  return input.applications.map((application) =>
    projectCatalogApplicationRow(application, input.deployments),
  );
}

/** The label of a use, such as "Inference route". */
export function applicationUseLabel(use: ApplicationUse): string {
  return useLabels[use];
}

const useLabels: Record<ApplicationUse, string> = {
  "inference-route": "Inference route",
  "persistent-storage": "Persistent storage",
};

/** The application deployments made from the catalog application that are not being torn down. */
export function catalogApplicationDeployments(
  applicationId: string,
  deployments: readonly ApplicationDeployment[],
): ApplicationDeployment[] {
  // Paused deployments still count as deployed; ones being torn down are on their way out.
  return deployments.filter(
    (deployment) =>
      deployment.snapshot?.applicationId === applicationId &&
      deployment.desiredState !== ApplicationDeploymentDesiredState.TEARDOWN,
  );
}

function projectCatalogApplicationRow(
  application: Application,
  deployments: readonly ApplicationDeployment[],
): CatalogApplicationRow {
  const services = application.graph?.services ?? [];
  const runningCount = catalogApplicationDeployments(application.applicationId, deployments).length;

  return {
    applicationId: application.applicationId,
    colorKey: application.applicationId,
    ...(application.description ? { description: application.description } : {}),
    displayName: application.displayName,
    platforms: commonPlatforms(services),
    uses: applicationUses.filter((use) => services.some((service) => serviceUses(service, use))),
    runningCount,
    runningStatus:
      runningCount > 0 ? { label: `${runningCount} deployed`, tone: "success" } : undefined,
    serviceCount: services.length,
    ...(application.version ? { version: application.version } : {}),
  };
}

// A deployment runs every service on one platform, so only platforms all services support count.
function commonPlatforms(services: readonly ApplicationService[]): string[] {
  const [first, ...rest] = services.map(
    (service) => new Set((service.artifact?.supportedPlatforms ?? []).map(platformLabel)),
  );
  if (!first) return [];

  return [...first].filter((platform) => rest.every((platforms) => platforms.has(platform)));
}

// Secret bindings get no use: launch rejects them, and no catalog application declares one.
function serviceUses(service: ApplicationService, use: ApplicationUse): boolean {
  switch (use) {
    case "inference-route":
      return service.configurationBindings.some(
        ({ input }) => input?.kind.case === "inferenceRoute",
      );
    case "persistent-storage":
      return service.storageBindings.length > 0;
  }
}

function platformLabel(platform: OciPlatform): string {
  return [platform.operatingSystem, platform.architecture, platform.variant]
    .filter(Boolean)
    .join("/");
}
