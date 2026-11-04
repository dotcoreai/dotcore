import type { ModelClient } from "@/api/model-client";
import type { SiteClient } from "@/api/site-client";
import { ModelCatalogPage } from "@/features/models/model-catalog-page";

export interface ModelCatalogRouteProps {
  client?: ModelClient;
  siteClient?: SiteClient;
}

export function ModelCatalogRoute({ client, siteClient }: ModelCatalogRouteProps) {
  return <ModelCatalogPage client={client} siteClient={siteClient} />;
}
