import { ListChecksIcon, ServerIcon } from "lucide-react";
import type { ReactNode } from "react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import { Tags } from "@/components/data-table/data-table-cells";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import type { DataTableDefinition } from "@/lib/data-table-model";
import { createDataTableViewCodec } from "@/lib/data-table-view";

import { applicationInspectorPanelId } from "./application-inspector-panel";
import {
  applicationUseLabel,
  applicationUses,
  type CatalogApplicationRow,
} from "./catalog-application-row";

/** A column of the Application Catalog table, in display order. */
export type CatalogApplicationColumnId =
  "application" | "version" | "uses" | "platforms" | "services";

/** A filter field of the Application Catalog table. */
export type CatalogApplicationField = "uses" | "platform";

/**
 * The application catalog as a data table: one row per application, without children, filtered on
 * what the application uses and the platforms it runs on.
 */
export const catalogApplicationTableDefinition: DataTableDefinition<
  CatalogApplicationRow,
  CatalogApplicationRow,
  CatalogApplicationColumnId,
  CatalogApplicationField
> = {
  columns: [
    {
      id: "application",
      key: "application",
      label: "Application",
      pinned: true,
      sort: { key: "name", kind: "name" },
    },
    { defaultVisible: true, id: "version", key: "version", label: "Version" },
    { id: "uses", key: "uses", label: "Uses" },
    { id: "platforms", key: "platforms", label: "Platforms" },
    {
      id: "services",
      key: "services",
      label: "Services",
      sort: { kind: "value", value: (application) => application.serviceCount },
    },
  ],
  defaultSort: { column: "application", direction: "asc" },
  fields: [
    {
      domain: {
        kind: "closed",
        values: applicationUses.map((value) => ({
          label: applicationUseLabel(value),
          value,
        })),
      },
      id: "uses",
      key: "uses",
      label: "Uses",
      // Uses add up: each selected one must hold.
      match: "all",
      values: (application) => application.uses,
    },
    {
      domain: { kind: "open" },
      id: "platform",
      key: "platform",
      label: "Platform",
      match: "any",
      values: (application) => application.platforms,
    },
  ],
  rowId: (application) => application.applicationId,
  rowName: (application) => application.displayName,
  searchText: (application) => [application.displayName, application.applicationId],
};

/** The Application Catalog view in the URL: `q`, `f`, `uses`, `platform`, `sort`, and `cols`. */
export const catalogApplicationViewCodec = createDataTableViewCodec(
  catalogApplicationTableDefinition,
);

/** The application catalog as a rendered data table. */
export const catalogApplicationTableUi: DataTableUiDefinition<
  CatalogApplicationRow,
  CatalogApplicationRow,
  CatalogApplicationColumnId,
  CatalogApplicationField
> = {
  codec: catalogApplicationViewCodec,
  columns: {
    application: { cell: ({ row }) => applicationCell(row) },
    platforms: { cell: ({ row }) => <Tags mono values={row.platforms} /> },
    uses: {
      cell: ({ row }) => <Tags values={row.uses.map(applicationUseLabel)} />,
    },
    services: { cell: ({ row }) => <span className="tabular-nums">{row.serviceCount}</span> },
    version: {
      cell: ({ row }) =>
        row.version ? (
          <span className="tabular-nums">{row.version}</span>
        ) : (
          <span className="text-muted-foreground">—</span>
        ),
    },
  },
  definition: catalogApplicationTableDefinition,
  fields: {
    platform: {
      icon: ServerIcon,
      renderValue: ({ label }) => <span className="truncate font-mono text-xs">{label}</span>,
    },
    uses: { icon: ListChecksIcon },
  },
  label: "Application catalog",
  noun: { one: "application", other: "applications" },
  panelId: applicationInspectorPanelId,
};

function applicationCell(application: CatalogApplicationRow): ReactNode {
  return (
    <>
      <div className="flex flex-wrap items-center gap-1.5 text-sm font-semibold">
        <span>
          <ResourceMarker colorKey={application.colorKey} />
          {application.displayName}
        </span>
        {application.runningStatus ? <StatusBadge status={application.runningStatus} /> : null}
      </div>
      {application.description ? (
        <div className="mt-0.5 text-xs text-muted-foreground">{application.description}</div>
      ) : null}
    </>
  );
}
