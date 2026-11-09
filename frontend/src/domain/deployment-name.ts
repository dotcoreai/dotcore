export const DEPLOYMENT_NAME_MAX_LENGTH = 128;

export function normalizeDeploymentName(value: string): string {
  return value.trim();
}

export function validateDeploymentName(value: string): string | undefined {
  const normalized = normalizeDeploymentName(value);
  if (!normalized) return "Deployment name is required.";
  if (Array.from(normalized).length > DEPLOYMENT_NAME_MAX_LENGTH) {
    return "Deployment name must be 128 characters or fewer.";
  }
  if (/\p{Cc}/u.test(normalized)) {
    return "Deployment name cannot contain control characters.";
  }
  return undefined;
}
