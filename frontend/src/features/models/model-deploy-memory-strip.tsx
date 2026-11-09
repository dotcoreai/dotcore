import { type ReactNode, useMemo } from "react";

import { Notice } from "@/components/ui/notice";
import { MemoryRows } from "@/features/memory/memory-rows";
import { formatBytes } from "@/lib/format";
import { cn } from "@/lib/utils";
import { ResourceColorContext, useResourceColors } from "@/theme/resource-color-context";

import { type MemoryOverCapacity, type PredictedMemory } from "./model-deploy-memory";

/**
 * The model deploy dialog's memory strip: the selected target's memory rows with this deployment
 * drawn on top of what occupies them, or why they can't show. It informs and never blocks.
 */
export function ModelDeployMemoryStrip({ memory }: { memory: PredictedMemory }) {
  const header = memory.kind === "noTarget" ? "No execution target" : memory.targetName;

  return (
    <section aria-label="Predicted memory" className="grid gap-2">
      {header ? (
        <p
          className={cn(
            "truncate text-xs font-semibold",
            memory.kind === "noTarget" ? "text-muted-foreground" : "text-foreground",
          )}
        >
          {header}
        </p>
      ) : null}
      {memory.kind === "ready" ? (
        <PredictedColor deploymentId={memory.deploymentId}>
          <MemoryRows
            capacity={memory.capacity}
            focusDeploymentId={memory.deploymentId}
            focusStatus={memory.focusStatus}
            rows={memory.rows}
            surface="panel"
            unavailable={memory.unavailable}
          />
        </PredictedColor>
      ) : memory.kind === "loading" ? (
        // Every target has RAM; its other rows join once its memory is read.
        <MemoryRows pending={["ram"]} rows={[]} surface="panel" />
      ) : memory.kind === "unavailable" ? (
        <p className="text-xs text-muted-foreground">{memory.message}</p>
      ) : null}
    </section>
  );
}

/**
 * A Deploy-pane warning naming the memory this deployment may take beyond capacity, and the pools
 * where it leaves less than the fit line's headroom free, if any.
 */
export function MemoryCapacityWarning({ memory }: { memory: PredictedMemory }) {
  if (memory.kind !== "ready" || !memory.overCapacity.length) return null;

  const beyond = memory.overCapacity.filter((entry) => entry.beyondTotal);
  const tight = memory.overCapacity.filter((entry) => !entry.beyondTotal);
  const sentences = [
    beyond.length
      ? `This deployment may exceed memory capacity on ${memory.targetName}: ${rowAmounts(beyond)}.`
      : undefined,
    tight.length
      ? beyond.length
        ? `It leaves less than ${headroom(tight)} of memory free there: ${rowAmounts(tight)}.`
        : `This deployment leaves less than ${headroom(tight)} of memory free on ${memory.targetName}: ${rowAmounts(tight)}.`
      : undefined,
  ];

  return (
    <Notice role="note" tone="warning">
      {sentences.filter(Boolean).join(" ")}
    </Notice>
  );
}

/** The headroom the pools' fit lines keep free, the same share on every pool. */
function headroom(entries: MemoryOverCapacity[]): string {
  return `${entries[0].headroomPercent}%`;
}

function rowAmounts(entries: MemoryOverCapacity[]): string {
  return entries.map((entry) => `${entry.label} by ${formatBytes(entry.bytes)}`).join(", ");
}

/** Draws the deployment being created in the color it would most likely get once it exists. */
function PredictedColor({ children, deploymentId }: { children: ReactNode; deploymentId: string }) {
  const colors = useResourceColors();
  const predicted = useMemo(
    () => ({
      ...colors,
      colorOf: (key: string) => (key === deploymentId ? colors.nextColor : colors.colorOf(key)),
    }),
    [colors, deploymentId],
  );

  return (
    <ResourceColorContext.Provider value={predicted}>{children}</ResourceColorContext.Provider>
  );
}
