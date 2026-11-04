import { resolveOrigin } from "@/api/runtime-config";

export const testManagementOrigin = "http://127.0.0.1:50051";

export function resolveManagementOrigin(
  configuredOrigin: string | undefined,
  options: { allowTestFallback?: boolean; allowViteFallback?: boolean } = {},
): string {
  return resolveOrigin("managementOrigin", configuredOrigin, testManagementOrigin, options);
}
