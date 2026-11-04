import { useState } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

import type { MetricGrid } from "./metric-grid";
import { useMetricsPausedByLogs } from "./metric-pause";
import type { MetricRange } from "./metric-ranges";
import { useMetricRanges } from "./use-metric-ranges";
import { type MetricHistoryQuery, useMetricWatch } from "./use-metric-watch";

/**
 * A group of queries a panel watches: those current values read and those charts read. Several
 * rows may share one group, such as every GPU's utilization row reading one query grouped by
 * device.
 */
export interface MetricQueryGroup {
  /** Queries over the panel's current window; empty when current values come from elsewhere. */
  current: readonly MetricHistoryQuery[];
  history: readonly MetricHistoryQuery[];
  id: string;
}

/** The window the current values of a panel read: long enough for a peak, at a step that suits them. */
export interface MetricCurrentWindow {
  durationMs: number;
  stepMs: number;
}

/** What a panel offers its rows: current grids, expansion, the shared range and history grids. */
export interface MetricPanel {
  /** The group's current grids, in the order of its current queries, once the first answer arrived. */
  current: (groupId: string) => MetricGrid[] | undefined;
  /** Whether the current values stopped updating; the last ones read stay. */
  currentFailed: boolean;
  expanded: (rowId: string) => boolean;
  /** The group's history grids, in the order of its history queries, at the selected range. */
  history: (groupId: string) => MetricGrid[] | undefined;
  /** Whether history stopped updating; the last grids read stay. */
  historyFailed: boolean;
  /** Whether history cannot be read at all: no range is served, or the ranges failed to load. */
  historyUnavailable: boolean;
  historyLoadingVisible: boolean;
  range: MetricRange | undefined;
  ranges: readonly MetricRange[];
  setExpanded: (rowId: string, expanded: boolean) => void;
  setRange: (label: string) => void;
}

export interface MetricPanelOptions {
  /** The range label to start at, such as the range of the page the sidebar opened from. */
  initialRange?: string;
  /** Pauses the panel, such as for a sidebar hidden behind another view. */
  paused?: boolean;
  /** The range of a caller's own range picker, overriding the panel's. */
  rangeLabel?: string;
}

/**
 * The metric state of one sidebar. Current values stream while the panel is mounted, over a fixed
 * window. History streams only while at least one row is expanded, and then carries every group's
 * history queries, so expanding or collapsing another row leaves the stream as it is. Rows start
 * collapsed at the given initial range, or the finest one the store serves; one range applies to
 * every expanded chart. Rows are named by any id, independent of the query groups they read. A
 * caller that shows its own range picker passes the range label, which then overrides the panel's.
 * A paused panel holds no stream and keeps showing what it last read: while a log viewer is open
 * anywhere in the console, or when the caller pauses it for a sidebar hidden behind another view.
 */
export function useMetricPanel(
  client: MetricsClient,
  groups: readonly MetricQueryGroup[],
  currentWindow: MetricCurrentWindow,
  options: MetricPanelOptions = {},
): MetricPanel {
  const pausedByLogs = useMetricsPausedByLogs();
  const paused = (options.paused ?? false) || pausedByLogs;
  const ranges = useMetricRanges(client);
  const [rangeLabel, setRangeLabel] = useState(options.initialRange);
  const [expandedIds, setExpandedIds] = useState<ReadonlySet<string>>(() => new Set());
  const range =
    ranges?.value?.find((entry) => entry.label === (options.rangeLabel ?? rangeLabel)) ??
    ranges?.value?.[0];
  const anyExpanded = expandedIds.size > 0;

  const currentQueries = groups.flatMap((group) => group.current);
  const historyQueries = anyExpanded ? groups.flatMap((group) => group.history) : [];
  const current = useMetricWatch(
    client,
    currentQueries,
    { ...currentWindow, label: "current" },
    { paused },
  );
  const history = useMetricWatch(client, historyQueries, range, { paused });
  const historyUnavailable = ranges?.failed === true || ranges?.value?.length === 0;
  const historyLoadingVisible = useDelayedLoading(
    anyExpanded && !paused && !history?.value && !history?.failed && !historyUnavailable,
  );

  return {
    current: (groupId) => groupSlice(groups, groupId, (group) => group.current, current?.value),
    currentFailed: current?.failed ?? false,
    expanded: (rowId) => expandedIds.has(rowId),
    history: (groupId) => groupSlice(groups, groupId, (group) => group.history, history?.value),
    historyFailed: history?.failed ?? false,
    historyLoadingVisible,
    historyUnavailable,
    range,
    ranges: ranges?.value ?? [],
    setExpanded: (rowId, expanded) =>
      setExpandedIds((ids) => {
        const next = new Set(ids);
        if (expanded) next.add(rowId);
        else next.delete(rowId);
        return next;
      }),
    setRange: setRangeLabel,
  };
}

/** The grids of one group out of the panel's flattened query answers. */
function groupSlice(
  groups: readonly MetricQueryGroup[],
  groupId: string,
  queries: (group: MetricQueryGroup) => readonly MetricHistoryQuery[],
  grids: readonly MetricGrid[] | undefined,
): MetricGrid[] | undefined {
  if (!grids) return undefined;

  let offset = 0;
  for (const group of groups) {
    const count = queries(group).length;
    if (group.id === groupId) return count ? grids.slice(offset, offset + count) : undefined;
    offset += count;
  }

  return undefined;
}
