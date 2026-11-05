import { ActivityIcon, ServerIcon } from "lucide-react";
import type { MouseEvent } from "react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import type { DataTableDefinition } from "@/lib/data-table-model";
import { createDataTableViewCodec } from "@/lib/data-table-view";
import { formatBytes } from "@/lib/format";

import type { ApplicationDeploymentRow } from "./application-deployment-row";
import { applicationInspectorPanelId } from "./application-inspector-panel";

/** A column of the Apps table, in display order. */
export type ApplicationDeploymentColumnId =
  "app" | "status" | "machine" | "route" | "endpoint" | "vram";

/** A filter field of the Apps table. */
export type ApplicationDeploymentField = "status" | "machine";

/**
 * The installed application deployments as a data table: one row per deployment, without
 * children, filtered on the projected status and the machine's name.
 */
export const applicationDeploymentTableDefinition: DataTableDefinition<
  ApplicationDeploymentRow,
  ApplicationDeploymentRow,
  ApplicationDeploymentColumnId,
  ApplicationDeploymentField
> = {
  columns: [
    { id: "app", key: "app", label: "App", pinned: true, sort: { key: "name", kind: "name" } },
    {
      defaultVisible: true,
      id: "status",
      key: "status",
      label: "Status",
      sort: { kind: "text", value: (row) => row.status.label },
    },
    {
      defaultVisible: true,
      id: "machine",
      key: "machine",
      label: "Machine",
      sort: { kind: "text", value: (row) => row.machineName },
    },
    { defaultVisible: true, id: "route", key: "route", label: "Route" },
    { defaultVisible: true, id: "endpoint", key: "endpoint", label: "Endpoint" },
    {
      defaultVisible: true,
      id: "vram",
      key: "vram",
      label: "VRAM",
      sort: {
        kind: "value",
        value: (row) => (row.vramBytes === undefined ? undefined : Number(row.vramBytes)),
      },
    },
  ],
  defaultSort: { column: "app", direction: "asc" },
  fields: [
    {
      domain: { kind: "open" },
      id: "status",
      key: "status",
      label: "Status",
      match: "any",
      values: (row) => [row.status.label],
    },
    {
      domain: { kind: "open" },
      id: "machine",
      key: "machine",
      label: "Machine",
      match: "any",
      values: (row) => (row.machineName ? [row.machineName] : []),
    },
  ],
  rowId: (row) => row.deploymentId,
  rowName: (row) => row.displayName,
  searchText: (row) => [
    row.displayName,
    row.applicationName ?? "",
    row.machineName ?? "",
    row.routeName ?? "",
    row.deploymentId,
  ],
};

/** The Apps view in the URL: `q`, `f`, `status`, `machine`, `sort`, and `cols`. */
export const applicationDeploymentViewCodec = createDataTableViewCodec(
  applicationDeploymentTableDefinition,
);

/** The application deployments as a rendered data table. */
export const applicationDeploymentTableUi: DataTableUiDefinition<
  ApplicationDeploymentRow,
  ApplicationDeploymentRow,
  ApplicationDeploymentColumnId,
  ApplicationDeploymentField
> = {
  codec: applicationDeploymentViewCodec,
  columns: {
    app: { cell: ({ row }) => appCell(row) },
    endpoint: { cell: ({ row }) => endpointCell(row), showFrom: "34rem" },
    machine: { cell: ({ row }) => machineCell(row), showFrom: "32rem" },
    route: { cell: ({ row }) => textOrDash(row.routeName), showFrom: "40rem" },
    status: { cell: ({ row }) => <StatusBadge status={row.status} /> },
    vram: {
      cell: ({ row }) =>
        row.vramBytes === undefined ? (
          dash
        ) : (
          <span className="tabular-nums">{formatBytes(row.vramBytes)}</span>
        ),
      showFrom: "46rem",
    },
  },
  definition: applicationDeploymentTableDefinition,
  fields: {
    machine: { icon: ServerIcon },
    status: { icon: ActivityIcon },
  },
  label: "Installed applications",
  noun: { one: "app", other: "apps" },
  panelId: applicationInspectorPanelId,
};

const dash = <span className="text-muted-foreground">—</span>;

function textOrDash(value: string | undefined) {
  return value ? <span>{value}</span> : dash;
}

function appCell(row: ApplicationDeploymentRow) {
  const detail = [row.applicationName, row.version].filter(Boolean).join(" · ");

  return (
    <>
      <div className="text-sm font-semibold">
        <ResourceMarker colorKey={row.deploymentId} />
        {row.displayName}
      </div>
      {detail ? <div className="mt-0.5 text-xs text-muted-foreground">{detail}</div> : null}
    </>
  );
}

function machineCell(row: ApplicationDeploymentRow) {
  if (!row.machineId || !row.machineName) return dash;

  return (
    <span>
      <ResourceMarker colorKey={row.machineId} />
      {row.machineName}
    </span>
  );
}

function endpointCell(row: ApplicationDeploymentRow) {
  if (!row.endpointUrl) return dash;

  return (
    <a
      aria-label={`Open ${row.displayName}`}
      className="text-primary underline-offset-2 hover:underline"
      href={row.endpointUrl}
      // The row opens the panel on click; following the link must not.
      onClick={(event: MouseEvent) => event.stopPropagation()}
      rel="noopener noreferrer"
      target="_blank"
    >
      Open
    </a>
  );
}
