import type { MessageInitShape } from "@bufbuild/protobuf";

import {
  type SizingScenarioSchema,
  SizingScenarioUnsupportedReason,
} from "@/gen/dotcore/model/v1/service_pb";
import { formatCompactCount } from "@/lib/format";
import { parseSafeInteger } from "@/lib/safe-integer";
import { panelParameter } from "@/panels/panel-stack";

import { defaultMemoryBasis, type MemoryBasis } from "./profile-compatibility";

/**
 * An operator use case that sizing evaluates on every profile: the context each request may use
 * and how many requests run at once, with the total context they share following them unless the
 * scenario sets it. Without one, profiles are sized from their defaults.
 */
export interface SizingScenario {
  /** The largest context one request may use. */
  contextTokensPerRequest: number;
  /** The requests generated at once; absent, each profile keeps its own. */
  parallelRequests?: number;
  /** The context the requests share; absent, it follows the other two. */
  totalContextTokens?: number;
}

/** Reads the scenario of a location search; a missing or invalid value means none. */
export function parseSizingScenario(search: string | URLSearchParams): SizingScenario | undefined {
  const params = new URLSearchParams(search);
  return scenarioFromInputs({
    parallel: params.get(scenarioParallelKey) ?? "",
    perRequest: params.get(scenarioPerRequestKey) ?? "",
    total: params.get(scenarioTotalKey) ?? "",
  });
}

/**
 * Replaces the scenario parameters of a location search, leaving every other parameter. The panel
 * parameters stay last, the order the panel helpers write, so the location stays canonical.
 */
export function applySizingScenario(
  search: string | URLSearchParams,
  scenario: SizingScenario | undefined,
): string {
  const params = new URLSearchParams(search);
  const panels = params.getAll(panelParameter);
  params.delete(scenarioPerRequestKey);
  params.delete(scenarioParallelKey);
  params.delete(scenarioTotalKey);
  params.delete(panelParameter);
  if (scenario) {
    params.set(scenarioPerRequestKey, String(scenario.contextTokensPerRequest));
    if (scenario.parallelRequests !== undefined)
      params.set(scenarioParallelKey, String(scenario.parallelRequests));
    if (scenario.totalContextTokens !== undefined)
      params.set(scenarioTotalKey, String(scenario.totalContextTokens));
  }
  for (const panel of panels) params.append(panelParameter, panel);
  const serialized = params.toString();
  return serialized ? `?${serialized}` : "";
}

/** Reads the memory basis of a location search; a missing or unknown value means free now. */
export function parseMemoryBasis(search: string | URLSearchParams): MemoryBasis {
  return new URLSearchParams(search).get(memoryBasisKey) === "total" ? "total" : defaultMemoryBasis;
}

/**
 * Replaces the memory basis parameter of a location search, leaving every other parameter; free
 * now, the default, leaves none. The panel parameters stay last, as for the scenario.
 */
export function applyMemoryBasis(search: string | URLSearchParams, basis: MemoryBasis): string {
  const params = new URLSearchParams(search);
  const panels = params.getAll(panelParameter);
  params.delete(memoryBasisKey);
  params.delete(panelParameter);
  if (basis !== defaultMemoryBasis) params.set(memoryBasisKey, basis);
  for (const panel of panels) params.append(panelParameter, panel);
  const serialized = params.toString();
  return serialized ? `?${serialized}` : "";
}

/** Whether two scenarios, or two absences of one, are the same. */
export function sameSizingScenario(
  left: SizingScenario | undefined,
  right: SizingScenario | undefined,
): boolean {
  return sizingScenarioKey(left) === sizingScenarioKey(right);
}

/** A scenario as one string that equal scenarios share; empty without one. */
export function sizingScenarioKey(scenario: SizingScenario | undefined): string {
  return scenario
    ? `${scenario.contextTokensPerRequest}/${scenario.parallelRequests ?? ""}/${scenario.totalContextTokens ?? ""}`
    : "";
}

/** The compatibility read's scenario for the catalog's one. */
export function sizingScenarioRequest({
  contextTokensPerRequest,
  parallelRequests,
  totalContextTokens,
}: SizingScenario): MessageInitShape<typeof SizingScenarioSchema> {
  return {
    contextTokensPerRequest: BigInt(contextTokensPerRequest),
    ...(parallelRequests === undefined ? {} : { parallelRequests }),
    ...(totalContextTokens === undefined ? {} : { totalContextTokens: BigInt(totalContextTokens) }),
  };
}

