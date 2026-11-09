import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";

/**
 * The user-facing name of one application deployment. A caller-supplied name wins over the
 * catalog application's display name; the deployment ID and the auto-derived `applicationId-1`
 * name are not presentation names, so they fall back to the display name (or a generic label when
 * the snapshot is missing).
 */
export function applicationDeploymentPresentationName(deployment: ApplicationDeployment): string {
  return deployment.applicationDeploymentName &&
    deployment.applicationDeploymentName !== deployment.applicationDeploymentId &&
    deployment.applicationDeploymentName !== `${deployment.snapshot?.applicationId}-1`
    ? deployment.applicationDeploymentName
    : deployment.snapshot?.displayName || "Application";
}
