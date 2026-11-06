import { useRef } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import { OverviewSectionIcon } from "@/app/console-section-icons";
import { PageActions, PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import { InferenceSection } from "@/features/inference/inference-section";
import { MetricRangeControl } from "@/features/metrics/metric-range-control";

import { InferenceCountCards, OverviewAppsSection } from "./overview-count-cards";
import { OverviewMachinesSection } from "./overview-machines-section";
import { OverviewSidebar } from "./overview-sidebar";
import { useOverviewMetrics } from "./use-overview-metrics";

export interface OverviewPageProps {
  metricsClient?: MetricsClient;
  modelClient?: ModelClient;
  routeClient?: RouteClient;
}

/** The site overview page: an at-a-glance view of the site's inference, models, routes, apps and machines. */
export function OverviewPage({
  metricsClient = defaultMetricsClient,
  modelClient,
  routeClient,
}: OverviewPageProps) {
  const openerRef = useRef<HTMLElement | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const metrics = useOverviewMetrics(metricsClient);

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={OverviewSectionIcon} ref={headingRef} tabIndex={-1}>
            Overview
          </PageHeaderHeading>
          {metrics.ranges.length > 1 ? (
            <PageActions>
              <MetricRangeControl label="Overview range" metrics={metrics} />
            </PageActions>
          ) : null}
        </PageHeader>

        <InferenceSection leading={<InferenceCountCards />} metrics={metrics} />

        <OverviewAppsSection metrics={metrics} />

        <OverviewMachinesSection metrics={metrics} metricsClient={metricsClient} />

        <OverviewSidebar
          fallbackFocusRef={headingRef}
          initialRange={metrics.range?.label}
          metricsClient={metricsClient}
          modelClient={modelClient}
          openerRef={openerRef}
          routeClient={routeClient}
        />
      </div>
    </div>
  );
}
