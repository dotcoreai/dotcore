import { Outlet } from "react-router-dom";

import type { ApplicationClient } from "@/api/application-client";

import { ApplicationsPollingProvider } from "./applications-polling-provider";

export interface ApplicationsSectionProps {
  client?: ApplicationClient;
}

/** Layout route for the Apps section pages, which share one applications polling instance. */
export function ApplicationsSection({ client }: ApplicationsSectionProps) {
  return (
    <ApplicationsPollingProvider client={client}>
      <Outlet />
    </ApplicationsPollingProvider>
  );
}
