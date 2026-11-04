import { useEffect, useMemo, useRef } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type MetricsClient } from "@/api/metrics-client";
import { type RouteClient } from "@/api/route-client";
import { RoutesSectionIcon } from "@/app/console-section-icons";
import { AddAction } from "@/components/ui/add-action";
import { EmptyState } from "@/components/ui/empty-state";
import { Notice } from "@/components/ui/notice";
import {
  PageActions,
  PageHeader,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { canonicalizePanelLocation } from "@/lib/panel-stack";

import { RouteCard } from "./route-card";
import { useRouteSidebarHost } from "./use-route-sidebar-host";

export interface RoutesPageProps {
  client?: RouteClient;
  metricsClient?: MetricsClient;
}

export function RoutesPage({ client, metricsClient }: RoutesPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const openerRef = useRef<HTMLElement>(null);
  const host = useRouteSidebarHost({
    client,
    metricsClient,
    openerRef,
    returnFocusRef: headingRef,
  });
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );

  useEffect(() => {
    if (!canonical.recovered) return;
    void navigate(canonical, { replace: true });
  }, [canonical, navigate]);

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={RoutesSectionIcon} ref={headingRef} tabIndex={-1}>
            Routes
          </PageHeaderHeading>
          <PageActions>
            <AddAction label="Create route" onClick={host.openCreate} />
          </PageActions>
        </PageHeader>
        {host.loadError ? (
          <Notice role="alert" tone="destructive">
            {host.loadError}
          </Notice>
        ) : null}
        {host.error ? (
          <Notice role="alert" tone="destructive">
            {host.error}
          </Notice>
        ) : null}
        <section aria-labelledby="inference-routes-heading" className="flex flex-col gap-4">
          <div>
            <PageSectionHeading id="inference-routes-heading">Routes</PageSectionHeading>
          </div>
          {host.loadingVisible && !host.loadError ? (
            <p className="text-sm text-muted-foreground" role="status">
              Loading routes…
            </p>
          ) : !host.routes ? null : host.routes.length ? (
            <ul aria-label="Inference routes" className="inspector-card-grid">
              {host.routes.map((view) => (
                <RouteCard
                  deploymentMentions={host.deploymentMentions}
                  key={view.route?.inferenceRouteId ?? view.route?.name}
                  onSelect={(opener) => host.select(view, opener)}
                  panelId={host.panelId}
                  selected={view.route?.inferenceRouteId === host.selectedRouteId}
                  view={view}
                />
              ))}
            </ul>
          ) : (
            <EmptyState
              action={<AddAction label="Create route" onClick={host.openCreate} />}
              description="Create a route to give one or more model deployments a single name that clients can call."
              icon={RoutesSectionIcon}
              title="No routes yet"
            />
          )}
        </section>
        {host.sidebar}
      </div>
    </div>
  );
}
