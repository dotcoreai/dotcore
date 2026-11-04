import type { RefObject } from "react";

import {
  useWatchedExecutionTargets,
  useWatchedMachines,
  useWatchedModelDeployments,
  useWatchedRoutes,
} from "@/app/resource-watch";
import { formatChartValue } from "@/components/metrics/time-series-chart/chart-data";
import { PageSectionHeading } from "@/components/ui/page-header";
import { placementLabel, placementMachine } from "@/features/deployments/placement";
import { dimensionNames } from "@/features/metrics/metric-names";
import { requestCardIds } from "@/features/metrics/request-card-specs";
import type { MetricCards, MetricCardUnit } from "@/features/metrics/use-metric-cards";
import { modelDeploymentPresentationName } from "@/features/models/model-projection";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";

import { overviewSplitIds } from "./overview-card-specs";
import { OverviewTable, type OverviewTableColumn, type OverviewTableRow } from "./overview-table";

const columns: OverviewTableColumn[] = [
  { key: requestCardIds.requests, label: "Requests" },
  { key: requestCardIds.promptTokens, label: "Processed tokens", minWidth: "@sm:table-cell" },
  { key: requestCardIds.generatedTokens, label: "Generated tokens", minWidth: "@md:table-cell" },
];

/**
 * The overview's models and routes tables, side by side on wide screens. A row per current
 * deployment and per route shows its figures over the page range, read from the page's one metric
 * stream; one with no traffic reads zero. A row opens the same sidebar as the models and routes
 * pages.
 */
export function OverviewTablesSection({
  metrics,
  openerRef,
}: {
  metrics: MetricCards;
  /** Receives the row that opened the sidebar, so focus returns to it when the sidebar closes. */
  openerRef: RefObject<HTMLElement | null>;
}) {
  const deployments = useWatchedModelDeployments()?.value?.deployments;
  const routes = useWatchedRoutes()?.value?.routes;
  const machines = useWatchedMachines()?.value?.machines;
  const targets = useWatchedExecutionTargets()?.value?.executionTargets;
  const units = new Map(metrics.cards.map((card) => [card.id, card.unit]));
  const read = metrics.cards.some((card) => card.reading !== undefined);

  const figures = (splitId: string, groupBy: string, id: string) =>
    columns.map((column) => {
      const unit = units.get(column.key);
      if (!read || !unit) return "—";

      const group = metrics
        .groupValues(splitId, column.key)
        .find((entry) => entry.labels[groupBy] === id);
      // A group missing from the answer had no traffic: no requests, but no speed either.
      const value = group?.value ?? (unit === "count" ? 0 : null);
      return formatFigure(unit, value);
    });

  const deploymentRows = (deployments ?? []).map((deployment): OverviewTableRow => {
    const upstream = deployment.snapshot?.source.case === "upstream";
    const placement = deployment.intent?.placement;

    return {
      cells: figures(
        overviewSplitIds.model,
        dimensionNames.deployment,
        deployment.modelDeploymentId,
      ),
      colorKey: deployment.modelDeploymentId,
      entry: { page: "models", resourceId: deployment.modelDeploymentId, screen: "deployment" },
      id: deployment.modelDeploymentId,
      name: modelDeploymentPresentationName(deployment),
      subtitle: upstream
        ? "upstream"
        : placementLabel(placement, placementMachine(machines, placement?.siteNodeId), targets),
    };
  });
  const deploymentNames = new Map(
    (deployments ?? []).map((deployment) => [
      deployment.modelDeploymentId,
      modelDeploymentPresentationName(deployment),
    ]),
  );
  const routeRows = (routes ?? []).flatMap((view): OverviewTableRow[] => {
    const route = view.route;
    if (!route) return [];

    return [
      {
        cells: figures(overviewSplitIds.route, dimensionNames.route, route.inferenceRouteId),
        colorKey: route.inferenceRouteId,
        entry: { page: "routes", resourceId: route.inferenceRouteId, screen: "route" },
        id: route.inferenceRouteId,
        name: route.name,
        subtitle: routeMembers(view, deploymentNames),
      },
    ];
  });

  return (
    <div className="grid min-w-0 gap-6 xl:grid-cols-2">
      <TableSection
        columns={columns}
        empty="No models deployed."
        heading="Models"
        openerRef={openerRef}
        rows={deploymentRows}
      />
      <TableSection
        columns={columns}
        empty="No routes."
        heading="Routes"
        openerRef={openerRef}
        rows={routeRows}
      />
    </div>
  );
}

/** A titled overview table, or what to say when it has no row. */
function TableSection({
  columns,
  empty,
  heading,
  openerRef,
  rows,
}: {
  columns: readonly OverviewTableColumn[];
  empty: string;
  heading: string;
  openerRef: RefObject<HTMLElement | null>;
  rows: readonly OverviewTableRow[];
}) {
  const headingId = `overview-${heading.toLowerCase()}-heading`;

  return (
    <section aria-labelledby={headingId} className="flex min-w-0 flex-col gap-3">
      <PageSectionHeading id={headingId}>{heading}</PageSectionHeading>
      {rows.length ? (
        <OverviewTable columns={columns} label={heading} openerRef={openerRef} rows={rows} />
      ) : (
        <p className="text-sm text-muted-foreground">{empty}</p>
      )}
    </section>
  );
}

/** The members of a route in priority order, groups joined by arrows. */
function routeMembers(view: InferenceRouteView, names: ReadonlyMap<string, string>): string {
  return (view.route?.fallbackGroups ?? [])
    .map((group) => group.modelDeploymentIds.map((id) => names.get(id) ?? id).join(", "))
    .join(" → ");
}

function formatFigure(unit: MetricCardUnit, value: number | null): string {
  if (value === null) return "—";

  return formatChartValue(unit === "count" ? "requests" : unit, value);
}
