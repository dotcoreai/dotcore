import { useMemo, useRef, useState } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import type { ChartSeries, ChartUnit } from "@/components/metrics/time-series-chart/chart-data";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { useWatchPaused } from "@/hooks/use-watch-pause";

import type { MetricStatsGrid, MetricStatsSeries } from "./metric-grid";
import { coveredTime, perStepValues } from "./metric-range-values";
import { cardRanges, type MetricRange } from "./metric-ranges";
import { useMetricRanges } from "./use-metric-ranges";
import { type MetricAnswer, type MetricHistoryQuery, useMetricAnswers } from "./use-metric-watch";

/** Unit of a card's value: a plain count, or one of the chart units it charts in. */
export type MetricCardUnit = "count" | Exclude<ChartUnit, "requests">;

/**
 * One card a feature declares: its label and unit, the queries it reads, and how it turns their
 * answers into its figures and its chart.
 */
export interface MetricCardSpec {
  /** The selected card's chart over the whole subject, from its answers in query order. */
  chart: (answers: readonly MetricAnswer[]) => ChartSeries[];
  id: string;
  label: string;
  /** How its per-step values draw: bars for counts, a line for the rest. */
  mark: "bars" | "line";
  /**
   * A split group's value over the range, such as its total or its mean, from the group's series
   * of the regrouped query. A card without one lists no group values.
   */
  groupValue?: (series: MetricStatsSeries, grid: MetricStatsGrid) => number | null;
  /** The queries it reads; a split regroups the first one unless `splitQueries` names another. */
  queries: readonly MetricHistoryQuery[];
  /** Its figures over the range, from its answers in query order. */
  read: (answers: readonly MetricAnswer[]) => MetricCardFigures;
  /**
   * The ids of the splits that apply to this card; any other split shows it as the whole subject.
   * Every split applies when absent.
   */
  splits?: readonly string[];
  /**
   * The query a split regroups in place of the first one, by split id, for a card whose figure
   * lives on another metric once broken down, such as attempts instead of requests by model.
   */
  splitQueries?: Readonly<Record<string, MetricHistoryQuery>>;
  /**
   * The chart of a split from the card's own answers, for a card whose queries already keep the
   * split's groups apart, such as readings kept per machine. Such a card adds no regrouped query.
   */
  splitChart?: (own: readonly MetricAnswer[], split: MetricCardSplit) => ChartSeries[];
  /**
   * A split group's per-step chart values, for a card whose chart does not plot the raw per-step
   * values of its query, such as a counter charted as a rate. Defaults to the raw values.
   */
  splitValues?: (series: MetricStatsSeries, stepMs: number) => (number | null)[];
  unit: MetricCardUnit;
}

/** What a card shows over the range: its value, an optional figure below, and per-step values. */
export interface MetricCardFigures {
  line?: MetricCardLine;
  /** Per-step values for a sparkline, gaps kept as `null`. */
  sparkline: (number | null)[];
  /** `null` when the range holds no data. */
  value: number | null;
}

/** A figure below a card's value, such as the share of prompt tokens served from cache. */
export interface MetricCardLine {
  label: string;
  unit: MetricCardUnit;
  /** `null` when there is nothing to relate it to. */
  value: number | null;
}

/** A way to break every card's chart down, such as by model; its groups chart as their own series. */
export interface MetricCardSplit {
  groupBy: readonly string[];
  id: string;
  label: string;
  /** A group's series label; the group's label values by default. */
  seriesLabel?: (labels: Readonly<Record<string, string>>) => string;
  /** The key a group's color derives from, so it matches the group's marker; its label values by default. */
  seriesColorKey?: (labels: Readonly<Record<string, string>>) => string;
}

/** One group of a split with the card's value over the range; `null` when it holds no data. */
export interface MetricCardGroupValue {
  labels: Readonly<Record<string, string>>;
  value: number | null;
}

/** One card of the panel; its reading is absent until the first answer. */
export interface MetricCard {
  id: string;
  label: string;
  /** How its per-step values draw. */
  mark: "bars" | "line";
  reading: MetricCardReading | undefined;
  unit: MetricCardUnit;
}

export interface MetricCardReading extends MetricCardFigures {
  /** When the store's history starts inside the range; absent when it covers the whole range. */
  historyStartMs?: number;
}

/** The selected card's chart: its series on the step grid of the answers. */
export interface MetricCardChart {
  historyStartMs: number;
  id: string;
  series: ChartSeries[];
  startMs: number;
  stepCount: number;
  stepMs: number;
  title: string;
  unit: ChartUnit;
}

