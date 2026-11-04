import type { ApplicationClient } from "@/api/application-client";
import { AppCatalogPage } from "@/features/applications/app-catalog-page";

export interface AppCatalogRouteProps {
  client?: ApplicationClient;
}

export function AppCatalogRoute({ client }: AppCatalogRouteProps) {
  return <AppCatalogPage client={client} />;
}
