import type { RouteClient } from "@/api/route-client";
import { RoutesPage } from "@/features/routes/routes-page";

export function RoutesRoute({ client }: { client?: RouteClient }) {
  return <RoutesPage client={client} />;
}
