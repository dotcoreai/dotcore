import { useEffect, useMemo, useRef } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { type ModelClient, modelClient } from "@/api/model-client";
import { usePanelNavigation } from "@/app/console-layout";
import { ModelsSectionIcon } from "@/app/console-section-icons";
import {
  useWatchedApplicationDeployments,
  useWatchedMachines,
  useWatchedNodeMemory,
} from "@/app/resource-watch";
import { DeployActionIcon } from "@/components/ui/action-icons";
import { AddAction } from "@/components/ui/add-action";
import { EmptyState } from "@/components/ui/empty-state";
import {
  PageActions,
  PageHeader,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { ResourceCard } from "@/components/ui/resource-card";
import { MemoryHoverProvider } from "@/features/memory/memory-hover";
import {
  useDeploymentHoverState,
  useMemoryHoverSetter,
  useReleaseHoverOnUnmount,
} from "@/features/memory/memory-hover-store";
import { useHardwareSnapshots } from "@/features/memory/use-hardware-snapshots";
import { useMemoryHistory } from "@/features/memory/use-memory-history";
import { useMemoryView } from "@/features/memory/use-memory-view";
import { siteMemoryState } from "@/features/memory/use-site-memory";
import { modelInspectorPanelId } from "@/features/models/model-inspector-panel";
import {
  localModelSnapshot,
  modelDeploymentPresentationName,
  projectModelDeploymentStatus,
} from "@/features/models/model-projection";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import {
  activePanel,
  canonicalizePanelLocation,
  panelPagePath,
  selectPanelLocation,
} from "@/lib/panel-stack";
import { mapRead } from "@/lib/read-state";
import { upstreamLocalityLabel } from "@/lib/upstream-presentation";

import { DeploymentSidebar } from "./deployment-sidebar";
import { ModelsMemory } from "./models-memory";
import { useSharedModelsPolling } from "./models-polling-context";

export interface ModelsPageProps {
  client?: ModelClient;
  metricsClient?: MetricsClient;
}

export function ModelsPage(props: ModelsPageProps) {
  // Links hovered memory segments and running cards without re-rendering the page on hover.
  return (
    <MemoryHoverProvider>
      <ModelsPageContent {...props} />
    </MemoryHoverProvider>
  );
}

function ModelsPageContent({
  client = modelClient,
  metricsClient = defaultMetricsClient,
}: ModelsPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const panelNavigation = usePanelNavigation();
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const urlEntry = activePanel(canonical.search);
  const selectedDeploymentId = urlEntry?.page === "models" ? urlEntry.resourceId : undefined;
  const { deploymentsFailed, error: pollingError, snapshot } = useSharedModelsPolling();
  const loadingFeedbackVisible = useDelayedLoading(!snapshot);
  const [memoryView, setMemoryView] = useMemoryView();
  // A failed or reconnecting deployment read may leave segments outdated.
  const machines = useWatchedMachines();
  const machineList = machines?.value?.machines;
  const snapshotDetails = useHardwareSnapshots(machineList ?? noMachines);
  const watchedApplications = useWatchedApplicationDeployments();
  const applications = watchedApplications?.value?.deployments;
  const watchedNodeMemory = useWatchedNodeMemory();
  const nodeMemory = useMemo(
    () => mapRead(watchedNodeMemory, (value) => value.nodes),
    [watchedNodeMemory],
  );
  const deploymentsReadFailed =
    Boolean(pollingError) || deploymentsFailed || Boolean(watchedApplications?.failed);
  const siteMemory = useMemo(
    () =>
      siteMemoryState({
        applications,
        deployments: snapshot?.runningModels,
        deploymentsReadFailed,
        machines: machineList,
        machinesReadFailed: machines?.failed ?? false,
        nodeMemory,
      }),
    [
      applications,
      deploymentsReadFailed,
      machineList,
      machines?.failed,
      nodeMemory,
      snapshot?.runningModels,
    ],
  );
  const memoryHistory = useMemoryHistory(machineList, snapshotDetails, snapshot?.runningModels);
  const openerRef = useRef<HTMLElement | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    if (canonical.recovered || (urlEntry && !selectedDeploymentId)) {
      void navigate(canonical, { replace: true });
    }
  }, [canonical, navigate, selectedDeploymentId, urlEntry]);

  const select = (modelDeploymentId: string, opener: HTMLElement) => {
    openerRef.current = opener;
    panelNavigation.navigatePanel(
      selectPanelLocation(location, {
        page: "models",
        resourceId: modelDeploymentId,
        screen: "deployment",
      }),
    );
  };

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={ModelsSectionIcon} ref={headingRef} tabIndex={-1}>
            Models
          </PageHeaderHeading>
          <PageActions>
            <AddAction icon={DeployActionIcon} label="Deploy" to={panelPagePath("model-catalog")} />
          </PageActions>
        </PageHeader>

        <ModelsMemory
          history={memoryHistory}
          machineCount={machineList?.length ?? 0}
          metricsClient={metricsClient}
          onViewChange={setMemoryView}
          state={siteMemory}
          view={memoryView}
        />

        {loadingFeedbackVisible && !pollingError ? (
          <p className="text-sm text-muted-foreground" role="status">
            Loading models…
          </p>
        ) : null}

        {pollingError ? (
          <p className="text-sm text-destructive" role="alert">
            {pollingError}
          </p>
        ) : null}

        {snapshot && !loadingFeedbackVisible ? (
          <>
            <section aria-labelledby="running-models-heading" className="flex flex-col gap-4">
              <div>
                <PageSectionHeading id="running-models-heading">Running</PageSectionHeading>
              </div>
              {snapshot.runningModels.length > 0 ? (
                <ul aria-label="Running models" className="inspector-card-grid">
                  {snapshot.runningModels.map((deployment) => {
                    return (
                      <RunningModelCard
                        key={deployment.modelDeploymentId}
                        deployment={deployment}
                        duplicateName={
                          snapshot.runningModels.filter(
                            (candidate) =>
                              modelDeploymentPresentationName(candidate) ===
                              modelDeploymentPresentationName(deployment),
                          ).length > 1
                        }
                        onSelect={(opener) => {
                          select(deployment.modelDeploymentId, opener);
                        }}
                        selected={selectedDeploymentId === deployment.modelDeploymentId}
                      />
                    );
                  })}
                </ul>
              ) : (
                <EmptyState
                  action={
                    <AddAction
                      icon={DeployActionIcon}
                      label="Deploy"
                      to={panelPagePath("model-catalog")}
                    />
                  }
                  description="Deploy a model from the catalog to start serving it on one of your machines."
                  icon={ModelsSectionIcon}
                  title="No models running"
                />
              )}
            </section>
          </>
        ) : null}

        <DeploymentSidebar
          client={client}
          entry={urlEntry}
          fallbackFocusRef={headingRef}
          metricsClient={metricsClient}
          openerRef={openerRef}
        />
      </div>
    </div>
  );
}

