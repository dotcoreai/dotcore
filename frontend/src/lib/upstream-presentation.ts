import { UpstreamModelLocality } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";

export const UPSTREAM_MODEL_NAME = "Upstream model";

export function upstreamModelPresentationName(deploymentId?: string): string {
  return deploymentId ? `${UPSTREAM_MODEL_NAME} (${deploymentId})` : UPSTREAM_MODEL_NAME;
}

/**
 * Where an upstream provider runs, as one lowercase word for a description line. An unspecified
 * boundary has no word, so the caller omits the segment rather than printing a placeholder.
 */
export function upstreamLocalityLabel(locality: UpstreamModelLocality): string | undefined {
  switch (locality) {
    case UpstreamModelLocality.PUBLIC_EXTERNAL:
      return "hosted";
    case UpstreamModelLocality.PRIVATE:
      return "private";
    case UpstreamModelLocality.LOCAL:
      return "local";
    default:
      return undefined;
  }
}
