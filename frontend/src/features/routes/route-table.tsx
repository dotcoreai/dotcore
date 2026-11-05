import { ActivityIcon } from "lucide-react";
import { Fragment } from "react";

import type { DataTableUiDefinition } from "@/components/data-table/data-table";
import { formatChartValue } from "@/components/metrics/time-series-chart/chart-data";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import type { InferenceRowValues } from "@/features/inference/use-inference-metrics";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { DataTableDefinition } from "@/lib/data-table-model";
import { createDataTableViewCodec } from "@/lib/data-table-view";
import type { StatusProjection } from "@/lib/status-projection";

import type { RouteMemberMention } from "./route-inspector";
import { projectRouteHealth } from "./route-projection";
import { inspectorPanelId } from "./use-route-sidebar-host";

/** A column of the routes table, in display order. */
export type RouteColumnId =
  "route" | "status" | "members" | "requests" | "generatedTokens" | "tokenGeneration" | "ttft";

/** A filter field of the routes table. */
export type RouteField = "status";

/** One member of a route's fallback group. `ready` is absent until the controller reports. */
export interface RouteRowMember {
  id: string;
  name: string;
  ready?: boolean;
}

/** One route as the table shows it. */
export interface RouteRow {
  /** The members of each fallback group, in priority order. */
  groups: RouteRowMember[][];
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
    { defaultVisible: true, id: "members", key: "members", label: "Members" },
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
  searchText: (row) => [
    row.name,
    row.routeId,
    ...row.groups.flatMap((group) => group.map((member) => member.name)),
  ],
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
    members: { cell: ({ row }) => membersCell(row) },
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
 * console knows to their name; a member it cannot name shows its ID. `trafficOf` reads a route's
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
        groups: route.fallbackGroups.map((group, groupIndex) => {
          const observed = view.availability?.fallbackGroups.find(
            (candidate) => candidate.fallbackGroupIndex === groupIndex,
          );

          return group.modelDeploymentIds.map((id) => ({
            id,
            name: deploymentMentions.get(id)?.name ?? id,
            ready: observed?.members.find((member) => member.modelDeploymentId === id)?.eligible,
          }));
        }),
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

/**
 * The route's groups in priority order, members joined by commas and groups by arrows. A member the
 * controller reported as not eligible is struck through, with its state kept for assistive
 * technology since a strike-through is visual only.
 */
function membersCell(row: RouteRow) {
  const text = row.groups.map((group) => group.map((member) => member.name).join(", ")).join(" → ");

  return (
    <span className="block min-w-24 whitespace-normal" data-testid="route-flow" title={text}>
      {row.groups.map((group, groupIndex) => (
        <Fragment key={groupIndex}>
          {groupIndex > 0 ? " → " : null}
          {group.map((member, memberIndex) => (
            <Fragment key={member.id}>
              {memberIndex > 0 ? ", " : null}
              {member.ready === false ? (
                <span className="text-muted-foreground line-through" data-ready="false">
                  {member.name}
                  <span className="sr-only"> (not ready)</span>
                </span>
              ) : (
                <span>{member.name}</span>
              )}
            </Fragment>
          ))}
        </Fragment>
      ))}
    </span>
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