/** What a card panel offers: the cards, the selected card's chart, the range and the split. */
export interface MetricCards {
  cards: MetricCard[];
  /** The selected card's chart, once read; absent when no card is selected. */
  chart: MetricCardChart | undefined;
  /** Whether the cards stopped updating; the last values read stay. */
  failed: boolean;
  /**
   * A card's value for each group of a split over the range, as the card's spec reads a group;
   * empty until the first answer, and for a card the split does not regroup.
   */
  groupValues: (splitId: string, cardId: string) => MetricCardGroupValue[];
  loadingVisible: boolean;
  range: MetricRange | undefined;
  ranges: readonly MetricRange[];
  selected: string | undefined;
  setRange: (label: string) => void;
  setSelected: (id: string | undefined) => void;
  setSplit: (id: string | undefined) => void;
  /** The split the chart is broken down by; absent for the whole subject. */
  split: string | undefined;
  /** Whether the cards cannot be read at all: no range is served, or the ranges failed to load. */
  unavailable: boolean;
}

export interface MetricCardsOptions {
  /** The range label to start at, such as the range of the page the panel opened from. */
  initialRange?: string;
  initialSplit?: string;
  /** Pauses the panel, such as for a sidebar hidden behind another view. */
  paused?: boolean;
  splits?: readonly MetricCardSplit[];
}

/**
 * The metric state of a card panel. One stream at the selected range carries every card's
 * queries and, for each split, every card's first query regrouped (unless the card charts the
 * split from its own answers), so selecting a card or a split reads the same answers and only a
 * range change reopens it. Cards start at the given range, or the finest one the store serves,
 * with no card selected. A card's reading is recomputed only when its answers change. A paused
 * panel holds no stream and keeps showing what it last read: while a log viewer is open anywhere
 * in the console, or when the caller pauses it.
 */
export function useMetricCards(
  client: MetricsClient,
  specs: readonly MetricCardSpec[],
  options: MetricCardsOptions = {},
): MetricCards {
  const pausedByLogs = useWatchPaused();
  const paused = (options.paused ?? false) || pausedByLogs;
  const splits = options.splits ?? noSplits;
  const served = useMetricRanges(client);
  const [rangeLabel, setRange] = useState(options.initialRange);
  const [selected, setSelected] = useState<string>();
  const [split, setSplit] = useState(options.initialSplit);
  const readings = useRef(new Map<string, { answers: unknown[]; reading: MetricCardReading }>());

  const ranges = served?.value ? cardRanges(served.value) : [];
  const range = ranges.find((entry) => entry.label === rangeLabel) ?? ranges[0];
  const plan = useMemo(() => planCardQueries(specs, splits), [specs, splits]);
  const watch = useMetricAnswers(client, plan.queries, range, { paused });
  const answers = watch?.value;
  const unavailable = served?.failed === true || served?.value?.length === 0;
  const loadingVisible = useDelayedLoading(!paused && !answers && !watch?.failed && !unavailable);

  const cards = specs.map((spec, index): MetricCard => {
    const card = { id: spec.id, label: spec.label, mark: spec.mark, unit: spec.unit };
    if (!answers) return { ...card, reading: undefined };

    return {
      ...card,
      reading: memoizedReading(readings.current, spec, ownAnswers(plan, answers, index), index),
    };
  });

  return {
    cards,
    chart:
      answers && selected
        ? selectedChart(plan, specs, splits, answers, selected, split)
        : undefined,
    failed: watch?.failed ?? false,
    groupValues: (splitId, cardId) =>
      answers ? splitGroupValues(plan, specs, answers, splitId, cardId) : [],
    loadingVisible,
    range,
    ranges,
    selected,
    setRange,
    setSelected,
    setSplit,
    split,
    unavailable,
  };
}

const noSplits: readonly MetricCardSplit[] = [];

/**
 * Where each card's answers sit in the stream: its own queries as a slice, then its regrouped
 * query by split id. Every reader of the answers goes through it.
 */
interface CardQueryPlan {
  cards: { grouped: ReadonlyMap<string, number>; own: { end: number; start: number } }[];
  queries: MetricHistoryQuery[];
}

/**
 * The stream's queries: every card's own queries in card order, then, split by split, the
 * regrouped query of each card the split applies to and does not chart from its own answers.
 */
function planCardQueries(
  specs: readonly MetricCardSpec[],
  splits: readonly MetricCardSplit[],
): CardQueryPlan {
  const queries: MetricHistoryQuery[] = [];
  const cards = specs.map((spec) => {
    const start = queries.length;
    queries.push(...spec.queries);
    return { grouped: new Map<string, number>(), own: { end: queries.length, start } };
  });

  for (const split of splits) {
    specs.forEach((spec, index) => {
      if (!appliesTo(spec, split.id) || spec.splitChart) return;

      cards[index].grouped.set(split.id, queries.length);
      queries.push(regroup(spec.splitQueries?.[split.id] ?? spec.queries[0], split.groupBy));
    });
  }

  return { cards, queries };
}

