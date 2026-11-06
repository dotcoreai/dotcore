import type { ChartSeries } from "@/components/metrics/time-series-chart/chart-data";
import type { MetricStatsGrid } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import { gaugeRange, perStepValues } from "@/features/metrics/metric-range-values";
import type { MetricCardSpec, MetricCardSplit } from "@/features/metrics/use-metric-cards";
import type { MetricHistoryQuery } from "@/features/metrics/use-metric-watch";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { MetricAggregate, MetricReduce } from "@/gen/dotcore/metrics/v1/metric_pb";
import { applicationDeploymentPresentationName } from "@/lib/application-deployment-name";

import type { MachineFacts } from "./machine-site-values";

/** The split id and card ids of the apps section. */
export const appSplitId = "app";

/** The app card ids in display order. */
export const appCardIds = ["app-cpu", "app-ram", "app-vram"] as const;

/**
 * The three app cards the overview reads over the range, each summing the apps' use (the services
 * of an app add up) and breaking down by app. CPU is the apps' share of the CPUs of the machines
 * hosting them: each app's ratio of its machine is weighted by that machine's logical CPUs, then
 * divided by the CPUs of all hosting machines. A card's value is the range average.
 */
export function appCardSpecs(facts: MachineFacts): MetricCardSpec[] {
  return appCards.map((card) => ({
    chart: (answers) => [
      line(card.id, card.label, "chart-1", totalSteps(card, answers[0]?.stats, facts)),
    ],
    id: card.id,
    label: card.label,
    mark: "line",
    queries: [card.query],
    read: (answers) => {
      const { values } = totalSteps(card, answers[0]?.stats, facts);
      return { sparkline: values, value: gaugeRange(values).average };
    },
    splitChart: (answers, split) =>
      [...appValues(card, answers[0]?.stats, facts)].map(([deployment, values]) =>
        line(
          deployment,
          split.seriesLabel?.({ [dimensionNames.deployment]: deployment }) ?? deployment,
          { resource: deployment },
          values,
        ),
      ),
    splits: [appSplitId],
    unit: card.unit,
  }));
}

/** The by-app split of the app cards, naming each group like the console's apps. */
export function appSplit(deployments: readonly ApplicationDeployment[] = []): MetricCardSplit {
  const names = new Map(
    deployments.map((deployment) => [
      deployment.applicationDeploymentId,
      applicationDeploymentPresentationName(deployment),
    ]),
  );

  return {
    groupBy: [dimensionNames.deployment],
    id: appSplitId,
    label: "By app",
    seriesLabel: (labels) =>
      names.get(labels[dimensionNames.deployment] ?? "") ?? labels[dimensionNames.deployment] ?? "",
  };
}

interface AppCard {
  id: (typeof appCardIds)[number];
  label: string;
  /** Whether each reading is a ratio of its machine's CPUs, so it is weighted by that machine. */
  perMachine: boolean;
  query: MetricHistoryQuery;
  unit: "bytes" | "ratio";
}

const appCards: readonly AppCard[] = [
  {
    id: "app-cpu",
    label: "CPU",
    perMachine: true,
    query: gaugeQuery(metricNames.applicationHostCpuUtilization, [
      dimensionNames.deployment,
      dimensionNames.node,
    ]),
    unit: "ratio",
  },
  {
    id: "app-ram",
    label: "RAM",
    perMachine: false,
    query: gaugeQuery(metricNames.applicationHostMemoryUsed, [dimensionNames.deployment]),
    unit: "bytes",
  },
  {
    id: "app-vram",
    label: "VRAM",
    perMachine: false,
    query: gaugeQuery(metricNames.applicationAcceleratorMemoryUsed, [dimensionNames.deployment]),
    unit: "bytes",
  },
];

interface Steps {
  provisional: boolean[];
  values: (number | null)[];
}

/** Each app's value per step; CPU weighted to the apps' share of the hosting machines' CPUs. */
function appValues(
  card: AppCard,
  stats: MetricStatsGrid | undefined,
  facts: MachineFacts,
): Map<string, Steps> {
  const apps = new Map<string, Steps>();
  if (!stats) return apps;

  // A machine without a known CPU count cannot weight its readings, so it does not count.
  const weighted = (node: string | undefined) =>
    node === undefined ? 0 : (facts.cpus.get(node) ?? 0);
  const members = stats.series.filter(
    (series) => !card.perMachine || weighted(series.labels[dimensionNames.node]) > 0,
  );
  const capacity = card.perMachine
    ? [...new Set(members.map((series) => series.labels[dimensionNames.node]))].reduce(
        (total, node) => total + weighted(node),
        0,
      )
    : 1;

  for (const series of members) {
    const deployment = series.labels[dimensionNames.deployment];
    if (deployment === undefined) continue;

    const weight = card.perMachine ? weighted(series.labels[dimensionNames.node]) : 1;
    const values = perStepValues(series.steps).map((value) =>
      value === null ? null : (value * weight) / capacity,
    );
    const steps = { provisional: series.provisional, values };
    const held = apps.get(deployment);
    apps.set(deployment, held ? sum([held, steps]) : steps);
  }

  return apps;
}

/** The apps added up per step; absent where no app reports. */
function totalSteps(card: AppCard, stats: MetricStatsGrid | undefined, facts: MachineFacts): Steps {
  return sum([...appValues(card, stats, facts).values()], stats?.stepCount);
}

/** Per step, the sum of the members reporting in it; a step none reports stays absent. */
function sum(members: readonly Steps[], stepCount?: number): Steps {
  const length = stepCount ?? Math.max(0, ...members.map((member) => member.values.length));
  const values: (number | null)[] = [];
  const provisional: boolean[] = [];

  for (let step = 0; step < length; step += 1) {
    let total: number | null = null;
    let open = false;
    for (const member of members) {
      const value = member.values[step];
      if (value === null || value === undefined) continue;

      total = (total ?? 0) + value;
      open ||= member.provisional[step] ?? false;
    }
    values.push(total);
    provisional.push(open);
  }

  return { provisional, values };
}

function gaugeQuery(metric: string, groupBy: readonly string[]): MetricHistoryQuery {
  return {
    aggregate: MetricAggregate.SUM,
    groupBy,
    matchers: {},
    metric,
    reduce: MetricReduce.AVG,
  };
}

function line(id: string, label: string, color: ChartSeries["color"], steps: Steps): ChartSeries {
  return { color, id, label, mark: "line", provisional: steps.provisional, values: steps.values };
}
