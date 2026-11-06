import { InfoIcon } from "lucide-react";
import type { MouseEvent } from "react";

import { formatChartValue } from "@/components/metrics/time-series-chart/chart-data";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

import type { DeploymentGpu } from "./model-deployment-gpu";

/**
 * A deployment's GPU use in a table cell. The figure is the whole device's use, so when other
 * deployments hold memory on it and a figure exists, an info icon names them. `gpu.sharedWith`
 * holds display names here.
 */
export function DeploymentGpuCell({ gpu }: { gpu: DeploymentGpu | undefined }) {
  if (gpu?.utilization === undefined) {
    return <span className="text-muted-foreground">—</span>;
  }

  const note = gpu.sharedWith.length
    ? `GPU use of the whole device, shared with ${gpu.sharedWith.join(", ")}`
    : undefined;

  return (
    <span className="inline-flex items-center gap-1">
      <span className="tabular-nums">{formatChartValue("ratio", gpu.utilization)}</span>
      {note ? (
        <Tooltip>
          <TooltipTrigger
            aria-label={note}
            render={
              <Button
                // The row opens its panel on click; the icon only explains the figure.
                onClick={(event: MouseEvent) => event.stopPropagation()}
                size="icon-xs"
                type="button"
                variant="ghost"
              />
            }
          >
            <InfoIcon aria-hidden="true" className="text-muted-foreground" />
          </TooltipTrigger>
          <TooltipContent>{note}</TooltipContent>
        </Tooltip>
      ) : null}
    </span>
  );
}
