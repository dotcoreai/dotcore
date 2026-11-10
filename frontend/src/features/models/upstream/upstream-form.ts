import { create } from "@bufbuild/protobuf";

import { UPSTREAM_MODEL_NAME } from "@/domain/upstream-presentation";
import type { DeploymentExposureValues } from "@/features/models/deployment-exposure";
import {
  type ModelDeployment,
  OpenAiCompatibleEndpointConfigurationSchema,
  UpstreamModelDesiredConfigurationSchema,
  UpstreamModelInputCapabilitiesSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";

export interface UpstreamFormValues extends DeploymentExposureValues {
  name?: string;
  upstreamModelId: string;
  baseUrl: string;
  apiKey: string;
  // These values are intentionally not exposed by the minimal endpoint form.
  supportsText: boolean;
  supportsImage: boolean;
  locality: number;
  trustDomain: string;
  dataEgressClass: number;
  allowInsecureHttp: boolean;
}

export function deriveUpstreamDeploymentName(value: string): string {
  return [...value.trim()].slice(0, 128).join("");
}

export function validateUpstreamBaseUrl(value: string): string | undefined {
  const baseUrl = value.trim();
  if (!baseUrl) return "Base URL is required.";
  if (/^https?:\/\/(?:\/|$)/i.test(baseUrl)) return "Base URL must include a host.";

  let url: URL;
  try {
    url = new URL(baseUrl);
  } catch {
    return "Enter a valid Base URL.";
  }

  if (url.protocol !== "http:" && url.protocol !== "https:")
    return "Base URL must use HTTP or HTTPS.";
  if (!url.hostname) return "Base URL must include a host.";
  if (url.username || url.password) return "Base URL must not include credentials.";
  if (url.search) return "Base URL must not include a query.";
  if (url.hash) return "Base URL must not include a fragment.";
  return undefined;
}

export function validateModelId(value: string): string | undefined {
  return value.trim() ? undefined : "Upstream model ID is required.";
}

export function defaultUpstreamForm(deployment?: ModelDeployment): UpstreamFormValues {
  const config = deployment?.upstreamDesiredConfiguration;
  return {
    exposure: "direct",
    name: deployment?.modelDeploymentName ?? UPSTREAM_MODEL_NAME,
    upstreamModelId: config?.upstreamModelId ?? "",
    baseUrl: config?.endpoint?.baseUrl ?? "",
    // The stored credential is never exposed through a console draft. A blank edit retains it.
    apiKey: "",
    supportsText: config?.inputCapabilities?.supportsText ?? true,
    supportsImage: config?.inputCapabilities?.supportsImage ?? false,
    locality: config?.boundary?.locality ?? 1,
    trustDomain: config?.boundary?.trustDomain ?? "",
    dataEgressClass: config?.boundary?.dataEgressClass ?? 1,
    allowInsecureHttp: config?.endpoint?.allowInsecureHttp ?? false,
  };
}

export function toUpstreamConfiguration(values: UpstreamFormValues) {
  const baseUrl = values.baseUrl.trim();
  return create(UpstreamModelDesiredConfigurationSchema, {
    upstreamModelId: values.upstreamModelId.trim(),
    apiKey: values.apiKey,
    inputCapabilities: create(UpstreamModelInputCapabilitiesSchema, {
      supportsText: values.supportsText,
      supportsImage: values.supportsImage,
    }),
    endpoint: create(OpenAiCompatibleEndpointConfigurationSchema, {
      baseUrl,
      allowInsecureHttp: baseUrl.toLowerCase().startsWith("http://"),
    }),
    boundary: {
      locality: values.locality,
      trustDomain: values.trustDomain.trim() || upstreamTrustDomain(baseUrl),
      dataEgressClass: values.dataEgressClass,
    },
  });
}

function upstreamTrustDomain(baseUrl: string): string {
  try {
    return new URL(baseUrl).hostname;
  } catch {
    return "";
  }
}
