export interface DotcoreRuntimeConfig {
  managementOrigin: string;
  inferenceOrigin: string;
}

type RuntimeOrigin = keyof DotcoreRuntimeConfig;

declare global {
  interface Window {
    __DOTCORE_CONFIG__?: unknown;
  }
}

export function readRuntimeConfig(): DotcoreRuntimeConfig | undefined {
  const candidate = window.__DOTCORE_CONFIG__;
  if (candidate === undefined) {
    return undefined;
  }

  if (!isRecord(candidate)) {
    throw invalidRuntimeConfig("must be an object");
  }

  return {
    managementOrigin: validateOrigin(candidate.managementOrigin, "managementOrigin"),
    inferenceOrigin: validateOrigin(candidate.inferenceOrigin, "inferenceOrigin"),
  };
}

export function resolveOrigin(
  origin: RuntimeOrigin,
  configuredOrigin: string | undefined,
  testFallback: string,
  {
    allowTestFallback = false,
    allowViteFallback = false,
  }: { allowTestFallback?: boolean; allowViteFallback?: boolean } = {},
): string {
  const runtimeConfig = readRuntimeConfig();
  if (runtimeConfig !== undefined) {
    return runtimeConfig[origin];
  }

  if (allowTestFallback && !configuredOrigin?.trim()) {
    return testFallback;
  }

  if (allowViteFallback) {
    return validateOrigin(
      configuredOrigin,
      `VITE_DOTCORE_${origin === "managementOrigin" ? "MANAGEMENT" : "INFERENCE"}_ORIGIN`,
    );
  }

  throw new Error(
    `Dotcore runtime config is missing ${origin}. Provide /config.js with a valid ${origin} before starting the production frontend.`,
  );
}

function validateOrigin(value: unknown, name: string): string {
  if (value === "same-origin") {
    return window.location.origin;
  }

  if (typeof value !== "string" || !value.trim()) {
    throw invalidRuntimeConfig(`${name} must be a non-empty HTTP(S) origin`);
  }

  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw invalidRuntimeConfig(`${name} must be a valid HTTP(S) origin`);
  }

  if (
    (url.protocol !== "http:" && url.protocol !== "https:") ||
    !url.hostname ||
    url.username ||
    url.password ||
    url.pathname !== "/" ||
    url.search ||
    url.hash
  ) {
    throw invalidRuntimeConfig(`${name} must be a valid HTTP(S) origin`);
  }

  return url.origin;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function invalidRuntimeConfig(message: string): Error {
  return new Error(`Invalid Dotcore runtime config: ${message}.`);
}
