import type { ReactNode } from "react";
import { Link } from "react-router-dom";

import {
  useWatchedApplicationDeployments,
  useWatchedModelDeployments,
  useWatchedRoutes,
} from "@/app/resource-watch";
import { MetricCard } from "@/components/metrics/metric-card/metric-card";
import { PageSectionHeading } from "@/components/ui/page-header";
import { projectApplicationDeploymentStatus } from "@/features/applications/application-projection";
import { deploymentStatus } from "@/features/models/model-projection";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

/** A count card that opens the list page behind its number. */
export function OverviewCountCard({
  label,
  line,
  pending = false,
  to,
  unit,
  value,
}: {
  label: string;
  /** What the count is out of, with any attention text after it. */
  line?: string;
  pending?: boolean;
  /** The list page the card opens. */
  to: string;
  unit: string;
  value: number;
}) {
  return (
    <Link
      aria-label={label}
      className="flex min-w-0 rounded-lg outline-none focus-visible:ring-2 focus-visible:ring-ring/50 [&>*]:flex-1 hover:[&>*]:border-chart-1/50"
      to={to}
    >
      <MetricCard
        id={label}
        label={label}
        line={line}
        pending={pending}
        selectable={false}
        unit={unit}
        value={String(value)}
      />
    </Link>
  );
}

/**
 * The count cards that lead the overview's inference section: models ready out of those deployed,
 * with the failed ones called out, and routes available out of those configured, with the
 * unavailable ones called out.
 */
export function InferenceCountCards(): ReactNode {
  const deployments = useWatchedModelDeployments()?.value?.deployments;
  const routes = useWatchedRoutes()?.value?.routes;
  const ready = deployments?.filter(
    (deployment) => deploymentStatus(deployment) === ModelDeploymentStatus.READY,
  ).length;
  const failed = deployments?.filter(
    (deployment) => deploymentStatus(deployment) === ModelDeploymentStatus.FAILED,
  ).length;
  const enabled = routes?.filter((view) => view.route?.enabled);
  const available = enabled?.filter((view) => view.availability?.available).length;
  const unavailable = enabled?.filter(
    (view) => view.availability && !view.availability.available,
  ).length;

  return (
    <>
      <OverviewCountCard
        label="Models"
        line={countLine(deployments?.length, "deployed", failed, "failed")}
        pending={ready === undefined}
        to="/models"
        unit="ready"
        value={ready ?? 0}
      />
      <OverviewCountCard
        label="Routes"
        line={countLine(routes?.length, "configured", unavailable, "unavailable")}
        pending={available === undefined}
        to="/routes"
        unit="available"
        value={available ?? 0}
      />
    </>
  );
}

/** The overview's apps section: apps ready out of those deployed, with the failed ones called out. */
export function OverviewAppsSection() {
  const deployments = useWatchedApplicationDeployments()?.value?.deployments;
  const statuses = deployments?.map(projectApplicationDeploymentStatus);
  const ready = statuses?.filter((status) => status.tone === "success").length;
  const failed = statuses?.filter((status) => status.label === "Failed").length;

  return (
    <section aria-labelledby="overview-apps-heading" className="flex flex-col gap-3">
      <PageSectionHeading id="overview-apps-heading">Apps</PageSectionHeading>
      <div className="grid grid-cols-2 gap-2 lg:grid-cols-3 xl:grid-cols-4">
        <OverviewCountCard
          label="Apps"
          line={countLine(deployments?.length, "deployed", failed, "failed")}
          pending={ready === undefined}
          to="/apps"
          unit="ready"
          value={ready ?? 0}
        />
      </div>
    </section>
  );
}

function countLine(
  total: number | undefined,
  totalLabel: string,
  attention: number | undefined,
  attentionLabel: string,
): string | undefined {
  if (total === undefined) return undefined;

  return attention
    ? `of ${total} ${totalLabel} · ${attention} ${attentionLabel}`
    : `of ${total} ${totalLabel}`;
}
