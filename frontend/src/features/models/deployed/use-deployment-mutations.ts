import { ConnectError } from "@connectrpc/connect";
import { type RefObject, useCallback, useRef, useState } from "react";

import type { ModelClient } from "@/api/model-client";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/domain/deployment-metadata";
import { deploymentStatus } from "@/domain/model-projection";
import {
  describeMutationError,
  type MutationAction,
} from "@/features/models/model-mutation-errors";
import {
  matchesEndpoint,
  type useModelsPolling,
} from "@/features/models/polling/use-models-polling";
import {
  toUpstreamConfiguration,
  type UpstreamFormValues,
} from "@/features/models/upstream/upstream-form";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { rejectedMutationCodes } from "@/lib/mutation-errors";

/** The deployment whose panel is open, named by its immutable model deployment ID. */
export interface DeploymentSelection {
  modelDeploymentId: string;
}

export interface UseDeploymentMutationsOptions extends Pick<
  ModelsPolling,
  "applyDeploymentEndpointOverlay" | "applyDeploymentMetadataOverlay" | "applyDeploymentOverlay"
> {
  client: ModelClient;
  /** The currently open selection; results for another selection are dropped. */
  selectionRef: RefObject<DeploymentSelection | undefined>;
}

type ModelsPolling = ReturnType<typeof useModelsPolling>;

/**
 * The pause, resume, teardown and edit mutations of model deployments, with the pending actions and
 * errors they produce. Errors and results are attributed to the selection that started them and
 * dropped when another deployment is open by then.
 */
