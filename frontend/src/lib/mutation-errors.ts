import { Code, ConnectError } from "@connectrpc/connect";

/** Error codes that mean the service rejected the request, so its outcome is known. */
export const rejectedMutationCodes = new Set([
  Code.InvalidArgument,
  Code.NotFound,
  Code.AlreadyExists,
  Code.PermissionDenied,
  Code.Unauthenticated,
  Code.FailedPrecondition,
  Code.ResourceExhausted,
  Code.OutOfRange,
  Code.Unimplemented,
]);

/**
 * Describes a failed mutation, separating known rejections from unknown outcomes.
 * `uncertainHint` supplies the domain-specific "check ..." guidance appended to an
 * uncertain-outcome message.
 */
export function describeMutationError(
  action: string,
  error: unknown,
  uncertainHint: (action: string) => string,
): string {
  const connectError = ConnectError.from(error);
  const code = Code[connectError.code] ?? "Unknown";
  const message = connectError.rawMessage || "The service returned no details";

  if (rejectedMutationCodes.has(connectError.code)) {
    return `${action} was rejected (${code}): ${message}. Check the request and service logs.`;
  }

  return `${action} outcome is uncertain (${code}): ${message}. ${uncertainHint(action)}`;
}
