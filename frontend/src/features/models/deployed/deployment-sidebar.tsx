import { type RefObject, useCallback, useEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { useClients } from "@/api/clients";
import { InspectorPlaceholder } from "@/components/ui/inspector-placeholder";
import { localModelSnapshot, modelDeploymentPresentationName } from "@/domain/model-projection";
import { placementMachine } from "@/domain/placement";
import { deploymentChatTarget } from "@/features/chat/chattable-route";
import { DeploymentLogViewer } from "@/features/deployment-logs/deployment-log-viewer";
import { DeploymentEditor } from "@/features/deployments/deployment-editor";
import { useSiteMemory } from "@/features/memory/use-site-memory";
import {
  DeploymentModelInspector,
  type DeploymentModelInspectorScreen,
} from "@/features/models/inspector/deployment-model-inspector";
import { modelInspectorPanelId } from "@/features/models/inspector/model-inspector-panel";
import { useSharedModelsPolling } from "@/features/models/polling/models-polling-context";
import { UpstreamEditInspector } from "@/features/models/upstream/upstream-edit-inspector";
import { UpstreamInspector } from "@/features/models/upstream/upstream-inspector";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import { usePanelNavigation } from "@/panels/panel-navigation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import { appendPanelLocation, type PanelEntry } from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";
import {
  useWatchedExecutionTargets,
  useWatchedMachines,
  useWatchedRoutes,
} from "@/resource-watch/resource-watch";

import {
  catalogModelHrefForDeployment,
  containingRoutesForDeployment,
  placementMachineHrefForDeployment,
} from "./deployment-links";
import {
  DeploymentMemoryRows,
  DeploymentMetricsProvider,
  DeploymentRequests,
} from "./deployment-metrics";
import {
  type DeploymentSelection,
  isSameSelection,
  useDeploymentMutations,
} from "./use-deployment-mutations";

export interface DeploymentSidebarProps {
  /** The active panel entry; the sidebar shows it when it is a model deployment panel. */
  entry: PanelEntry | undefined;
  /** The element focus moves to when the open deployment disappears, such as the page heading. */
  fallbackFocusRef: RefObject<HTMLElement | null>;
  /** The range label the metric cards start at, such as the range of the hosting page. */
  initialRange?: string;
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
  entry,
  fallbackFocusRef,
  initialRange,
  openerRef,
}: DeploymentSidebarProps) {
  const { metricsClient, modelClient: client } = useClients();
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const [selection, setSelection] = useState<DeploymentSelection>();
  const urlSelection = deploymentSelectionFromEntry(entry);
  const editingDeployment = entry?.page === "models" && entry.screen === "edit";
  const viewingLogs = entry?.page === "models" && entry.screen === "logs";
  const deploymentScreen: DeploymentModelInspectorScreen =
    entry?.page === "models" && entry.screen === "configuration" ? "configuration" : "overview";
  const logOpenerRef = useRef<HTMLButtonElement>(null);
  const selectionRef = useRef<DeploymentSelection | undefined>(undefined);
  const shouldRestoreFallbackFocus = useRef(false);
  const {
    applyDeploymentEndpointOverlay,
    applyDeploymentMetadataOverlay,
    applyDeploymentOverlay,
    deploymentsFailed,
    error: pollingError,
    snapshot,
  } = useSharedModelsPolling();
  const {
    clearErrors,
    clearUpstreamError,
    mutationError,
    pause,
    pending,
    resume,
    unload,
    updateMetadata,
    updateUpstream,
    upstreamEditPending,
    upstreamError,
  } = useDeploymentMutations({
    applyDeploymentEndpointOverlay,
    applyDeploymentMetadataOverlay,
    applyDeploymentOverlay,
    client,
    selectionRef,
  });
  const machineList = useWatchedMachines()?.value?.machines;
  // A failed or reconnecting deployment read may leave segments outdated.
  const siteMemory = useSiteMemory(
    snapshot?.runningModels,
    Boolean(pollingError) || deploymentsFailed,
  );

  const closeSelection = useCallback(() => {
    selectionRef.current = undefined;
    clearErrors();
    setSelection(undefined);
    panelNavigation.close();
  }, [clearErrors, panelNavigation]);
  const requestCloseSelection = useCallback(() => {
    panelNavigation.close();
  }, [panelNavigation]);

  useEffect(() => {
    if (!isSameSelection(selectionRef.current, urlSelection)) {
      selectionRef.current = urlSelection;
      clearErrors();
      setSelection(urlSelection);
    }
  }, [clearErrors, urlSelection]);

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
        ? pending.isPending(selection.modelDeploymentId)
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
            clearUpstreamError();
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
          pending={pending.isPending(selectedDeployment.modelDeploymentId)}
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
            pending={pending.isPending(selectedDeployment.modelDeploymentId)}
            pendingAction={pending.pendingAction(selectedDeployment.modelDeploymentId, [
              "pause",
              "resume",
              "unload",
            ])}
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
            clearUpstreamError();
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

const noMachines: readonly Machine[] = [];
const noRouteViews: readonly InferenceRouteView[] = [];

function deploymentSelectionFromEntry(
  entry: PanelEntry | undefined,
): DeploymentSelection | undefined {
  return entry?.page === "models" && entry.resourceId
    ? { modelDeploymentId: entry.resourceId }
    : undefined;
}
