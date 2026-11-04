import type { ModelClient } from "@/api/model-client";
import { ModelsPage } from "@/features/models/models-page";

export interface HomeRouteProps {
  client?: ModelClient;
}

export function HomeRoute({ client }: HomeRouteProps) {
  return <ModelsPage client={client} />;
}
