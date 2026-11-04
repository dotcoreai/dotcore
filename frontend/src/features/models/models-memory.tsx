import { useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { MetricRows } from "@/components/metrics/metric-row/metric-row";
import { PageSectionHeading } from "@/components/ui/page-header";
import { memoryQueryGroups } from "@/features/memory/memory-history";
import { MemoryRowHistory } from "@/features/memory/memory-row-history";
import { MemoryMetricRows } from "@/features/memory/memory-rows";
import { MemoryViewSwitch, MemoryViewSwitchPlace } from "@/features/memory/memory-view-switch";
import { pastTotalCapacity } from "@/features/memory/pool-spans";
import type { MemoryHistory } from "@/features/memory/use-memory-history";
import type { SiteMemoryState } from "@/features/memory/use-site-memory";
import { useMetricPanel } from "@/features/metrics/use-metric-panel";
import { MemoryView } from "@/gen/dotcore/console/v1/preferences_pb";

/**
 * The Models page memory section under the page header: a heading with the memory view choice,
 * then every machine merged into one bar per memory type, or each machine's span kept and named.
 * A row expands to its history, with a line per model deployment measured in it; history streams
 * only while a row is expanded. The view choice shows only when the site lists more than one
 * machine. VRAM and RAM rows are always drawn, empty while memory is first read or when no machine
 * has that memory, so the section keeps its shape; a short status replaces the section when no
 * machine has readable memory.
 */
export function ModelsMemory({
  history,
  machineCount,
  metricsClient,
  onViewChange,
  state,
  view,
}: {
  history: MemoryHistory;
  machineCount: number;
  metricsClient: MetricsClient;
  onViewChange: (view: MemoryView) => void;
  state: SiteMemoryState;
  view: MemoryView;
}) {
  const groups = useMemo(() => memoryQueryGroups(history.described), [history.described]);
  const panel = useMetricPanel(metricsClient, groups, noCurrentWindow);

  if (state.kind === "unavailable") {
    return (
      <p className="text-xs text-muted-foreground">
        Memory usage is unavailable: no machine reported readable memory.
      </p>
    );
  }

  const byMachine = view === MemoryView.BY_MACHINE && machineCount > 1;
  // While memory is first read the count may not be known, so the chosen view decides whether the
  // empty rows keep the line the By machine view names machines on.
  const spans =
    state.kind === "loading" ? view === MemoryView.BY_MACHINE && machineCount !== 1 : byMachine;
  const rows = state.kind === "loading" ? [] : state.memory.rows;

  return (
    <section aria-labelledby="models-memory-heading" className="flex flex-col gap-3">
      <div className="flex min-h-6 items-center justify-between gap-4">
        <PageSectionHeading id="models-memory-heading">Memory</PageSectionHeading>
        {machineCount > 1 ? (
          <MemoryViewSwitch label="Memory view" onChange={onViewChange} view={view} />
        ) : (
          <MemoryViewSwitchPlace />
        )}
      </div>
      <MetricRows fitLabels>
        <MemoryMetricRows
          capacity={pastTotalCapacity(rows)}
          emptyChildren={(kind) => (
            <MemoryRowHistory
              breakdown={byMachine ? "machines" : "combined"}
              history={history}
              kind={kind}
              panel={panel}
            />
          )}
          expanded={(kind) => panel.expanded(kind)}
          kinds={memoryKinds}
          mode={spans ? "spans" : "merged"}
          onExpandedChange={(kind, expanded) => panel.setExpanded(kind, expanded)}
          rows={rows}
          surface="page"
          unavailable={state.kind === "loading" ? [] : state.memory.unavailable}
        >
          {(row) => (
            <MemoryRowHistory
              breakdown={byMachine ? "machines" : "combined"}
              history={history}
              kind={row.kind}
              panel={panel}
            />
          )}
        </MemoryMetricRows>
      </MetricRows>
    </section>
  );
}

// The memory groups read history only: the bars come from the node memory watch.
const noCurrentWindow = { durationMs: 0, stepMs: 0 } as const;

// VRAM leads RAM, so the rows read from the accelerators down to host memory.
const memoryKinds = ["vram", "ram"] as const;