/** The card's reading, kept from the last render while its answers' grids are the same objects. */
function memoizedReading(
  cache: Map<string, { answers: unknown[]; reading: MetricCardReading }>,
  spec: MetricCardSpec,
  answers: readonly MetricAnswer[],
  index: number,
): MetricCardReading {
  const key = `${index}:${spec.id}`;
  // The spec is part of the identity: a spec rebuilt from new facts reads the same answers anew.
  const identities = [spec, ...answers.flatMap((answer) => [answer.grid, answer.stats])];
  const cached = cache.get(key);
  if (cached && sameItems(cached.answers, identities)) return cached.reading;

  const reading = { ...spec.read(answers), ...historyStart(answers[0]?.stats) };
  cache.set(key, { answers: identities, reading });

  return reading;
}

/**
 * The selected card's chart: the spec's own series for the whole subject, or one series per group
 * of the split, stacked when the card draws bars. A split that does not apply charts the whole
 * subject.
 */
function selectedChart(
  plan: CardQueryPlan,
  specs: readonly MetricCardSpec[],
  splits: readonly MetricCardSplit[],
  answers: readonly MetricAnswer[],
  selected: string,
  splitId: string | undefined,
): MetricCardChart | undefined {
  const specIndex = specs.findIndex((spec) => spec.id === selected);
  const spec = specs[specIndex];
  if (!spec) return undefined;

  const own = ownAnswers(plan, answers, specIndex);
  const split = splits.find((entry) => entry.id === splitId);
  const applies = split !== undefined && appliesTo(spec, split.id);
  const grouped = applies ? groupedAnswer(plan, answers, specIndex, split.id)?.stats : undefined;
  const layout = grouped ?? own[0]?.stats;
  if (!layout) return undefined;

  const series =
    applies && spec.splitChart
      ? spec.splitChart(own, split)
      : applies && grouped
        ? splitSeries(spec, split, grouped)
        : spec.chart(own);

  return {
    historyStartMs: layout.incarnation.startedAtMs,
    id: spec.id,
    series,
    startMs: layout.startMs,
    stepCount: layout.stepCount,
    stepMs: layout.stepMs,
    title: spec.label,
    unit: spec.unit === "count" ? "requests" : spec.unit,
  };
}

/** A card's value for each group of the split's regrouped query, as the card reads a group. */
function splitGroupValues(
  plan: CardQueryPlan,
  specs: readonly MetricCardSpec[],
  answers: readonly MetricAnswer[],
  splitId: string,
  cardId: string,
): MetricCardGroupValue[] {
  const specIndex = specs.findIndex((spec) => spec.id === cardId);
  const groupValue = specs[specIndex]?.groupValue;
  const grid = groupedAnswer(plan, answers, specIndex, splitId)?.stats;
  if (!groupValue || !grid) return [];

  return grid.series.map((series) => ({ labels: series.labels, value: groupValue(series, grid) }));
}

/** One series per group of a split answer, each drawn the way the card draws its values. */
function splitSeries(
  spec: MetricCardSpec,
  split: MetricCardSplit,
  grid: MetricStatsGrid,
): ChartSeries[] {
  return grid.series.map((series: MetricStatsSeries) => {
    // The raw label values name the group, so its color follows the group whatever its label.
    const key = split.groupBy.map((name) => series.labels[name] ?? "").join(" / ");
    return {
      color: { resource: split.seriesColorKey?.(series.labels) ?? key },
      id: key,
      label: split.seriesLabel?.(series.labels) ?? key,
      mark: spec.mark === "bars" ? "stackedBars" : "line",
      provisional: series.provisional,
      values: spec.splitValues?.(series, grid.stepMs) ?? perStepValues(series.steps),
    };
  });
}

function historyStart(grid: MetricStatsGrid | undefined): { historyStartMs?: number } {
  return grid && coveredTime(grid).startsInside
    ? { historyStartMs: grid.incarnation.startedAtMs }
    : {};
}

/** Whether a split breaks the card down; a card a split does not apply to reads as the whole subject. */
function appliesTo(spec: MetricCardSpec, splitId: string): boolean {
  return spec.splits === undefined || spec.splits.includes(splitId);
}

function ownAnswers(
  plan: CardQueryPlan,
  answers: readonly MetricAnswer[],
  specIndex: number,
): MetricAnswer[] {
  const own = plan.cards[specIndex]?.own;
  return own ? answers.slice(own.start, own.end) : [];
}

function groupedAnswer(
  plan: CardQueryPlan,
  answers: readonly MetricAnswer[],
  specIndex: number,
  splitId: string,
): MetricAnswer | undefined {
  const index = plan.cards[specIndex]?.grouped.get(splitId);
  return index === undefined ? undefined : answers[index];
}

function regroup(query: MetricHistoryQuery, groupBy: readonly string[]): MetricHistoryQuery {
  return { ...query, groupBy: [...query.groupBy, ...groupBy] };
}

function sameItems(a: readonly unknown[], b: readonly unknown[]): boolean {
  return a.length === b.length && a.every((item, index) => item === b[index]);
}
