import { type RefObject, useEffect, useMemo } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import type { MetricsClient } from "@/api/metrics-client";
import type { ModelClient } from "@/api/model-client";
import type { RouteClient } from "@/api/route-client";
import { MachineSidebarHost } from "@/features/machines/machine-sidebar-host";
import { DeploymentSidebar } from "@/features/models/deployment-sidebar";
import { RouteSidebarHost } from "@/features/routes/route-sidebar-host";
import { activePanel, canonicalizePanelLocation } from "@/panels/panel-stack";

export interface OverviewSidebarProps {
  /** The element focus moves to when the open resource disappears, such as the page heading. */
  fallbackFocusRef: RefObject<HTMLElement | null>;
  /** The range label the sidebar's metrics start at: the overview's range when it opened. */
  initialRange?: string;
  metricsClient?: MetricsClient;
  modelClient?: ModelClient;
  /** The element focus returns to when the sidebar closes, set by whatever opened it. */
  openerRef: RefObject<HTMLElement | null>;
  routeClient?: RouteClient;
}

/**
 * The sidebar the overview shows for its active panel: the host of the active entry's resource
 * kind, the same sidebar that kind's own page opens. A stack the overview cannot render is moved to
 * the page that renders it.
 */
export function OverviewSidebar({
  fallbackFocusRef,
  initialRange,
  metricsClient,
  modelClient,
  openerRef,
  routeClient,
}: OverviewSidebarProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const entry = activePanel(canonical.search);

  useEffect(() => {
    if (canonical.recovered) void navigate(canonical, { replace: true });
  }, [canonical, navigate]);

  // One host per resource kind the overview hosts; a kind without a host here shows no sidebar.
  switch (entry?.page) {
    case "models":
      return (
        <DeploymentSidebar
          client={modelClient}
          entry={entry}
          fallbackFocusRef={fallbackFocusRef}
          initialRange={initialRange}
          metricsClient={metricsClient}
          openerRef={openerRef}
        />
      );
    case "routes":
      return (
        <RouteSidebarHost
          client={routeClient}
          initialRange={initialRange}
          metricsClient={metricsClient}
          openerRef={openerRef}
          returnFocusRef={fallbackFocusRef}
        />
      );
    case "machines":
      return (
        <MachineSidebarHost
          initialRange={initialRange}
          metricsClient={metricsClient}
          openerRef={openerRef}
        />
      );
    default:
      return null;
  }
}
