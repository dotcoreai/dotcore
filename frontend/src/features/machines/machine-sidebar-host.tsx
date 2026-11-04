import { type RefObject, useCallback, useEffect, useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { usePanelNavigation } from "@/app/console-layout";
import { usePanelTitle, usePublishPanelTitles } from "@/app/panel-titles-context";
import {
  useWatchedApplicationDeployments,
  useWatchedExecutionTargets,
  useWatchedMachines,
  useWatchedModelDeployments,
  useWatchedNodeMemory,
} from "@/app/resource-watch";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { ResourceTitle } from "@/components/ui/resource-marker";
import {
  type MachineSnapshotDetail,
  useHardwareSnapshots,
} from "@/features/memory/use-hardware-snapshots";
import { modelDeploymentPresentationName } from "@/features/models/model-projection";
import type { ConfiguredExecutionTarget, Machine } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { applicationDeploymentPresentationName } from "@/lib/application-deployment-name";
import {
  activePanel,
  appendPanelLocation,
  canonicalizePanelLocation,
  type PanelEntry,
} from "@/lib/panel-stack";
import { panelTitle } from "@/lib/panel-titles";
import { mapRead, type ReadState } from "@/lib/read-state";

import {
  ExecutionTargetContent,
  type ExecutionTargetFocus,
  MachineDetailContent,
} from "./machine-details";
import { inspectedMachineId, machineInspectorPanelId } from "./machine-panels";

export interface MachineSidebarHostProps {
  /** The range label the resource history charts start at, such as the hosting page's range. */
  initialRange?: string;
  metricsClient?: MetricsClient;
  /** The element focus returns to when the sidebar closes; set it before opening the sidebar. */
  openerRef: RefObject<HTMLElement | null>;
}

/**
 * The machine sidebar of whichever page hosts it: a machine's details and its execution target
 * screens, opened from the panel stack. It reads the machine inventory and its hardware snapshot
 * from the resource watch, and the execution targets and application deployments only while open.
 * Navigation keeps the hosting page, so the sidebar opens over the machines page or the overview
 * alike.
 */
export function MachineSidebarHost({
  initialRange,
  metricsClient = defaultMetricsClient,
  openerRef,
}: MachineSidebarHostProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const panelNavigation = usePanelNavigation();
  // The inventory streams from the resource watch, so a machine that connects, goes stale, or
  // disconnects reaches the open sidebar without a reload.
  const machines = useWatchedMachines()?.value?.machines;
  // The requested stack picks the inspected machine, so the reads its sidebar needs can start
  // before canonicalization, which needs the execution targets to validate a target screen.
  const machineId = inspectedMachineId(location.search);
  const selectedMachine = machines?.find((machine) => machine.machineId === machineId);
  const inspectorOpen = Boolean(selectedMachine);
  const selectedMachines = useMemo(
    () => (selectedMachine ? [selectedMachine] : noMachines),
    [selectedMachine],
  );
  const detail = useHardwareSnapshots(selectedMachines, { enabled: inspectorOpen }).get(
    machineId ?? "",
  );
  // Deployments attribute memory to models and name the deployments a target runs. Execution
  // targets and application deployments only serve the sidebar, so they are watched only while
  // it is open.
  const watchedDeployments = useWatchedModelDeployments();
  const deployments = useMemo(
    () => mapRead(watchedDeployments, (value) => value.deployments),
    [watchedDeployments],
  );
  const watchedApplicationDeployments = useWatchedApplicationDeployments({
    enabled: inspectorOpen,
  });
  const applicationDeployments = useMemo(
    () => mapRead(watchedApplicationDeployments, (value) => value.deployments),
    [watchedApplicationDeployments],
  );
  // Node memory draws the machine's memory rows, so it is watched only while the sidebar is open.
  const watchedNodeMemory = useWatchedNodeMemory({ enabled: inspectorOpen });
  const nodeMemory = useMemo(
    () => mapRead(watchedNodeMemory, (value) => value.nodes),
    [watchedNodeMemory],
  );
  const watchedExecutionTargets = useWatchedExecutionTargets({ enabled: inspectorOpen });
  const executionTargets = useMemo(
    () => mapRead(watchedExecutionTargets, (value) => value.executionTargets),
    [watchedExecutionTargets],
  );
  const targets = executionTargets?.value;
  usePublishPanelTitles([
    ...(machines ?? []).map((machine) =>
      panelTitle(
        "machines",
        "machine",
        machine.machineId,
        machine.displayName || machine.machineId,
      ),
    ),
    ...(deployments?.value ?? []).map((deployment) =>
      panelTitle(
        "models",
        "deployment",
        deployment.modelDeploymentId,
        modelDeploymentPresentationName(deployment),
      ),
    ),
    ...(applicationDeployments?.value ?? []).map((deployment) =>
      panelTitle(
        "apps",
        "deployment",
        deployment.applicationDeploymentId,
        applicationDeploymentPresentationName(deployment),
      ),
    ),
    ...(targets ?? []).map((target) =>
      panelTitle("machines", "execution-target", target.executionTargetId, target.displayName),
    ),
  ]);
  const canonical = useMemo(
    () =>
      canonicalizePanelLocation(
        { pathname: location.pathname, search: location.search },
        {
          isAvailable: (entry, parent) => isMachinePanelAvailable(entry, parent, machines, targets),
        },
      ),
    [location.pathname, location.search, machines, targets],
  );
  const entry = activePanel(canonical.search);
  // Names the panel from an earlier page's list while this page's inventory is still read.
  const publishedTitle = usePanelTitle(entry);
  const executionTargetId =
    entry?.page === "machines" && entry.screen === "execution-target"
      ? entry.resourceId
      : undefined;
  const [returnFocusTargetId, setReturnFocusTargetId] = useState<string>();
  const executionTargetFocus = useMemo<ExecutionTargetFocus | undefined>(
    () =>
      returnFocusTargetId
        ? {
            executionTargetId: returnFocusTargetId,
            onFocused: () => setReturnFocusTargetId(undefined),
          }
        : undefined,
    [returnFocusTargetId],
  );
  // The body reveals once every read it shows has settled, so its sections appear together.
  const machineBodyReady =
    detail !== undefined &&
    detail.kind !== "loading" &&
    isSettled(executionTargets) &&
    isSettled(deployments) &&
    isSettled(applicationDeployments) &&
    isSettled(nodeMemory);
  const displayedInspector = useDisplayedMachineInspector(
    selectedMachine,
    detail,
    machineBodyReady,
  );
  useEffect(() => {
    if (!canonical.recovered) return;
    void navigate(canonical, { replace: true });
  }, [canonical, navigate]);

  const selectExecutionTarget = useCallback(
    (target: ConfiguredExecutionTarget) => {
      setReturnFocusTargetId(undefined);
      panelNavigation.navigatePanel(
        appendPanelLocation(location, executionTargetEntry(target.executionTargetId)),
      );
    },
    [location, panelNavigation],
  );

  const back = useCallback(() => {
    // Returning from a target screen focuses the target row that opened it.
    setReturnFocusTargetId(executionTargetId);
    panelNavigation.back();
  }, [executionTargetId, panelNavigation]);

  const openDeployment = useCallback(
    (deploymentEntry: PanelEntry) => {
      panelNavigation.navigatePanel(appendPanelLocation(location, deploymentEntry));
    },
    [location, panelNavigation],
  );

  const closeInspector = useCallback(() => {
    setReturnFocusTargetId(undefined);
    panelNavigation.close();
  }, [panelNavigation]);

  return (
    <>
      <InspectorShell
        body={
          selectedMachine && executionTargetId ? (
            <ExecutionTargetContent
              applicationDeployments={applicationDeployments}
              deployments={deployments}
              executionTargetId={executionTargetId}
              executionTargets={executionTargets}
              machineId={selectedMachine.machineId}
              machineName={selectedMachine.displayName || undefined}
              onOpenDeployment={openDeployment}
            />
          ) : displayedInspector?.detail ? (
            <MachineDetailContent
              applicationDeployments={applicationDeployments}
              connection={displayedInspector.machine.observationFreshness}
              deployments={deployments}
              detail={displayedInspector.detail}
              executionTargetFocus={executionTargetFocus}
              executionTargets={executionTargets}
              initialRange={initialRange}
              loadingStatus={displayedInspector.loadingStatus}
              machineId={displayedInspector.machine.machineId}
              machineName={displayedInspector.machine.displayName || undefined}
              metricsClient={metricsClient}
              nodeMemory={nodeMemory}
              onSelectExecutionTarget={selectExecutionTarget}
              siteManager={displayedInspector.machine.siteManager}
            />
          ) : null
        }
        focusKey={
          executionTargetId
            ? `execution-target:${executionTargetId}`
            : (displayedInspector?.machine.machineId ?? machineId)
        }
        id={machineInspectorPanelId}
        onBack={back}
        onOpenChange={(open) => {
          if (!open) closeInspector();
        }}
        // The URL opens the panel before the inventory is read, so a panel arriving from another
        // page never closes and reopens; the inventory then fills or recovers it.
        open={machineId !== undefined}
        openerRef={openerRef}
        title={
          selectedMachine && executionTargetId ? (
            <ResourceTitle
              colorKey={selectedMachine.machineId}
              // The generic name stands in until the targets read names this one.
              name={
                targets?.find(
                  (target) =>
                    target.siteNodeId === selectedMachine.machineId &&
                    target.executionTargetId === executionTargetId,
                )?.displayName ?? "Execution target"
              }
            />
          ) : displayedInspector ? (
            <ResourceTitle
              colorKey={displayedInspector.machine.machineId}
              name={displayedInspector.machine.displayName}
            />
          ) : machineId ? (
            <ResourceTitle
              colorKey={machineId}
              name={publishedTitle ?? (executionTargetId ? "Execution target" : "Machine details")}
            />
          ) : (
            "Machine details"
          )
        }
      />
    </>
  );
}

/** The machine a machine inspector shows, and its snapshot detail once its body may render. */
interface DisplayedMachineInspector {
  /** Absent while the selected machine's own detail is not read yet. */
  detail?: MachineSnapshotDetail;
  /** Whether a loading body states that it loads: only once delayed loading shows. */
  loadingStatus: boolean;
  machine: Machine;
}

/**
 * The machine inspector's displayed content. A newly selected machine replaces the shown one only
 * once its body is ready or its delayed loading is visible, so switching machines never empties
 * the panel mid-swap and never shows one machine's snapshot under another machine's title. A
 * first open has nothing to hold, so it shows the loading body at once, whose empty rows hold the
 * place of the rows to come.
 */
function useDisplayedMachineInspector(
  selectedMachine: Machine | undefined,
  detail: MachineSnapshotDetail | undefined,
  ready: boolean,
): DisplayedMachineInspector | undefined {
  const loadingVisible = useDelayedLoading(Boolean(selectedMachine) && !ready);
  const [held, setHeld] = useState<DisplayedMachineInspector>();
  const current =
    selectedMachine && detail && (ready || loadingVisible)
      ? {
          detail: loadingVisible ? loadingDetail : detail,
          loadingStatus: true,
          machine: selectedMachine,
        }
      : undefined;

  // Remembers the last shown content during render, so the next swap can keep it in view.
  if (current && (current.machine !== held?.machine || current.detail !== held.detail)) {
    setHeld(current);
  }
  if (!selectedMachine) {
    if (held) setHeld(undefined);
    return undefined;
  }

  return (
    current ??
    held ?? { detail: detail && loadingDetail, loadingStatus: false, machine: selectedMachine }
  );
}

const loadingDetail: MachineSnapshotDetail = { kind: "loading" };
const noMachines: readonly Machine[] = [];

/**
 * Whether a Machines panel entry still names a listed resource: a machine in the inventory, or a
 * target its parent machine owns. Entries stay available until the list that decides them is read.
 */
function isMachinePanelAvailable(
  entry: PanelEntry,
  parent: PanelEntry | undefined,
  machines: readonly Machine[] | undefined,
  targets: readonly ConfiguredExecutionTarget[] | undefined,
): boolean {
  if (entry.page !== "machines") return true;
  if (entry.screen === "machine") {
    return (
      machines === undefined || machines.some((machine) => machine.machineId === entry.resourceId)
    );
  }
  return (
    targets === undefined ||
    targets.some(
      (target) =>
        target.executionTargetId === entry.resourceId && target.siteNodeId === parent?.resourceId,
    )
  );
}

/** Whether a read has a value or a failed attempt, rather than a first read in flight. */
function isSettled(read: ReadState<unknown> | undefined): boolean {
  return read !== undefined && (read.value !== undefined || read.failed);
}

function executionTargetEntry(executionTargetId: string): PanelEntry {
  return { page: "machines", resourceId: executionTargetId, screen: "execution-target" };
}
