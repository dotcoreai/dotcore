import { describeMutationError as describeSharedMutationError } from "@/lib/mutation-errors";

export type ApplicationMutationAction =
  "Create" | "Teardown" | "Pause" | "Resume" | "Update metadata";

/** Describes a failed application deployment mutation, separating rejections from unknown outcomes. */
export function describeMutationError(action: ApplicationMutationAction, error: unknown): string {
  return describeSharedMutationError(
    action,
    error,
    () => "Check the Installed section and service logs before retrying.",
  );
}