export function useDeploymentMutations({
  applyDeploymentEndpointOverlay,
  applyDeploymentMetadataOverlay,
  applyDeploymentOverlay,
  client,
  selectionRef,
}: UseDeploymentMutationsOptions) {
  const [upstreamEditPending, setUpstreamEditPending] = useState(false);
  const [upstreamError, setUpstreamError] = useState<string>();
  const [mutationError, setMutationError] = useState<MutationError>();
  const [pendingActions, setPendingActions] = useState<ReadonlySet<string>>(() => new Set());
  const pendingActionsRef = useRef<Set<string>>(new Set());
  const upstreamEditPendingRef = useRef(false);

  const clearErrors = useCallback(() => {
    setMutationError(undefined);
    setUpstreamError(undefined);
  }, []);
  const clearUpstreamError = useCallback(() => setUpstreamError(undefined), []);

  const updatePending = (key: string, pending: boolean) => {
    if (pending) {
      pendingActionsRef.current.add(key);
    } else {
      pendingActionsRef.current.delete(key);
    }

    setPendingActions(new Set(pendingActionsRef.current));
  };

  const unload = async (deployment: ModelDeployment): Promise<boolean> => {
    const initiatingSelection: DeploymentSelection = {
      modelDeploymentId: deployment.modelDeploymentId,
    };
    const pendingKey = deploymentPendingKey(deployment.modelDeploymentId, "unload");
    if (pendingActionsRef.current.has(pendingKey)) {
      return false;
    }

    setMutationError(undefined);
    updatePending(pendingKey, true);
    try {
      const response = await client.model.removeModelDeployment({
        modelDeploymentId: deployment.modelDeploymentId,
      });
      if (!isSameSelection(selectionRef.current, initiatingSelection)) {
        return false;
      }

      const acceptedDeployment = response.deployment;
      if (
        !acceptedDeployment ||
        acceptedDeployment.modelDeploymentId !== deployment.modelDeploymentId
      ) {
        setMutationError({
          action: "Teardown",
          message:
            "Teardown response protocol error: missing or mismatched accepted deployment ID.",
          selection: initiatingSelection,
        });
        return false;
      }

      if (
        acceptedDeployment.desiredState !== ModelDeploymentDesiredState.TEARDOWN ||
        deploymentStatus(acceptedDeployment) !== ModelDeploymentStatus.UNLOADING
      ) {
        setMutationError({
          action: "Teardown",
          message:
            "Teardown response protocol error: accepted deployment is not TEARDOWN/UNLOADING.",
          selection: initiatingSelection,
        });
        return false;
      }

      applyDeploymentOverlay(acceptedDeployment);
      return true;
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          action: "Teardown",
          message: describeMutationError("Teardown", mutationError),
          selection: initiatingSelection,
        });
      }
      return false;
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const pause = async (deployment: ModelDeployment) => {
    const initiatingSelection: DeploymentSelection = {
      modelDeploymentId: deployment.modelDeploymentId,
    };
    const pendingKey = deploymentPendingKey(deployment.modelDeploymentId, "pause");
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    updatePending(pendingKey, true);
    try {
      const response = await client.model.pauseModel({
        modelDeploymentId: deployment.modelDeploymentId,
      });

      const acceptedDeployment = response.deployment;
      if (
        !acceptedDeployment ||
        acceptedDeployment.modelDeploymentId !== deployment.modelDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            action: "Pause",
            message: "Pause response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ModelDeploymentDesiredState.PAUSED) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            action: "Pause",
            message: "Pause response protocol error: accepted deployment is not PAUSED.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (
        !acceptedDeployment.state ||
        !new Set([
          ModelDeploymentStatus.PAUSING,
          ModelDeploymentStatus.PAUSED,
          ModelDeploymentStatus.UNAVAILABLE,
        ]).has(acceptedDeployment.state.status)
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            action: "Pause",
            message: "Pause response protocol error: accepted deployment has an invalid status.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          action: "Pause",
          message: describeMutationError("Pause", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const resume = async (deployment: ModelDeployment) => {
    const initiatingSelection: DeploymentSelection = {
      modelDeploymentId: deployment.modelDeploymentId,
    };
    const pendingKey = deploymentPendingKey(deployment.modelDeploymentId, "resume");
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    updatePending(pendingKey, true);
    try {
      const response = await client.model.resumeModel({
        modelDeploymentId: deployment.modelDeploymentId,
      });

      const acceptedDeployment = response.deployment;
      if (
        !acceptedDeployment ||
        acceptedDeployment.modelDeploymentId !== deployment.modelDeploymentId
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            action: "Resume",
            message:
              "Resume response protocol error: missing or mismatched accepted deployment ID.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (acceptedDeployment.desiredState !== ModelDeploymentDesiredState.PRESENT) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            action: "Resume",
            message: "Resume response protocol error: accepted deployment is not PRESENT.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      if (
        !acceptedDeployment.state ||
        !new Set([
          ModelDeploymentStatus.QUEUED,
          ModelDeploymentStatus.PREPARING,
          ModelDeploymentStatus.LOADING,
          ModelDeploymentStatus.WARMING_UP,
          ModelDeploymentStatus.READY,
          ModelDeploymentStatus.FAILED,
          ModelDeploymentStatus.UNAVAILABLE,
        ]).has(acceptedDeployment.state.status)
      ) {
        if (isSameSelection(selectionRef.current, initiatingSelection)) {
          setMutationError({
            action: "Resume",
            message: "Resume response protocol error: accepted deployment has an invalid status.",
            selection: initiatingSelection,
          });
        }
        return;
      }

      applyDeploymentOverlay(acceptedDeployment);
    } catch (mutationError) {
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          action: "Resume",
          message: describeMutationError("Resume", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const updateUpstream = async (
    deployment: ModelDeployment,
    values: UpstreamFormValues,
  ): Promise<boolean> => {
    if (upstreamEditPendingRef.current) return false;
    upstreamEditPendingRef.current = true;
    setUpstreamEditPending(true);
    setUpstreamError(undefined);
    const requested = toUpstreamConfiguration(values);
    try {
      const response = await client.model.updateModelDeployment({
        modelDeploymentId: deployment.modelDeploymentId,
        upstreamDesiredConfiguration: requested,
      });
      if (
        !response.deployment ||
        response.deployment.modelDeploymentId !== deployment.modelDeploymentId
      ) {
        throw new Error("Update response protocol error: missing or mismatched deployment ID.");
      }
      // Endpoint updates do not own the name: only the endpoint overlays the live deployment, so
      // a pending rename stays when the response carries a stale whole-deployment projection.
      if (response.deployment.upstreamDesiredConfiguration) {
        applyDeploymentEndpointOverlay(
          deployment.modelDeploymentId,
          response.deployment.upstreamDesiredConfiguration,
        );
      }
      return true;
    } catch (error) {
      const connectError = ConnectError.from(error);
      if (!rejectedMutationCodes.has(connectError.code) && client.model.listModelDeployments) {
        try {
          const refreshed = await client.model.listModelDeployments({});
          const observed = refreshed.deployments.find(
            (candidate) => candidate.modelDeploymentId === deployment.modelDeploymentId,
          );
          if (
            !values.apiKey.trim() &&
            observed?.upstreamDesiredConfiguration &&
            matchesEndpoint(observed.upstreamDesiredConfiguration, requested)
          ) {
            applyDeploymentEndpointOverlay(
              deployment.modelDeploymentId,
              observed.upstreamDesiredConfiguration,
            );
            return true;
          }
        } catch {
          // The original uncertain result remains actionable when its one refresh fails.
        }
      }
      setUpstreamError(
        values.apiKey.trim()
          ? "Endpoint update outcome is uncertain because a replacement API key cannot be verified. Check the deployment before retrying."
          : describeMutationError("Update endpoint", error),
      );
      return false;
    } finally {
      upstreamEditPendingRef.current = false;
      setUpstreamEditPending(false);
    }
  };

  const updateMetadata = async (
    deployment: ModelDeployment,
    update: DeploymentMetadataUpdate,
  ): Promise<DeploymentMetadata> => {
    const initiatingSelection: DeploymentSelection = {
      modelDeploymentId: deployment.modelDeploymentId,
    };
    const requested = update.metadata;
    const pendingKey = deploymentPendingKey(deployment.modelDeploymentId, "update-metadata");
    if (pendingActionsRef.current.has(pendingKey)) {
      throw new Error("Metadata save is already pending.");
    }
    updatePending(pendingKey, true);
    try {
      const acceptedMetadata = await client.model.updateModelDeploymentMetadata(
        deployment.modelDeploymentId,
        update,
      );
      applyDeploymentMetadataOverlay(deployment.modelDeploymentId, acceptedMetadata);
      return acceptedMetadata;
    } catch (error) {
      const connectError = ConnectError.from(error);
      if (!rejectedMutationCodes.has(connectError.code)) {
        try {
          const refreshed = await client.model.listModelDeployments({});
          const matches = refreshed.deployments.filter(
            (candidate) => candidate.modelDeploymentId === deployment.modelDeploymentId,
          );
          const observed = matches[0];
          const observedMetadata: DeploymentMetadata | undefined = observed
            ? { displayName: observed.modelDeploymentName }
            : undefined;
          if (
            matches.length === 1 &&
            observedMetadata &&
            update.fields.every((field) => observedMetadata[field] === requested[field])
          ) {
            applyDeploymentMetadataOverlay(deployment.modelDeploymentId, observedMetadata);
            return observedMetadata;
          }
        } catch {
          // Preserve the uncertain outcome when the authoritative read also fails.
        }
      }
      if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setMutationError({
          action: "Update metadata",
          message: describeMutationError("Update metadata", error),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
    throw new Error("Name could not be saved. Check the deployment status and try again.");
  };

  return {
    clearErrors,
    clearUpstreamError,
    mutationError,
    pause,
    pendingActions,
    resume,
    unload,
    updateMetadata,
    updateUpstream,
    upstreamEditPending,
    upstreamError,
  };
}

interface MutationError {
  action: Exclude<MutationAction, "Deploy">;
  message: string;
  selection: DeploymentSelection;
}

const deploymentActions = ["pause", "resume", "unload", "update-metadata"] as const;

export function isSameSelection(
  left: DeploymentSelection | undefined,
  right: DeploymentSelection | undefined,
): boolean {
  return Boolean(left && right && left.modelDeploymentId === right.modelDeploymentId);
}

export function deploymentPendingKey(
  modelDeploymentId: string,
  action: (typeof deploymentActions)[number],
): string {
  return `deployment:${modelDeploymentId}:${action}`;
}

export function isDeploymentPending(
  pendingActions: ReadonlySet<string>,
  modelDeploymentId: string,
): boolean {
  return deploymentActions.some((action) =>
    pendingActions.has(deploymentPendingKey(modelDeploymentId, action)),
  );
}
