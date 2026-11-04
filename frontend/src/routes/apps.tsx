import type { ApplicationClient } from "@/api/application-client";
import { ApplicationsPage } from "@/features/applications/applications-page";

export interface AppsRouteProps {
  client?: ApplicationClient;
}

export function AppsRoute({ client }: AppsRouteProps) {
  return <ApplicationsPage client={client} />;
}
