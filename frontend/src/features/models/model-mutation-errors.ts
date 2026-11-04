import { Code, ConnectError } from "@connectrpc/connect";

import { describeMutationError as describeSharedMutationError } from "@/lib/mutation-errors";

import { parseRouteNameConflict, type RouteNameConflict } from "./deployment-exposure";

export type MutationAction = "Deploy" | "Teardown" | "Pause" | "Resume" | "Update metadata";

export function describeMutationError(
  action: MutationAction | "Update endpoint",
  error: unknown,
): string {
  const connectError = ConnectError.from(error);
  if (action === "Teardown" && connectError.code === Code.FailedPrecondition) {
    return connectError.rawMessage || "The service returned no details";
  }

  // Deploys start on the Catalog page, so their hint names the page that lists running models.
  return describeSharedMutationError(action, error, (sharedAction) => {
    const running =
      sharedAction === "Deploy" ? "Running on the Models page" : "the Running section";
    return `Check ${running} and service logs before retrying.`;
  });
}

/** Returns the conflict when the error is the backend rejecting a model deployment's route name as taken. */
export function routeConflictFromError(error: unknown): RouteNameConflict | undefined {
  const connectError = ConnectError.from(error);
  return connectError.code === Code.AlreadyExists
    ? parseRouteNameConflict(connectError.rawMessage)
    : undefined;
}
