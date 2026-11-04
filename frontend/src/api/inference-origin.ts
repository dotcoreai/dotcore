import { resolveOrigin } from "@/api/runtime-config";

export const testInferenceOrigin = "http://127.0.0.1:50052";

export function resolveInferenceOrigin(
  configuredOrigin: string | undefined,
  options: { allowTestFallback?: boolean; allowViteFallback?: boolean } = {},
): string {
  return resolveOrigin("inferenceOrigin", configuredOrigin, testInferenceOrigin, options);
}

/** The inference origin the browser console uses, with any trailing slash removed. */
export function consoleInferenceOrigin(): string {
  return resolveInferenceOrigin(import.meta.env.VITE_DOTCORE_INFERENCE_ORIGIN, {
    allowTestFallback: import.meta.env.MODE === "test",
    allowViteFallback: import.meta.env.DEV || import.meta.env.MODE === "test",
  }).replace(/\/+$/, "");
}
