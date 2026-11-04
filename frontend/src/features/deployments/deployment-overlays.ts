/**
 * An accepted mutation shown over the streamed deployments until the stream shows its effect. A
 * lifecycle overlay (create, pause, resume, delete) shows the accepted deployment until the stream
 * lists a created deployment or shows the accepted desired state; a content overlay (rename,
 * endpoint edit) applies its change over the streamed deployment until the stream shows it.
 */
export interface DeploymentOverlay<Deployment> {
  deployment: Deployment;
  fence: OverlayFence<Deployment>;
}

export type OverlayFence<Deployment> =
  | {
      kind: "lifecycle";
      /** Whether the deployment was not listed yet when accepted, so its absence is not a delete. */
      created: boolean;
    }
  | {
      kind: "content";
      apply: (streamed: Deployment) => Deployment;
      shown: (streamed: Deployment) => boolean;
    };

/** How the overlay merge reads one deployment type. */
export interface DeploymentFields<Deployment> {
  desiredState: (deployment: Deployment) => number;
  id: (deployment: Deployment) => string;
  tearingDown: (deployment: Deployment) => boolean;
}

/**
 * Applies the pending overlays to the streamed deployments and names the overlays the stream
 * already shows, which the caller drops. A stream payload read before a mutation but delivered
 * after its response still shows the earlier desired state or content, so it does not settle it.
 */
export function mergeDeploymentOverlays<Deployment>(
  deployments: readonly Deployment[],
  overlays: ReadonlyMap<string, DeploymentOverlay<Deployment>>,
  fields: DeploymentFields<Deployment>,
): { deployments: Deployment[]; settled: string[] } {
  const merged = [...deployments];
  const settled: string[] = [];

  for (const [deploymentId, overlay] of overlays) {
    const index = merged.findIndex((deployment) => fields.id(deployment) === deploymentId);
    const streamed = index >= 0 ? merged[index] : undefined;
    const { fence } = overlay;

    if (!streamed) {
      // A created deployment the stream has not listed yet stays; any other absence is a delete.
      if (fence.kind === "lifecycle" && fence.created && !fields.tearingDown(overlay.deployment)) {
        merged.push(overlay.deployment);
      } else {
        settled.push(deploymentId);
      }
    } else if (fields.tearingDown(streamed)) {
      // Teardown is terminal and supersedes any mutation accepted before it.
      settled.push(deploymentId);
    } else if (fence.kind === "lifecycle") {
      // A pause or resume is shown once the stream reaches its desired state; a create once listed.
      if (
        fence.created ||
        fields.desiredState(streamed) === fields.desiredState(overlay.deployment)
      )
        settled.push(deploymentId);
      else merged[index] = overlay.deployment;
    } else if (fence.shown(streamed)) {
      settled.push(deploymentId);
    } else {
      merged[index] = fence.apply(streamed);
    }
  }

  return { deployments: merged, settled };
}

/**
 * The overlays with `overlay` set for its deployment. A content overlay joins a pending content
 * overlay, such as an endpoint edit after a rename, so neither change hides the other until the
 * stream shows both; any other overlay replaces the pending one.
 */
export function withOverlay<Deployment>(
  overlays: ReadonlyMap<string, DeploymentOverlay<Deployment>>,
  deploymentId: string,
  overlay: DeploymentOverlay<Deployment>,
): ReadonlyMap<string, DeploymentOverlay<Deployment>> {
  const pending = overlays.get(deploymentId)?.fence;
  const { fence } = overlay;
  const joined: DeploymentOverlay<Deployment> =
    pending?.kind === "content" && fence.kind === "content"
      ? {
          deployment: overlay.deployment,
          fence: {
            apply: (streamed) => fence.apply(pending.apply(streamed)),
            kind: "content",
            shown: (streamed) => pending.shown(streamed) && fence.shown(streamed),
          },
        }
      : overlay;
  return new Map(overlays).set(deploymentId, joined);
}

/**
 * The overlays without the settled ones that `merged` read; an overlay set since then for the same
 * deployment is newer and stays.
 */
export function withoutSettled<Deployment>(
  overlays: ReadonlyMap<string, DeploymentOverlay<Deployment>>,
  merged: ReadonlyMap<string, DeploymentOverlay<Deployment>>,
  settled: readonly string[],
): ReadonlyMap<string, DeploymentOverlay<Deployment>> {
  const next = new Map(overlays);
  for (const deploymentId of settled) {
    if (next.get(deploymentId) === merged.get(deploymentId)) next.delete(deploymentId);
  }
  return next.size === overlays.size ? overlays : next;
}

/**
 * Whether a non-teardown overlay may still apply: a deployment being torn down, as accepted or
 * listed, never shows an earlier lifecycle again.
 */
export function acceptsOverlay<Deployment>(
  deployment: Deployment,
  pending: Deployment | undefined,
  listed: Deployment | undefined,
  fields: DeploymentFields<Deployment>,
): boolean {
  if (fields.tearingDown(deployment)) return true;
  return !(pending && fields.tearingDown(pending)) && !(listed && fields.tearingDown(listed));
}
