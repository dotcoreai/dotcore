import { ConnectError } from "@connectrpc/connect";
import { type RefObject, useCallback, useEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { type ModelClient, modelClient } from "@/api/model-client";
import {
  useWatchedExecutionTargets,
  useWatchedMachines,
  useWatchedRoutes,
} from "@/app/resource-watch";
import { InspectorPlaceholder } from "@/components/ui/inspector-placeholder";
import { deploymentChatTarget } from "@/features/chat/chattable-route";
import { DeploymentLogViewer } from "@/features/deployment-logs/deployment-log-viewer";
import { DeploymentEditor } from "@/features/deployments/deployment-editor";
import type { RouteMention } from "@/features/deployments/panel-mention";
import { placementMachine } from "@/features/deployments/placement";
import { useSiteMemory } from "@/features/memory/use-site-memory";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/lib/deployment-metadata";
import { rejectedMutationCodes } from "@/lib/mutation-errors";
import { usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import {
  appendPanelHref,
  appendPanelLocation,
  type PanelEntry,
  type PanelLocation,
} from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";

import {
  DeploymentMemoryRows,
  DeploymentMetricsProvider,
  DeploymentRequests,
} from "./deployment-metrics";
import { DeploymentModelInspector, type DeploymentModelInspectorScreen } from "./model-inspector";
import { modelInspectorPanelId } from "./model-inspector-panel";
import { describeMutationError, type MutationAction } from "./model-mutation-errors";
import {
  deploymentStatus,
  localModelSnapshot,
  modelDeploymentPresentationName,
} from "./model-projection";
import { useSharedModelsPolling } from "./models-polling-context";
import {
  toUpstreamConfiguration,
  UpstreamEditInspector,
  type UpstreamFormValues,
  UpstreamInspector,
} from "./upstream-model";
import { matchesEndpoint } from "./use-models-polling";

export interface DeploymentSidebarProps {
  client?: ModelClient;
  /** The active panel entry; the sidebar shows it when it is a model deployment panel. */
  entry: PanelEntry | undefined;
  /** The element focus moves to when the open deployment disappears, such as the page heading. */
  fallbackFocusRef: RefObject<HTMLElement | null>;
  /** The range label the metric cards start at, such as the range of the hosting page. */
  initialRange?: string;
  metricsClient?: MetricsClient;
  /** The element focus returns to when the sidebar closes, set by whatever opened it. */
  openerRef: RefObject<HTMLElement | null>;
}

/**
 * The sidebar of a local or upstream model deployment and its child screens (configuration, logs,
 * edit), with the deployment's pause, resume, teardown and edit actions. Any page hosting model
 * deployment panels mounts it; it follows the panel stack, navigates on the hosting page, and
 * publishes the deployment, machine and route names its back labels read.
 */
export function DeploymentSidebar({
  client = modelClient,
  entry,
  fallbackFocusRef,
  initialRange,
  metricsClient = defaultMetricsClient,
  openerRef,
}: DeploymentSidebarProps) {
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const [selection, setSelection] = useState<DeploymentSelection>();
  const urlSelection = deploymentSelectionFromEntry(entry);
  const editingDeployment = entry?.page === "models" && entry.screen === "edit";
  const viewingLogs = entry?.page === "models" && entry.screen === "logs";
  const deploymentScreen: DeploymentModelInspectorScreen =
    entry?.page === "models" && entry.screen === "configuration" ? "configuration" : "overview";
  const [upstreamEditPending, setUpstreamEditPending] = useState(false);
  const [upstreamError, setUpstreamError] = useState<string>();
  const [mutationError, setMutationError] = useState<MutationError>();
  const {
    applyDeploymentEndpointOverlay,
    applyDeploymentMetadataOverlay,
    applyDeploymentOverlay,
    deploymentsFailed,
    error: pollingError,
    snapshot,
  } = useSharedModelsPolling();
  const machineList = useWatchedMachines()?.value?.machines;
  // A failed or reconnecting deployment read may leave segments outdated.
  const siteMemory = useSiteMemory(
    snapshot?.runningModels,
    Boolean(pollingError) || deploymentsFailed,
  );
  const [pendingActions, setPendingActions] = useState<ReadonlySet<string>>(() => new Set());
  const pendingActionsRef = useRef<Set<string>>(new Set());
  const logOpenerRef = useRef<HTMLButtonElement>(null);
  const upstreamEditPendingRef = useRef(false);
  const selectionRef = useRef<DeploymentSelection | undefined>(undefined);
  const shouldRestoreFallbackFocus = useRef(false);

  const closeSelection = useCallback(() => {
    selectionRef.current = undefined;
    setMutationError(undefined);
    setUpstreamError(undefined);
    setSelection(undefined);
    panelNavigation.close();
  }, [panelNavigation]);
  const requestCloseSelection = useCallback(() => {
    panelNavigation.close();
  }, [panelNavigation]);

  useEffect(() => {
    if (!isSameSelection(selectionRef.current, urlSelection)) {
      selectionRef.current = urlSelection;
      setMutationError(undefined);
      setUpstreamError(undefined);
      setSelection(urlSelection);
    }
  }, [urlSelection]);

  const selectionMutationError =
    mutationError && isSameSelection(mutationError.selection, selection)
      ? mutationError
      : undefined;
  const selectionError = selectionMutationError?.message;
  const selectionTeardownError =
    selectionMutationError?.action === "Teardown" ? selectionMutationError.message : undefined;

  const routeViews = useWatchedRoutes()?.value?.routes ?? noRouteViews;

  const selectedDeployment = selection
    ? snapshot?.runningModels.find(
        (deployment) => deployment.modelDeploymentId === selection.modelDeploymentId,
      )
    : undefined;
  usePublishPanelTitles([
    ...(snapshot?.runningModels ?? []).map((deployment) =>
      panelTitle(
        "models",
        "deployment",
        deployment.modelDeploymentId,
        modelDeploymentPresentationName(deployment),
      ),
    ),
    ...(machineList ?? noMachines).map((machine) =>
      panelTitle(
        "machines",
        "machine",
        machine.machineId,
        machine.displayName || machine.machineId,
      ),
    ),
    ...routeViews.flatMap((view) =>
      view.route
        ? [panelTitle("routes", "route", view.route.inferenceRouteId, view.route.name)]
        : [],
    ),
  ]);
  const selectedPlacementMachine = placementMachine(
    machineList,
    selectedDeployment?.intent?.placement?.siteNodeId,
  );
  const executionTargets = useWatchedExecutionTargets({
    enabled: Boolean(selectedDeployment?.intent?.placement),
  });

  const selectedNavigationPending =
    editingDeployment && upstreamEditPending
      ? true
      : selection
        ? isDeploymentPending(pendingActions, selection.modelDeploymentId)
        : false;
  usePanelPendingNavigation(
    selectedNavigationPending,
    "An inspector operation is pending. Inspector navigation is unavailable.",
  );

  useEffect(() => {
    if (!selection || !snapshot) {
      return;
    }

    const deployment = snapshot.runningModels.find(
      (candidate) => candidate.modelDeploymentId === selection.modelDeploymentId,
    );
    if (!deployment) {
      queueMicrotask(() => {
        if (isSameSelection(selectionRef.current, selection)) {
          openerRef.current = fallbackFocusRef.current;
          shouldRestoreFallbackFocus.current = true;
          closeSelection();
        }
      });
    }
  }, [closeSelection, fallbackFocusRef, openerRef, selection, snapshot]);

  useEffect(() => {
    if (!selection && shouldRestoreFallbackFocus.current) {
      fallbackFocusRef.current?.focus();
      shouldRestoreFallbackFocus.current = false;
    }
  }, [fallbackFocusRef, selection]);

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

  const openChildScreen = (
    deployment: ModelDeployment,
    screen: "configuration" | "edit" | "logs",
  ) =>
    panelNavigation.navigatePanel(
      appendPanelLocation(location, {
        page: "models",
        resourceId: deployment.modelDeploymentId,
        screen,
      }),
    );

  const onInspectorOpenChange = (open: boolean) => {
    if (!open) requestCloseSelection();
  };

  return (
    <>
      {entry && urlSelection && !selectedDeployment ? (
        <InspectorPlaceholder
          entry={entry}
          error={snapshot ? undefined : pollingError}
          fallbackTitle="Model deployment"
          id={modelInspectorPanelId}
          onBack={() => panelNavigation.back()}
          onOpenChange={onInspectorOpenChange}
          openerRef={openerRef}
        />
      ) : null}

      {selectedDeployment?.snapshot?.source.case === "upstream" && !editingDeployment ? (
        <UpstreamInspector
          deployment={selectedDeployment}
          error={selectionError ?? upstreamError}
          initialRange={initialRange}
          metricsClient={metricsClient}
          teardownError={selectionTeardownError}
          onBack={() => panelNavigation.back()}
          onEdit={() => {
            setUpstreamError(undefined);
            openChildScreen(selectedDeployment, "edit");
          }}
          onUnload={() => unload(selectedDeployment)}
          chatTarget={deploymentChatTarget(
            routeViews,
            selectedDeployment.modelDeploymentId,
            localModelSnapshot(selectedDeployment.snapshot)?.requestKinds ?? [],
          )}
          containingRoutes={containingRoutesForDeployment(
            routeViews,
            selectedDeployment.modelDeploymentId,
            location,
          )}
          onOpenChange={onInspectorOpenChange}
          open={Boolean(urlSelection)}
          openerRef={openerRef}
          pending={isDeploymentPending(pendingActions, selectedDeployment.modelDeploymentId)}
        />
      ) : null}

      {selectedDeployment &&
      selectedDeployment.snapshot?.source.case !== "upstream" &&
      !editingDeployment ? (
        <DeploymentMetricsProvider
          active={deploymentScreen === "overview"}
          client={metricsClient}
          deployment={selectedDeployment}
          initialRange={initialRange}
          key={selectedDeployment.modelDeploymentId}
        >
          <DeploymentModelInspector
            catalogModel={snapshot?.availableModels.find(
              (candidate) =>
                candidate.modelId === localModelSnapshot(selectedDeployment.snapshot)?.modelId,
            )}
            executionTargets={executionTargets?.value?.executionTargets}
            catalogModelHref={catalogModelHrefForDeployment(selectedDeployment, location)}
            chatTarget={deploymentChatTarget(
              routeViews,
              selectedDeployment.modelDeploymentId,
              localModelSnapshot(selectedDeployment.snapshot)?.requestKinds ?? [],
            )}
            containingRoutes={containingRoutesForDeployment(
              routeViews,
              selectedDeployment.modelDeploymentId,
              location,
            )}
            deployment={selectedDeployment}
            error={selectionError}
            teardownError={selectionTeardownError}
            floatingLayer={
              viewingLogs && client.logs ? (
                <DeploymentLogViewer
                  colorKey={selectedDeployment.modelDeploymentId}
                  deploymentId={selectedDeployment.modelDeploymentId}
                  name={modelDeploymentPresentationName(selectedDeployment)}
                  onOpenChange={(open) => {
                    if (!open) panelNavigation.back();
                  }}
                  open
                  openerRef={logOpenerRef}
                  operations={client.logs}
                />
              ) : null
            }
            logOpenerRef={logOpenerRef}
            logs={client.logs}
            onBack={() => panelNavigation.back()}
            onOpenChange={onInspectorOpenChange}
            onOpenConfiguration={() => openChildScreen(selectedDeployment, "configuration")}
            onOpenLogs={() => openChildScreen(selectedDeployment, "logs")}
            onPause={() => {
              void pause(selectedDeployment);
            }}
            onResume={() => {
              void resume(selectedDeployment);
            }}
            onEdit={() => openChildScreen(selectedDeployment, "edit")}
            onUnload={() => unload(selectedDeployment)}
            open={Boolean(urlSelection)}
            openerRef={openerRef}
            pending={isDeploymentPending(pendingActions, selectedDeployment.modelDeploymentId)}
            pendingAction={
              pendingActions.has(
                deploymentPendingKey(selectedDeployment.modelDeploymentId, "pause"),
              )
                ? "pause"
                : pendingActions.has(
                      deploymentPendingKey(selectedDeployment.modelDeploymentId, "resume"),
                    )
                  ? "resume"
                  : pendingActions.has(
                        deploymentPendingKey(selectedDeployment.modelDeploymentId, "unload"),
                      )
                    ? "unload"
                    : undefined
            }
            memory={<DeploymentMemoryRows deployment={selectedDeployment} state={siteMemory} />}
            placementMachine={selectedPlacementMachine}
            requests={<DeploymentRequests />}
            placementMachineHref={placementMachineHrefForDeployment(
              selectedPlacementMachine,
              location,
            )}
            screen={deploymentScreen}
            suspendDismissal={viewingLogs}
          />
        </DeploymentMetricsProvider>
      ) : null}
      {editingDeployment &&
      selectedDeployment &&
      selectedDeployment.snapshot?.source.case !== "upstream" ? (
        <DeploymentEditor
          colorKey={selectedDeployment.modelDeploymentId}
          deploymentId={selectedDeployment.modelDeploymentId}
          metadata={{ displayName: modelDeploymentPresentationName(selectedDeployment) }}
          name={modelDeploymentPresentationName(selectedDeployment)}
          onClose={requestCloseSelection}
          onSaveMetadata={(update) => updateMetadata(selectedDeployment, update)}
          open
          openerRef={openerRef}
          terminal={selectedDeployment.desiredState === ModelDeploymentDesiredState.TEARDOWN}
        />
      ) : null}
      {editingDeployment && selectedDeployment?.snapshot?.source.case === "upstream" ? (
        <UpstreamEditInspector
          deployment={selectedDeployment}
          error={upstreamError}
          onBack={() => {
            setUpstreamError(undefined);
            panelNavigation.back();
          }}
          onOpenChange={(open) => {
            if (!open) requestCloseSelection();
          }}
          metadata={{ displayName: modelDeploymentPresentationName(selectedDeployment) }}
          onSaveEndpoint={(values) => updateUpstream(selectedDeployment, values)}
          onSaveMetadata={(update) => updateMetadata(selectedDeployment, update)}
          open
          openerRef={openerRef}
        />
      ) : null}
    </>
  );
}

/** The deployment whose panel is open, named by its immutable model deployment ID. */
interface DeploymentSelection {
  modelDeploymentId: string;
}

interface MutationError {
  action: Exclude<MutationAction, "Deploy">;
  message: string;
  selection: DeploymentSelection;
}

const deploymentActions = ["pause", "resume", "unload", "update-metadata"] as const;
const noMachines: readonly Machine[] = [];
const noRouteViews: readonly InferenceRouteView[] = [];

function deploymentSelectionFromEntry(
  entry: PanelEntry | undefined,
): DeploymentSelection | undefined {
  return entry?.page === "models" && entry.resourceId
    ? { modelDeploymentId: entry.resourceId }
    : undefined;
}

function isSameSelection(
  left: DeploymentSelection | undefined,
  right: DeploymentSelection | undefined,
): boolean {
  return Boolean(left && right && left.modelDeploymentId === right.modelDeploymentId);
}

function deploymentPendingKey(
  modelDeploymentId: string,
  action: (typeof deploymentActions)[number],
): string {
  return `deployment:${modelDeploymentId}:${action}`;
}

function isDeploymentPending(
  pendingActions: ReadonlySet<string>,
  modelDeploymentId: string,
): boolean {
  return deploymentActions.some((action) =>
    pendingActions.has(deploymentPendingKey(modelDeploymentId, action)),
  );
}

function containingRoutesForDeployment(
  routes: ReadonlyArray<InferenceRouteView>,
  modelDeploymentId: string,
  location: PanelLocation,
): readonly RouteMention[] {
  return routes.flatMap((view) => {
    const route = view.route;
    if (
      !route ||
      !route.fallbackGroups.some((group) => group.modelDeploymentIds.includes(modelDeploymentId))
    ) {
      return [];
    }
    return [
      {
        href: appendPanelHref(location, {
          page: "routes",
          resourceId: route.inferenceRouteId,
          screen: "route",
        }),
        inferenceRouteId: route.inferenceRouteId,
        name: route.name,
      },
    ];
  });
}

/** Destination of a deployment's catalog model mention, pushed above the deployment panel. */
function catalogModelHrefForDeployment(
  deployment: ModelDeployment,
  location: PanelLocation,
): string | undefined {
  const modelId = localModelSnapshot(deployment.snapshot)?.modelId;
  if (!modelId) return undefined;

  return appendPanelHref(location, {
    page: "model-catalog",
    resourceId: modelId,
    screen: "catalog",
  });
}

/** Destination of a deployment's placement machine mention, pushed above the deployment panel. */
function placementMachineHrefForDeployment(
  machine: Machine | undefined,
  location: PanelLocation,
): string | undefined {
  return machine
    ? appendPanelHref(location, {
        page: "machines",
        resourceId: machine.machineId,
        screen: "machine",
      })
    : undefined;
}
