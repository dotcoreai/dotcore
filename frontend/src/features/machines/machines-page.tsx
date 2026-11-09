import { useMemo, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { DataTable } from "@/components/data-table/data-table";
import { buildDataTable } from "@/components/data-table/data-table-model";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { AddAction } from "@/components/ui/add-action";
import { Notice } from "@/components/ui/notice";
import {
  PageActions,
  PageHeader,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { MachinesSectionIcon } from "@/components/ui/section-icons";
import { AddMachineDialog } from "@/features/machines/add-machine-dialog";
import { useHardwareSnapshots } from "@/features/memory/use-hardware-snapshots";
import { useMemoryView } from "@/features/memory/use-memory-view";
import { useMachinesNow } from "@/features/overview/use-machines-now";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { mapRead } from "@/lib/read-state";
import { usePanelNavigation } from "@/panels/panel-navigation";
import {
  useWatchedApplicationDeployments,
  useWatchedMachines,
  useWatchedModelDeployments,
  useWatchedNodeMemory,
} from "@/resource-watch/resource-watch";

import { inspectedMachineId, machineSelectionLocation } from "./machine-panels";
import { machineSummary } from "./machine-resources";
import { MachineSidebarHost } from "./machine-sidebar-host";
import {
  machineRows,
  machineTableDefinition,
  machineTableUi,
  machineViewCodec,
} from "./machine-table";
import { MachinesResources } from "./machines-resources";
import { useMachineFleet } from "./use-machine-fleet";
import { useMachineResources } from "./use-machine-resources";

export interface MachinesPageProps {
  metricsClient?: MetricsClient;
}

export function MachinesPage({ metricsClient = defaultMetricsClient }: MachinesPageProps) {
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const [addMachineOpen, setAddMachineOpen] = useState(false);
  const openerRef = useRef<HTMLElement>(null);
  // The inventory streams from the resource watch, so a machine that connects, goes stale, or
  // disconnects reaches the page without a reload.
  const inventory = useWatchedMachines();
  const machines = inventory?.value?.machines;
  // A failed watch keeps the last inventory; only one that never delivered shows the error.
  const inventoryErrorMessage =
    !machines && inventory?.failed ? inventoryUnavailableMessage : undefined;
  const inventoryLoadingVisible = useDelayedLoading(!machines && !inventoryErrorMessage);
  // Every machine's snapshot feeds the resources overview and the cards.
  const snapshots = useHardwareSnapshots(machines ?? noMachines);
  // Node memory draws the resources overview's memory rows; deployments name its segments.
  const watchedDeployments = useWatchedModelDeployments();
  const deployments = useMemo(
    () => mapRead(watchedDeployments, (value) => value.deployments),
    [watchedDeployments],
  );
  const watchedApplications = useWatchedApplicationDeployments();
  const applications = useMemo(
    () => mapRead(watchedApplications, (value) => value.deployments),
    [watchedApplications],
  );
  const watchedNodeMemory = useWatchedNodeMemory();
  const nodeMemory = useMemo(
    () => mapRead(watchedNodeMemory, (value) => value.nodes),
    [watchedNodeMemory],
  );
  const resources = useMachineResources(
    metricsClient,
    machines,
    inventory?.failed ?? false,
    snapshots,
    deployments,
    applications,
    nodeMemory,
  );
  const [view, setView] = useMemoryView();
  const fleet = useMachineFleet(machines, snapshots);
  const now = useMachinesNow(metricsClient, fleet.facts);
  const tableView = useDataTableView(machineViewCodec);
  const summaries = useMemo(
    () =>
      new Map<string, string>([
        ...resources.counted.map((entry): [string, string] => [
          entry.machineId,
          machineSummary(entry.snapshot),
        ]),
        ...resources.uncounted.map((entry): [string, string] => [
          entry.machineId,
          "Hardware unavailable",
        ]),
      ]),
    [resources.counted, resources.uncounted],
  );
  const machineTable = useMemo(
    () =>
      buildDataTable(
        machineTableDefinition,
        machineRows(machines ?? noMachines, {
          applications: applications?.value,
          models: deployments?.value,
          now: now?.machines,
          summaries,
        }),
        tableView.state,
      ),
    [applications, deployments, machines, now, summaries, tableView.state],
  );
  const selectedMachineId = inspectedMachineId(location.search);

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={MachinesSectionIcon} tabIndex={-1}>
            Machines
          </PageHeaderHeading>
          <PageActions>
            <AddAction label="Add machine" onClick={() => setAddMachineOpen(true)} />
          </PageActions>
        </PageHeader>
        <AddMachineDialog onOpenChange={setAddMachineOpen} open={addMachineOpen} />
        {/* Drawn while the list loads too, so its rows hold their place; an empty site has
                  none. */}
        {!inventoryErrorMessage && machines?.length !== 0 ? (
          <MachinesResources
            machineCount={machines?.length ?? 0}
            onViewChange={setView}
            resources={resources}
            view={view}
          />
        ) : null}
        <section aria-labelledby="machine-inventory-heading" className="flex flex-col gap-4">
          <div>
            <PageSectionHeading id="machine-inventory-heading">Inventory</PageSectionHeading>
          </div>
          {inventoryLoadingVisible ? (
            <p className="text-sm text-muted-foreground" role="status">
              Loading machines…
            </p>
          ) : null}
          {inventoryErrorMessage ? (
            <p className="text-sm text-destructive" role="alert">
              {inventoryErrorMessage}
            </p>
          ) : null}
          {!inventoryLoadingVisible && machines?.length === 0 ? (
            <Notice role="status">No machines are configured for this site.</Notice>
          ) : null}
          {!inventoryLoadingVisible && machines?.length ? (
            <div className="flex min-w-0 flex-col gap-3">
              <DataTableToolbar
                facets={machineTable.facets}
                onViewChange={tableView.set}
                ui={machineTableUi}
                view={tableView.state}
              />
              <DataTable
                onOpenRow={({ row }, opener) => {
                  openerRef.current = opener;
                  panelNavigation.navigatePanel(machineSelectionLocation(location, row.machineId));
                }}
                onViewChange={tableView.set}
                selection={selectedMachineId ? { rowId: selectedMachineId } : undefined}
                table={machineTable}
                ui={machineTableUi}
                view={tableView.state}
              />
            </div>
          ) : null}
        </section>
        <MachineSidebarHost metricsClient={metricsClient} openerRef={openerRef} />
      </div>
    </div>
  );
}

const noMachines: readonly Machine[] = [];
const inventoryUnavailableMessage =
  "The machines request failed: the resource watch is unavailable.";
