/**
 * A failed inference API response, read from the OpenAI-style error envelope the inference API
 * answers with. The message is always set, falling back to the HTTP status when the body has none.
 */
export interface InferenceError {
  status: number;
  type?: string;
  code?: string;
  param?: string;
  message: string;
}

/**
 * Reads the error envelope from a failed response's body text. Fields the body doesn't carry,
 * such as on a proxy's HTML error page, are left out.
 */
export function parseErrorEnvelope(
  response: Pick<Response, "status" | "statusText">,
  text: string,
): InferenceError {
  const fallback = response.statusText
    ? `Request failed: ${response.status} ${response.statusText}`
    : `Request failed: ${response.status}`;

  let envelope: Record<string, unknown> = {};
  try {
    const parsed: unknown = JSON.parse(text);
    if (isObject(parsed) && isObject(parsed.error)) envelope = parsed.error;
  } catch {
    // Not JSON: only the status is known.
  }

  return {
    status: response.status,
    type: optionalString(envelope.type),
    code: optionalString(envelope.code),
    param: optionalString(envelope.param),
    message: optionalString(envelope.message) ?? fallback,
  };
}

/** An operator-readable message for a request that never got a response. */
export function describeNetworkError(error: unknown): string {
  const detail = error instanceof Error ? error.message : undefined;
  return detail
    ? `Could not reach the inference API: ${detail}`
    : "Could not reach the inference API.";
}

/** Whether a failed fetch or body read was caused by aborting the request. */
export function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}
