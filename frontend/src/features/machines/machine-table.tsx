import { PlugIcon } from "lucide-react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import type { DataTableDefinition } from "@/components/data-table/data-table-model";
import { createDataTableViewCodec } from "@/components/data-table/data-table-view";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { nodeConnectionStatus } from "@/features/deployments/execution-target";
import { machineName } from "@/features/memory/described-machines";
import type { MachineNow } from "@/features/overview/machine-site-values";
import { formatReading, formatUsed } from "@/features/overview/overview-machine-specs";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { type Machine, NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";
import { formatCount } from "@/lib/format";
import type { StatusProjection } from "@/lib/status-projection";

import { machineInspectorPanelId } from "./machine-panels";

/** A column of the machines table, in display order. */
export type MachineColumnId = "machine" | "connection" | "cpu" | "ram" | "vram" | "running";

/** A filter field of the machines table. */
export type MachineField = "connection";

/** One machine as the table shows it. */
export interface MachineRow {
  applications: number;
  connection: StatusProjection;
  machineId: string;
  models: number;
  name: string;
  /** The newest readings; absent unless the machine is connected. */
  now?: MachineNow;
  /** The machine's hardware in a line, or why there is none. */
  summary?: string;
}

/** The facts a table row combines for each machine. */
export interface MachineRowSources {
  applications: readonly ApplicationDeployment[] | undefined;
  models: readonly ModelDeployment[] | undefined;
  /** The newest readings by machine id; absent until the first answer. */
  now: ReadonlyMap<string, MachineNow> | undefined;
  /** The hardware line of each machine that has one, or why it has none. */
  summaries: ReadonlyMap<string, string>;
}

/**
 * The machines as a data table: one row per machine, filtered on its status. Status sorts by its
 * label, CPU, RAM and VRAM sort by the share in use, and Running by the number of deployments.
 */
export const machineTableDefinition: DataTableDefinition<
  MachineRow,
  MachineRow,
  MachineColumnId,
  MachineField
> = {
  columns: [
    { id: "machine", key: "machine", label: "Machine", pinned: true, sort: { kind: "name" } },
    {
      defaultVisible: true,
      id: "connection",
      key: "connection",
      label: "Status",
      sort: { kind: "text", value: (machine) => machine.connection.label },
    },
    {
      defaultVisible: true,
      id: "cpu",
      key: "cpu",
      label: "CPU",
      sort: { kind: "value", value: (machine) => machine.now?.cpu },
    },
    {
      defaultVisible: true,
      id: "ram",
      key: "ram",
      label: "RAM",
      sort: {
        kind: "value",
        value: (machine) => share(machine.now?.ramUsed, machine.now?.ramTotal),
      },
    },
    {
      defaultVisible: true,
      id: "vram",
      key: "vram",
      label: "VRAM",
      sort: {
        kind: "value",
        value: (machine) => share(machine.now?.vramUsed, machine.now?.vramTotal),
      },
    },
    {
      defaultVisible: true,
      id: "running",
      key: "running",
      label: "Running",
      sort: { kind: "value", value: (machine) => machine.models + machine.applications },
    },
  ],
  defaultSort: { column: "machine", direction: "asc" },
  fields: [
    {
      domain: { kind: "open" },
      id: "connection",
      key: "connection",
      label: "Status",
      match: "any",
      values: (machine) => [machine.connection.label],
    },
  ],
  rowId: (machine) => machine.machineId,
  rowName: (machine) => machine.name,
  searchText: (machine) => [machine.name, machine.machineId],
};

/** The machines table view in the URL: `q`, `f`, `connection`, `sort`, and `cols`. */
export const machineViewCodec = createDataTableViewCodec(machineTableDefinition);

/** The machines as a rendered data table. */
export const machineTableUi: DataTableUiDefinition<
  MachineRow,
  MachineRow,
  MachineColumnId,
  MachineField
> = {
  codec: machineViewCodec,
  columns: {
    connection: { cell: ({ row }) => <StatusBadge status={row.connection} /> },
    cpu: { cell: ({ row }) => figure(formatReading("ratio", row.now?.cpu)), showFrom: "56rem" },
    machine: { cell: ({ row }) => machineCell(row) },
    ram: {
      cell: ({ row }) => figure(formatUsed(row.now?.ramUsed, row.now?.ramTotal)),
      showFrom: "46rem",
    },
    running: { cell: ({ row }) => figure(runningText(row)), showFrom: "32rem" },
    vram: {
      cell: ({ row }) => figure(formatUsed(row.now?.vramUsed, row.now?.vramTotal)),
      showFrom: "40rem",
    },
  },
  definition: machineTableDefinition,
  fields: { connection: { icon: PlugIcon } },
  label: "Machine inventory",
  noun: { one: "machine", other: "machines" },
  panelId: machineInspectorPanelId,
};

/**
 * The table rows of the listed machines. Readings show for connected machines only: a stale or
 * disconnected machine's last values would read as current.
 */
export function machineRows(
  machines: readonly Machine[],
  sources: MachineRowSources,
): MachineRow[] {
  return machines.map((machine) => {
    const state = machine.observationFreshness?.state;

    return {
      applications: countPlacedOn(
        machine.machineId,
        sources.applications?.map((item) => item.placement?.siteNodeId),
      ),
      connection: nodeConnectionStatus(state),
      machineId: machine.machineId,
      models: countPlacedOn(
        machine.machineId,
        sources.models?.map((item) => item.intent?.placement?.siteNodeId),
      ),
      name: machineName(machine),
      now:
        state === NodeObservationState.CONNECTED ? sources.now?.get(machine.machineId) : undefined,
      summary: sources.summaries.get(machine.machineId),
    };
  });
}

function machineCell(machine: MachineRow) {
  return (
    <>
      <div className="text-sm font-semibold">
        <ResourceMarker colorKey={machine.machineId} />
        {machine.name}
      </div>
      {machine.summary ? (
        <div className="mt-0.5 text-xs text-muted-foreground">{machine.summary}</div>
      ) : null}
    </>
  );
}

function figure(text: string) {
  return text === "—" ? (
    <span className="text-muted-foreground">—</span>
  ) : (
    <span className="tabular-nums">{text}</span>
  );
}

function runningText(machine: MachineRow): string {
  const parts = [
    machine.models ? formatCount(machine.models, "model") : undefined,
    machine.applications ? formatCount(machine.applications, "app") : undefined,
  ].filter((part) => part !== undefined);

  return parts.length ? parts.join(" · ") : "—";
}

function countPlacedOn(machineId: string, nodeIds: readonly (string | undefined)[] | undefined) {
  return nodeIds?.filter((nodeId) => nodeId === machineId).length ?? 0;
}

function share(used: number | undefined, total: number | undefined): number | undefined {
  return used === undefined || !total ? undefined : used / total;
}
