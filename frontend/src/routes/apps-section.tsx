import { Outlet } from "react-router-dom";

import type { ApplicationClient } from "@/api/application-client";
import { ApplicationsPollingProvider } from "@/features/applications/applications-polling-provider";

export interface AppsSectionRouteProps {
  client?: ApplicationClient;
}

/** Layout route for the Apps section pages, which share one applications polling instance. */
export function AppsSectionRoute({ client }: AppsSectionRouteProps) {
  return (
    <ApplicationsPollingProvider client={client}>
      <Outlet />
    </ApplicationsPollingProvider>
  );
}
