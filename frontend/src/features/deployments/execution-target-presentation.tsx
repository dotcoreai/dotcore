import { Detail, DetailList } from "@/components/ui/detail-list";
import { executionTargetPresentation } from "@/domain/execution-target";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { cn } from "@/lib/utils";

export function ExecutionTargetSummary({
  className,
  target,
}: {
  className?: string;
  target: ConfiguredExecutionTarget;
}) {
  const presentation = executionTargetPresentation(target);

  return (
    <div className={cn("rounded-lg border border-border bg-muted/30 p-3", className)}>
      <DetailList>
        <Detail label="Machine" value={target.siteNodeDisplayName || target.siteNodeId} />
        <Detail label="Target" value={target.displayName} />
        <Detail label="Target ID" value={target.executionTargetId} />
        <Detail label="Platform" value={target.platformLabel || "Unknown"} />
        <Detail label="Configured devices" value={presentation.devicesLabel} />
        <Detail label="Workload kind" value={presentation.workloadLabel} />
        <Detail label="Connection freshness" value={presentation.connectionLabel} />
        <Detail label="Runtime availability" value={presentation.runtimeLabel} />
      </DetailList>
      {presentation.runtimeDetail ? (
        <p className="mt-2 break-words text-xs text-muted-foreground">
          {presentation.runtimeDetail}
        </p>
      ) : null}
      {presentation.deploymentGate ? (
        <p className="mt-2 text-xs text-muted-foreground">{presentation.deploymentGate}</p>
      ) : null}
    </div>
  );
}