/** A scenario in a few characters, such as "64k per request · 4 parallel · 1M total". */
export function formatSizingScenario({
  contextTokensPerRequest,
  parallelRequests,
  totalContextTokens,
}: SizingScenario): string {
  return [
    `${formatCompactCount(contextTokensPerRequest)} per request`,
    parallelRequests === undefined ? undefined : `${parallelRequests} parallel`,
    totalContextTokens === undefined
      ? undefined
      : `${formatCompactCount(totalContextTokens)} total`,
  ]
    .filter(Boolean)
    .join(" · ");
}

/**
 * Why a profile can't serve the scenario, in the operator's terms, naming the profile's engine and
 * its maximum context when known.
 */
export function sizingUnsupportedText(
  reason: SizingScenarioUnsupportedReason,
  context: { engine?: string; maximumContextTokens?: number },
): string {
  const engine = context.engine || "The engine";
  switch (reason) {
    case SizingScenarioUnsupportedReason.CONTEXT_ABOVE_MAXIMUM:
      return context.maximumContextTokens
        ? `Context per request is above the ${formatCompactCount(context.maximumContextTokens)} maximum context`
        : "Context per request is above the maximum context";
    case SizingScenarioUnsupportedReason.PARAMETER_OUT_OF_RANGE:
      return `${engine} doesn't accept this scenario's settings`;
    case SizingScenarioUnsupportedReason.DERIVED_PARAMETER_OUT_OF_RANGE:
      return `${engine} can't size memory for this scenario`;
    default:
      return "This scenario isn't supported";
  }
}

/** A scenario input: the context per request, or the optional parallel requests or total context. */
export type ScenarioInput = "parallel" | "perRequest" | "total";

/**
 * Why the scenario inputs are invalid, by input. The context per request is a whole number of
 * tokens; the parallel requests are empty or a whole number of at least one; the total context is
 * empty or a whole number of tokens at least the context per request. Each profile checks the
 * values against its own engine's ranges.
 */
export function scenarioInputErrors(
  inputs: Record<ScenarioInput, string>,
): Partial<Record<ScenarioInput, string>> {
  const perRequest = scenarioNumber(inputs.perRequest.trim());
  const parallelInput = inputs.parallel.trim();
  const parallel = scenarioNumber(parallelInput);
  const totalInput = inputs.total.trim();
  const total = scenarioNumber(totalInput);
  const errors: Partial<Record<ScenarioInput, string>> = {};
  if (perRequest === undefined)
    errors.perRequest = "Context per request must be a whole number of tokens.";
  else if (total !== undefined && perRequest > total)
    errors.perRequest = "Context per request must be at most the total context.";
  if (parallelInput && !parallel)
    errors.parallel = "Parallel requests must be a whole number of at least 1.";
  if (totalInput && total === undefined)
    errors.total = "Total context must be a whole number of tokens.";
  return errors;
}

/** The inputs that describe a scenario, empty for the values it leaves to each profile. */
export function scenarioInputs(scenario: SizingScenario): Record<ScenarioInput, string> {
  return {
    parallel: scenario.parallelRequests === undefined ? "" : String(scenario.parallelRequests),
    perRequest: String(scenario.contextTokensPerRequest),
    total: scenario.totalContextTokens === undefined ? "" : String(scenario.totalContextTokens),
  };
}

/** The scenario the inputs describe, when they are valid. */
export function scenarioFromInputs(
  inputs: Record<ScenarioInput, string>,
): SizingScenario | undefined {
  const errors = scenarioInputErrors(inputs);
  if (errors.perRequest || errors.parallel || errors.total) return undefined;

  const parallel = inputs.parallel.trim();
  const total = inputs.total.trim();
  return {
    contextTokensPerRequest: Number(inputs.perRequest.trim()),
    ...(parallel ? { parallelRequests: Number(parallel) } : {}),
    ...(total ? { totalContextTokens: Number(total) } : {}),
  };
}

// The URL keys of the scenario inputs, beside the catalog table view.
const scenarioPerRequestKey = "req";
const scenarioParallelKey = "par";
const scenarioTotalKey = "ctx";
// The URL key of the memory basis, beside the scenario.
const memoryBasisKey = "mem";

// A scenario value travels as a 64-bit integer; the console holds the ones a number holds exactly.
function scenarioNumber(value: string): number | undefined {
  return parseSafeInteger(value);
}
