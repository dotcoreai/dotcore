import {
  describeNetworkError,
  type InferenceError,
  isAbortError,
  parseErrorEnvelope,
} from "@/api/inference-errors";
import { consoleInferenceOrigin } from "@/api/inference-origin";

export interface DecisionRequest {
  /** The inference route name, spliced into the body as the System One `model`. */
  model: string;
  /** The System One request body as JSON text, without `model`. */
  body: string;
  signal?: AbortSignal;
}

/**
 * One answer of a System One response. Choice probabilities are keyed by option name and score
 * probabilities are indexed by level; neither carries the request's order, which callers take
 * from the request.
 */
export type DecisionAnswer =
  | {
      type: "choice";
      choice: string;
      probabilities?: ReadonlyMap<string, number>;
      confidence?: number;
    }
  | { type: "score"; score: number; probabilities?: readonly number[]; confidence?: number }
  | { type: "noul"; yesProbability: number; confidence?: number };

export interface DecisionUsage {
  inputTokens: number;
}

/**
 * The outcome of one decision request. Every outcome that got a response keeps its raw text.
 * Answers are keyed by question name, without the request's question order.
 */
export type DecisionResult =
  | {
      kind: "answered";
      responseText: string;
      answers: ReadonlyMap<string, DecisionAnswer>;
      usage?: DecisionUsage;
      latencyMs: number;
    }
  | { kind: "unreadable"; responseText: string; message: string }
  | { kind: "failed"; responseText: string; error: InferenceError }
  | { kind: "unreachable"; message: string }
  | { kind: "invalid-body"; message: string };

export interface DecisionClient {
  /** Resolves with the outcome, or with nothing when the request was aborted. Never rejects. */
  ask: (request: DecisionRequest) => Promise<DecisionResult | undefined>;
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

/**
 * Client for the System One decision endpoint of the inference API. The body is sent as the
 * caller wrote it, since the inference API reads question order from key order and rejects
 * duplicate question names, both of which a JavaScript object would lose. Latency is measured
 * with the given clock, in milliseconds.
 */
export function createDecisionClient(
  origin = consoleInferenceOrigin(),
  fetchImpl: FetchLike = (input, init) => globalThis.fetch(input, init),
  now: () => number = () => performance.now(),
): DecisionClient {
  const endpoint = `${origin.replace(/\/+$/, "")}/v1/systemone`;

  async function ask({
    model,
    body,
    signal,
  }: DecisionRequest): Promise<DecisionResult | undefined> {
    const bodyError = checkBody(body);
    if (bodyError) return { kind: "invalid-body", message: bodyError };

    const started = now();
    let response: Response;
    let responseText: string;
    try {
      response = await fetchImpl(endpoint, {
        method: "POST",
        headers: { "Content-Type": "application/json", Accept: "application/json" },
        body: decisionRequestText(model, body),
        signal,
      });
    } catch (error) {
      if (signal?.aborted || isAbortError(error)) return undefined;
      return { kind: "unreachable", message: describeNetworkError(error) };
    }
    try {
      responseText = await response.text();
    } catch (error) {
      if (signal?.aborted || isAbortError(error)) return undefined;
      // The inference API answered, so its status is known even though its body never arrived.
      return {
        kind: "failed",
        responseText: "",
        error: { status: response.status, message: "The response was cut off before it arrived." },
      };
    }
    const latencyMs = now() - started;
    // A result that arrives after an abort belongs to a request the caller dropped.
    if (signal?.aborted) return undefined;

    if (!response.ok) {
      return { kind: "failed", responseText, error: parseErrorEnvelope(response, responseText) };
    }

    const parsed = parseResponse(responseText);
    if (!parsed) {
      return {
        kind: "unreadable",
        responseText,
        message: "The inference API sent an unreadable response.",
      };
    }
    return { kind: "answered", responseText, ...parsed, latencyMs };
  }

  return { ask };
}

export const decisionClient = createDecisionClient();

/**
 * The request text sent for a body, a JSON object without `model`: `model` written as its first
 * field, and the rest as written.
 */
export function decisionRequestText(model: string, body: string): string {
  const fields = body.trimStart().slice(1);
  const separator = fields.trimStart().startsWith("}") ? "" : ",";
  return `{"model":${JSON.stringify(model)}${separator}${fields}`;
}

/** Why a body can't be sent: it must be a JSON object that leaves `model` to the route. */
function checkBody(body: string): string | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(body);
  } catch {
    return "The request body is not valid JSON.";
  }

  if (!isObject(parsed)) return "The request body must be a JSON object.";
  if (Object.hasOwn(parsed, "model")) return "The request body must not set model.";
  return undefined;
}

function parseResponse(
  text: string,
): { answers: Map<string, DecisionAnswer>; usage?: DecisionUsage } | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return undefined;
  }
  if (!isObject(parsed) || !isObject(parsed.answers)) return undefined;

  const answers = new Map<string, DecisionAnswer>();
  for (const [name, value] of Object.entries(parsed.answers)) {
    const answer = parseAnswer(value);
    if (!answer) return undefined;
    answers.set(name, answer);
  }

  if (parsed.usage === undefined) return { answers };
  const inputTokens = isObject(parsed.usage) ? parsed.usage.input_tokens : undefined;
  if (!isNumber(inputTokens)) return undefined;
  return { answers, usage: { inputTokens } };
}

function parseAnswer(value: unknown): DecisionAnswer | undefined {
  if (!isObject(value)) return undefined;

  const confidence = value.confidence;
  if (confidence !== undefined && !isNumber(confidence)) return undefined;
  const withConfidence = confidence === undefined ? {} : { confidence };

  switch (value.type) {
    case "choice": {
      if (typeof value.choice !== "string") return undefined;
      if (value.probabilities === undefined) {
        return { type: "choice", choice: value.choice, ...withConfidence };
      }
      const probabilities = numberEntries(value.probabilities);
      if (!probabilities) return undefined;
      return {
        type: "choice",
        choice: value.choice,
        probabilities: new Map(probabilities),
        ...withConfidence,
      };
    }

    case "score": {
      if (!isNumber(value.score)) return undefined;
      if (value.probabilities === undefined) {
        return { type: "score", score: value.score, ...withConfidence };
      }
      const probabilities = levelProbabilities(value.probabilities);
      if (!probabilities) return undefined;
      return { type: "score", score: value.score, probabilities, ...withConfidence };
    }

    case "noul":
      if (!isNumber(value.noul)) return undefined;
      return { type: "noul", yesProbability: value.noul, ...withConfidence };

    default:
      return undefined;
  }
}

/** Score probabilities keyed by level index, as an array covering every level from zero. */
function levelProbabilities(value: unknown): number[] | undefined {
  const entries = numberEntries(value);
  if (!entries) return undefined;

  const levels: number[] = [];
  for (const [key, probability] of entries) {
    const index = Number(key);
    if (!Number.isInteger(index) || index < 0 || String(index) !== key) return undefined;
    levels[index] = probability;
  }

  if (levels.length !== entries.length) return undefined;
  return levels;
}

function numberEntries(value: unknown): [string, number][] | undefined {
  if (!isObject(value)) return undefined;
  const entries = Object.entries(value);
  return entries.every((entry): entry is [string, number] => isNumber(entry[1]))
    ? entries
    : undefined;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}
