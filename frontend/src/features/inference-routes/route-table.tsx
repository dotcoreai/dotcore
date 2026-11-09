import { ActivityIcon } from "lucide-react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import type { DataTableDefinition } from "@/components/data-table/data-table-model";
import { createDataTableViewCodec } from "@/components/data-table/data-table-view";
import { formatChartValue } from "@/components/metrics/time-series-chart/chart-data";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { projectRouteHealth } from "@/domain/route-projection";
import type { InferenceRowValues } from "@/features/metrics/inference/use-inference-metrics";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { StatusProjection } from "@/lib/status-projection";

import type { RouteMemberMention } from "./route-inspector";
import { inspectorPanelId } from "./use-route-sidebar-host";

/** A column of the routes table, in display order. */
export type RouteColumnId =
  "route" | "status" | "requests" | "generatedTokens" | "tokenGeneration" | "ttft";

/** A filter field of the routes table. */
export type RouteField = "status";

/** One route as the table shows it. */
export interface RouteRow {
  /** The names of the models in the route's fallback groups, searchable though not shown. */
  memberNames: string[];
  name: string;
  routeId: string;
  status: StatusProjection;
  /** The route's traffic over the page range; absent until the first read. */
  traffic?: InferenceRowValues;
}

/**
 * The routes as a data table: one row per route, filtered on the projected status. Status sorts by
 * its label and the traffic columns by their value over the range.
 */
export const routeTableDefinition: DataTableDefinition<
  RouteRow,
  RouteRow,
  RouteColumnId,
  RouteField
> = {
  columns: [
    { id: "route", key: "route", label: "Route", pinned: true, sort: { kind: "name" } },
    {
      defaultVisible: true,
      id: "status",
      key: "status",
      label: "Status",
      sort: { kind: "text", value: (row) => row.status.label },
    },
    {
      defaultVisible: true,
      id: "requests",
      key: "requests",
      label: "Requests",
      sort: { kind: "value", value: (row) => row.traffic?.requests },
    },
    {
      defaultVisible: true,
      id: "generatedTokens",
      key: "generated-tokens",
      label: "Generated tokens",
      sort: { kind: "value", value: (row) => row.traffic?.generatedTokens },
    },
    {
      defaultVisible: true,
      id: "tokenGeneration",
      key: "token-generation",
      label: "Token generation",
      sort: { kind: "value", value: (row) => row.traffic?.tokenGeneration ?? undefined },
    },
    {
      id: "ttft",
      key: "ttft",
      label: "Time to first token",
      sort: { kind: "value", value: (row) => row.traffic?.ttft ?? undefined },
    },
  ],
  defaultSort: { column: "route", direction: "asc" },
  fields: [
    {
      domain: { kind: "open" },
      id: "status",
      key: "status",
      label: "Status",
      match: "any",
      values: (row) => [row.status.label],
    },
  ],
  rowId: (row) => row.routeId,
  rowName: (row) => row.name,
  searchText: (row) => [row.name, row.routeId, ...row.memberNames],
};

/** The routes table view in the URL: `q`, `f`, `status`, `sort`, and `cols`. */
export const routeViewCodec = createDataTableViewCodec(routeTableDefinition);

/** The routes as a rendered data table. */
export const routeTableUi: DataTableUiDefinition<RouteRow, RouteRow, RouteColumnId, RouteField> = {
  codec: routeViewCodec,
  columns: {
    generatedTokens: {
      cell: ({ row }) =>
        figure(row.traffic && formatChartValue("requests", row.traffic.generatedTokens)),
      showFrom: "56rem",
    },
    requests: {
      cell: ({ row }) => figure(row.traffic && formatChartValue("requests", row.traffic.requests)),
      showFrom: "46rem",
    },
    route: { cell: ({ row }) => routeCell(row) },
    status: { cell: ({ row }) => <StatusBadge status={row.status} /> },
    tokenGeneration: {
      cell: ({ row }) => figure(optional("tokensPerSecond", row.traffic?.tokenGeneration)),
      showFrom: "64rem",
    },
    ttft: { cell: ({ row }) => figure(optional("seconds", row.traffic?.ttft)) },
  },
  definition: routeTableDefinition,
  fields: { status: { icon: ActivityIcon } },
  label: "Inference routes",
  noun: { one: "route", other: "routes" },
  panelId: inspectorPanelId,
};

/**
 * The table rows of the listed routes. `deploymentMentions` maps the model deployment IDs the
 * console knows to their name; a member it cannot name is searchable by its ID. `trafficOf` reads a route's
 * traffic by its ID.
 */
export function routeRows(
  views: readonly InferenceRouteView[],
  deploymentMentions: ReadonlyMap<string, RouteMemberMention>,
  trafficOf: (routeId: string) => InferenceRowValues | undefined,
): RouteRow[] {
  return views.flatMap((view) => {
    const route = view.route;
    if (!route) return [];

    return [
      {
        memberNames: route.fallbackGroups.flatMap((group) =>
          group.modelDeploymentIds.map((id) => deploymentMentions.get(id)?.name ?? id),
        ),
        name: route.name,
        routeId: route.inferenceRouteId,
        status: projectRouteHealth(view).status,
        traffic: trafficOf(route.inferenceRouteId),
      },
    ];
  });
}

function routeCell(row: RouteRow) {
  return (
    <div className="text-sm font-semibold">
      <ResourceMarker colorKey={row.routeId} />
      {row.name}
    </div>
  );
}

function optional(unit: "seconds" | "tokensPerSecond", value: number | null | undefined) {
  return value === null || value === undefined ? undefined : formatChartValue(unit, value);
}

function figure(text: string | undefined) {
  return text === undefined ? (
    <span className="text-muted-foreground">—</span>
  ) : (
    <span className="tabular-nums">{text}</span>
  );
}
