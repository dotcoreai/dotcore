import { validateDeploymentName } from "@/domain/deployment-name";
import type { ModelDeploymentExposure } from "@/gen/dotcore/model/v1/service_pb";

/** Exposure form state. An undefined `routeName` follows the deployment name; a string is custom. */
export interface DeploymentExposureValues {
  exposure: "direct" | "hidden";
  routeName?: string;
}

/** A route name rejected by the backend as already taken; the name is unknown when it cannot be parsed. */
export interface RouteNameConflict {
  routeName?: string;
}

const ROUTE_NAME_PATTERN = /^[a-z][a-z0-9.-]{0,62}$/;

export function deploymentExposure(
  values: DeploymentExposureValues,
  deploymentName: string,
): ModelDeploymentExposure {
  if (values.exposure === "hidden") {
    return { kind: { case: "hidden", value: {} } } as ModelDeploymentExposure;
  }

  return {
    kind: { case: "direct", value: { routeName: effectiveRouteName(values, deploymentName) } },
  } as ModelDeploymentExposure;
}

/**
 * The exposure form state of a deployment exposure, read back as `deploymentExposure` builds it: a
 * route name equal to the one the deployment name derives follows the name, any other is custom.
 */
export function deploymentExposureValues(
  exposure: ModelDeploymentExposure,
  deploymentName: string,
): DeploymentExposureValues {
  if (exposure.kind.case === "hidden") return { exposure: "hidden" };

  const routeName = exposure.kind.case === "direct" ? exposure.kind.value.routeName : undefined;
  return routeName === undefined || routeName === deriveRouteName(deploymentName)
    ? { exposure: "direct" }
    : { exposure: "direct", routeName };
}

export function validateDeploymentExposure(
  values: DeploymentExposureValues,
  deploymentName: string,
  existingRouteNames: Iterable<string> = [],
): string | undefined {
  if (values.exposure !== "direct") return undefined;

  return validateRouteName(effectiveRouteName(values, deploymentName), {
    following: values.routeName === undefined,
    existingRouteNames,
  });
}

/** Returns the route name that is displayed, validated, and submitted. */
export function effectiveRouteName(
  values: DeploymentExposureValues,
  deploymentName: string,
): string {
  return (values.routeName ?? deriveRouteName(deploymentName)).trim();
}

/** Returns a route-safe slug of a deployment name, or an empty string when none can be derived. */
export function deriveRouteName(deploymentName: string): string {
  const normalized = deploymentName
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9.]+/g, "-")
    .replace(/\.+/g, ".")
    .replace(/^[-.]+|[-.]+$/g, "");
  const stableId = normalized && !/^[a-z]/.test(normalized) ? `model-${normalized}` : normalized;

  return stableId.slice(0, 63).replace(/[-.]+$/g, "");
}

export function routeNameCollides(
  routeName: string,
  existingRouteNames: Iterable<string>,
): boolean {
  return new Set(existingRouteNames).has(routeName);
}

export function validateRouteName(
  routeName: string,
  {
    following = false,
    existingRouteNames = [],
  }: { following?: boolean; existingRouteNames?: Iterable<string> } = {},
): string | undefined {
  if (!ROUTE_NAME_PATTERN.test(routeName) && following) {
    return "No valid route name can be derived from the deployment name. Enter a route name.";
  }
  if (!routeName) return "Route name is required for a Direct deployment.";
  if (!ROUTE_NAME_PATTERN.test(routeName)) {
    return "Route name must start with a letter and contain only lowercase letters, numbers, hyphens, and periods.";
  }
  if (routeNameCollides(routeName, existingRouteNames)) {
    return routeNameTakenMessage(routeName);
  }
  return undefined;
}

export function routeNameTakenMessage(routeName: string): string {
  return `An inference route named "${routeName}" already exists.`;
}

/** Extracts the route name from the model service's `AlreadyExists` message. */
export function parseRouteNameConflict(message: string): RouteNameConflict {
  const routeName = /^Inference route name already exists: (.+)$/.exec(message.trim())?.[1];
  return { routeName };
}

/**
 * Returns the message for a backend route-name conflict, or undefined when the conflict no longer
 * applies because the route name was edited to another value.
 */
export function routeNameConflictMessage(
  conflict: RouteNameConflict | undefined,
  currentRouteName: string,
): string | undefined {
  if (!conflict) return undefined;
  if (conflict.routeName !== undefined && conflict.routeName !== currentRouteName) return undefined;
  return conflict.routeName === undefined
    ? "An inference route with this name already exists."
    : routeNameTakenMessage(conflict.routeName);
}

/**
 * Returns the message for a direct route that blocks deployment because its name is taken, either by
 * an existing route or by a backend conflict, or undefined when it is free. A route following the
 * deployment name also names the rename that fixes it, and reports a name that yields no valid
 * route. A route following an invalid deployment name has no message, so the name's own error
 * explains the problem.
 */
export function routeBlockingMessage({
  conflict,
  deploymentName,
  existingRouteNames,
  values,
}: {
  /** The route name the backend rejected as taken on the last deploy attempt. */
  conflict?: RouteNameConflict;
  deploymentName: string;
  existingRouteNames: Iterable<string>;
  values: DeploymentExposureValues;
}): string | undefined {
  if (values.exposure !== "direct") return undefined;

  const following = values.routeName === undefined;
  if (following && validateDeploymentName(deploymentName)) return undefined;

  const routeName = effectiveRouteName(values, deploymentName);
  const taken =
    routeNameConflictMessage(conflict, routeName) ??
    (routeNameCollides(routeName, existingRouteNames)
      ? routeNameTakenMessage(routeName)
      : undefined);
  if (taken) return following ? `${taken} Change the route or the deployment name.` : taken;

  // A custom route's format error waits for its blur or Deploy, but a derived one has no field to
  // blur.
  return following ? validateDeploymentExposure(values, deploymentName) : undefined;
}