interface RunningModelCardProps {
  deployment: ModelDeployment;
  duplicateName: boolean;
  onSelect: (opener: HTMLElement) => void;
  selected: boolean;
}

function RunningModelCard({
  deployment,
  duplicateName,
  onSelect,
  selected,
}: RunningModelCardProps) {
  const model = localModelSnapshot(deployment.snapshot);
  const upstream =
    deployment.snapshot?.source.case === "upstream" ? deployment.snapshot.source.value : undefined;
  const presentationName = modelDeploymentPresentationName(deployment);
  const accessibleName =
    upstream || duplicateName
      ? `${presentationName} (${deployment.modelDeploymentId})`
      : presentationName;

  const hoverState = useDeploymentHoverState(deployment.modelDeploymentId);
  const setHovered = useMemoryHoverSetter();
  useReleaseHoverOnUnmount(deployment.modelDeploymentId);

  return (
    <li
      className="min-w-0 rounded-lg transition-opacity data-[memory-hover=active]:ring-1 data-[memory-hover=active]:ring-primary data-[memory-hover=dimmed]:opacity-45"
      data-memory-hover={hoverState}
      onPointerEnter={() => setHovered(deployment.modelDeploymentId)}
      onPointerLeave={() => setHovered(undefined)}
    >
      <ResourceCard
        accessibleName={accessibleName}
        colorKey={deployment.modelDeploymentId}
        description={
          upstream
            ? upstreamDeploymentDescription(deployment)
            : localDeploymentDescription(model?.modelDisplayName, model?.profileId)
        }
        name={presentationName}
        onSelect={onSelect}
        panelId={modelInspectorPanelId}
        selected={selected}
        status={projectModelDeploymentStatus(deployment)}
      />
    </li>
  );
}

function localDeploymentDescription(
  modelDisplayName?: string,
  profileId?: string,
): string | undefined {
  const parts = [modelDisplayName, profileId].filter(Boolean);
  return parts.length ? parts.join(" · ") : undefined;
}

/**
 * An upstream deployment names what it actually is: an external model, the provider serving it, and
 * where that provider runs. The kind leads the description line rather than occupying a second badge.
 */
function upstreamDeploymentDescription(deployment: ModelDeployment): string {
  const boundary = deployment.upstreamDesiredConfiguration?.boundary;
  const locality = boundary ? upstreamLocalityLabel(boundary.locality) : undefined;
  return ["External", boundary?.trustDomain, locality].filter(Boolean).join(" · ");
}

const noMachines: readonly Machine[] = [];
