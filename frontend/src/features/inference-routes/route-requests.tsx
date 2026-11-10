import { useMemo } from "react";

import { type MetricsClient } from "@/api/metrics-client";
import { routeRequestCards } from "@/features/metrics/request-card-specs";
import { RequestCards } from "@/features/metrics/request-cards";

/**
 * The route's `Requests` section: request cards over a selectable range, each charting on a
 * click. Mount it per route, so the range and the selected card start afresh for each one.
 */
export function RouteRequests({
  client,
  initialRange,
  routeId,
}: {
  client: MetricsClient;
  /** The range label the cards start at, such as the range of the page the sidebar opened from. */
  initialRange?: string;
  routeId: string;
}) {
  const specs = useMemo(() => routeRequestCards(routeId), [routeId]);

  return (
    <RequestCards
      client={client}
      headingId="route-requests-heading"
      initialRange={initialRange}
      specs={specs}
    />
  );
}
