import { ConnectError } from "@connectrpc/connect";
import { type RefObject, useCallback, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/domain/deployment-metadata";
import { describeMutationError } from "@/features/applications/application-mutation-errors";
import {
  type ApplicationDeployment,
  ApplicationDeploymentDesiredState,
} from "@/gen/dotcore/application/v1/deployment_pb";
import { ApplicationLifecycleState } from "@/gen/dotcore/application/v1/state_pb";
import { usePendingActions } from "@/hooks/use-pending-actions";
import { rejectedMutationCodes } from "@/lib/mutation-errors";

import type { useApplicationsPolling } from "./use-applications-polling";

/** The deployment whose panel is open, named by its immutable application deployment ID. */
export interface ApplicationInspectorSelection {
  applicationDeploymentId: string;
}

type ApplicationsPolling = ReturnType<typeof useApplicationsPolling>;

export interface UseApplicationDeploymentMutationsOptions extends Pick<
  ApplicationsPolling,
  "applyDeploymentMetadataOverlay" | "applyDeploymentOverlay"
> {
  client: ApplicationClient;
  /** The currently open selection; errors for another selection are dropped. */
  selectionRef: RefObject<ApplicationInspectorSelection | undefined>;
}

/**
 * The remove, pause, resume and metadata mutations of application deployments, with the pending
 * actions and errors they produce. Errors are attributed to the selection that started them and
 * dropped when another deployment is open by then.
 */
export function useApplicationDeploymentMutations({
  applyDeploymentMetadataOverlay,
  applyDeploymentOverlay,
  client,
  selectionRef,
}: UseApplicationDeploymentMutationsOptions) {
  const [mutationError, setMutationError] = useState<MutationError>();
  const pending = usePendingActions<DeploymentAction>();

  const clearErrors = useCallback(() => setMutationError(undefined), []);

  const remove = async (deployment: ApplicationDeployment) => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    if (!pending.start(deployment.applicationDeploymentId, "remove")) {
      return;
    }

    try {
      const response = await client.application.removeApplicationDeployment({
        applicationDeploymentId: deployment.applicationDeploymentId,
      });
      const acceptedDeployment = response.deployment;

      if (
        !acceptedDeployment ||
        acceptedDeployment.applicationDeploymentId !== deployment.applicationDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Teardown response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ApplicationDeploymentDesiredState.TEARDOWN) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Teardown response protocol error: accepted deployment is not TEARDOWN.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Teardown", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      pending.finish(deployment.applicationDeploymentId, "remove");
    }
  };

  const pause = async (deployment: ApplicationDeployment) => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    if (!pending.start(deployment.applicationDeploymentId, "pause")) {
      return;
    }

    try {
      const response = await client.application.pauseDeployment({
        applicationDeploymentId: deployment.applicationDeploymentId,
      });
      const acceptedDeployment = response.deployment;

      if (
        !acceptedDeployment ||
        acceptedDeployment.applicationDeploymentId !== deployment.applicationDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Pause response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ApplicationDeploymentDesiredState.PAUSED) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Pause response protocol error: accepted deployment is not PAUSED.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (
        !acceptedDeployment.state ||
        !new Set([
          ApplicationLifecycleState.PAUSING,
          ApplicationLifecycleState.PAUSED,
          ApplicationLifecycleState.UNAVAILABLE,
        ]).has(acceptedDeployment.state.lifecycleState)
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Pause response protocol error: accepted deployment has an invalid lifecycle state.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Pause", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      pending.finish(deployment.applicationDeploymentId, "pause");
    }
  };

  const resume = async (deployment: ApplicationDeployment) => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    if (!pending.start(deployment.applicationDeploymentId, "resume")) {
      return;
    }

    try {
      const response = await client.application.resumeDeployment({
        applicationDeploymentId: deployment.applicationDeploymentId,
      });
      const acceptedDeployment = response.deployment;

      if (
        !acceptedDeployment ||
        acceptedDeployment.applicationDeploymentId !== deployment.applicationDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Resume response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ApplicationDeploymentDesiredState.PRESENT) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message: "Resume response protocol error: accepted deployment is not PRESENT.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (
        !acceptedDeployment.state ||
        !new Set([
          ApplicationLifecycleState.QUEUED,
          ApplicationLifecycleState.PREPARING,
          ApplicationLifecycleState.STARTING,
          ApplicationLifecycleState.RUNNING,
          ApplicationLifecycleState.READY,
          ApplicationLifecycleState.FAILED,
          ApplicationLifecycleState.UNAVAILABLE,
        ]).has(acceptedDeployment.state.lifecycleState)
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            message:
              "Resume response protocol error: accepted deployment has an invalid lifecycle state.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Resume", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      pending.finish(deployment.applicationDeploymentId, "resume");
    }
  };

  const updateMetadata = async (
    deployment: ApplicationDeployment,
    update: DeploymentMetadataUpdate,
  ): Promise<DeploymentMetadata> => {
    const initiatingSelection: ApplicationInspectorSelection = {
      applicationDeploymentId: deployment.applicationDeploymentId,
    };
    const requested = update.metadata;
    if (!pending.start(deployment.applicationDeploymentId, "update-metadata")) {
      throw new Error("Metadata save is already pending.");
    }
    try {
      const acceptedMetadata = await client.application.updateApplicationDeploymentMetadata(
        deployment.applicationDeploymentId,
        update,
      );
      applyDeploymentMetadataOverlay(deployment.applicationDeploymentId, acceptedMetadata);
      return acceptedMetadata;
    } catch (error) {
      const connectError = ConnectError.from(error);
      if (!rejectedMutationCodes.has(connectError.code)) {
        try {
          const refreshed = await client.application.listApplicationDeployments({});
          const matches = refreshed.deployments.filter(
            (candidate) => candidate.applicationDeploymentId === deployment.applicationDeploymentId,
          );
          const observed = matches[0];
          const observedMetadata: DeploymentMetadata | undefined = observed
            ? { displayName: observed.applicationDeploymentName }
            : undefined;
          if (
            matches.length === 1 &&
            observedMetadata &&
            update.fields.every((field) => observedMetadata[field] === requested[field])
          ) {
            applyDeploymentMetadataOverlay(deployment.applicationDeploymentId, observedMetadata);
            return observedMetadata;
          }
        } catch {
          // Preserve the uncertain outcome when the authoritative read also fails.
        }
      }
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          message: describeMutationError("Update metadata", error),
          selection: initiatingSelection,
        });
      }
    } finally {
      pending.finish(deployment.applicationDeploymentId, "update-metadata");
    }
    throw new Error("Name could not be saved. Check the deployment status and try again.");
  };

  return { clearErrors, mutationError, pause, pending, remove, resume, updateMetadata };
}

interface MutationError {
  message: string;
  selection: ApplicationInspectorSelection;
}

type DeploymentAction = "pause" | "resume" | "remove" | "update-metadata";

export function isSameSelection(
  left: ApplicationInspectorSelection | undefined,
  right: ApplicationInspectorSelection | undefined,
): boolean {
  return Boolean(left && right && left.applicationDeploymentId === right.applicationDeploymentId);
}
