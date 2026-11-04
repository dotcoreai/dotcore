import { useRef } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import { OverviewSectionIcon } from "@/app/console-section-icons";
import { PageActions, PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import { SegmentedControl } from "@/components/ui/segmented-control";

import { OverviewInferenceSection } from "./overview-inference-section";
import { OverviewMachinesSection } from "./overview-machines-section";
import { OverviewSidebar } from "./overview-sidebar";
import { OverviewTablesSection } from "./overview-tables-section";
import { useOverviewMetrics } from "./use-overview-metrics";

export interface OverviewPageProps {
  metricsClient?: MetricsClient;
  modelClient?: ModelClient;
  routeClient?: RouteClient;
}

/** The site overview page: an at-a-glance view of the site's inference, models, routes and machines. */
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
              <SegmentedControl
                aria-label="Overview range"
                onValueChange={metrics.setRange}
                options={metrics.ranges.map((range) => ({
                  label: range.label,
                  value: range.label,
                }))}
                value={metrics.range?.label}
              />
            </PageActions>
          ) : null}
        </PageHeader>

        <OverviewInferenceSection metrics={metrics} />

        <OverviewTablesSection metrics={metrics} openerRef={openerRef} />

        <OverviewMachinesSection
          metrics={metrics}
          metricsClient={metricsClient}
          openerRef={openerRef}
        />

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
