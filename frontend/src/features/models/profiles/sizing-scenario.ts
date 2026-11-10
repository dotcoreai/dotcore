import type { MessageInitShape } from "@bufbuild/protobuf";

import {
  type SizingScenarioSchema,
  SizingScenarioUnsupportedReason,
} from "@/gen/dotcore/model/v1/service_pb";
import { formatCompactCount } from "@/lib/format";
import { panelParameter } from "@/panels/panel-stack";

import { defaultMemoryBasis, type MemoryBasis } from "./profile-compatibility";

/**
 * An operator use case that sizing evaluates on every profile: a total context shared by the
 * requests a deployment serves at once, each optionally capped at a context per request. Without
 * one, profiles are sized from their defaults.
 */
export interface SizingScenario {
  /** The largest context one request may use; absent, each request may use up to the maximum. */
  contextTokensPerRequest?: number;
  totalContextTokens: number;
}

/** The smallest total context, or context per request, a scenario accepts, in tokens. */
export const minimumScenarioContextTokens = 1024;

/** Reads the scenario of a location search; a missing or invalid value means none. */
export function parseSizingScenario(search: string | URLSearchParams): SizingScenario | undefined {
  const params = new URLSearchParams(search);
  const totalContextTokens = scenarioNumber(params.get(scenarioTotalKey) ?? "");
  if (totalContextTokens === undefined) return undefined;

  const perRequest = params.get(scenarioPerRequestKey);
  if (perRequest === null) return { totalContextTokens };

  const contextTokensPerRequest = scenarioNumber(perRequest);
  return contextTokensPerRequest !== undefined && contextTokensPerRequest <= totalContextTokens
    ? { contextTokensPerRequest, totalContextTokens }
    : undefined;
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
  params.delete(scenarioTotalKey);
  params.delete(scenarioPerRequestKey);
  params.delete(panelParameter);
  if (scenario) {
    params.set(scenarioTotalKey, String(scenario.totalContextTokens));
    if (scenario.contextTokensPerRequest !== undefined)
      params.set(scenarioPerRequestKey, String(scenario.contextTokensPerRequest));
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
  return scenario ? `${scenario.totalContextTokens}/${scenario.contextTokensPerRequest ?? ""}` : "";
}

/** The compatibility read's scenario for the catalog's one. */
export function sizingScenarioRequest({
  contextTokensPerRequest,
  totalContextTokens,
}: SizingScenario): MessageInitShape<typeof SizingScenarioSchema> {
  return contextTokensPerRequest === undefined
    ? { totalContextTokens }
    : { contextTokensPerRequest, totalContextTokens };
}

/** A scenario in a few characters, such as "1M" or "1M · 64k per request". */
export function formatSizingScenario({
  contextTokensPerRequest,
  totalContextTokens,
}: SizingScenario): string {
  const total = formatCompactCount(totalContextTokens);
  return contextTokensPerRequest === undefined
    ? total
    : `${total} · ${formatCompactCount(contextTokensPerRequest)} per request`;
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

/** A scenario input: the total context, or the optional context per request. */
export type ScenarioInput = "perRequest" | "total";

/**
 * Why the scenario inputs are invalid, by input. The total context is a whole number of at least
 * 1024 tokens; the context per request is empty, or such a number at most the total context.
 */
export function scenarioInputErrors(
  inputs: Record<ScenarioInput, string>,
): Partial<Record<ScenarioInput, string>> {
  const total = scenarioNumber(inputs.total.trim());
  const perRequestInput = inputs.perRequest.trim();
  const perRequest = perRequestInput ? scenarioNumber(perRequestInput) : undefined;
  const errors: Partial<Record<ScenarioInput, string>> = {};
  if (total === undefined)
    errors.total = `Total context must be a whole number of at least ${minimumScenarioContextTokens} tokens.`;
  if (perRequestInput && perRequest === undefined)
    errors.perRequest = `Context per request must be a whole number of at least ${minimumScenarioContextTokens} tokens.`;
  else if (perRequest !== undefined && total !== undefined && perRequest > total)
    errors.perRequest = "Context per request must be at most the total context.";
  return errors;
}

/** The inputs that describe a scenario, with an empty context per request when it sets none. */
export function scenarioInputs(scenario: SizingScenario): Record<ScenarioInput, string> {
  return {
    perRequest:
      scenario.contextTokensPerRequest === undefined
        ? ""
        : String(scenario.contextTokensPerRequest),
    total: String(scenario.totalContextTokens),
  };
}

/** The scenario the inputs describe, when they are valid. */
export function scenarioFromInputs(
  inputs: Record<ScenarioInput, string>,
): SizingScenario | undefined {
  const errors = scenarioInputErrors(inputs);
  if (errors.total || errors.perRequest) return undefined;

  const perRequest = inputs.perRequest.trim();
  return {
    ...(perRequest ? { contextTokensPerRequest: Number(perRequest) } : {}),
    totalContextTokens: Number(inputs.total.trim()),
  };
}

/**
 * The inputs with the one the operator didn't edit following the edited one, so the context per
 * request stays at most the total context: a larger context per request raises the total, and a
 * smaller total lowers the context per request. Unchanged inputs come back as the same object, as
 * do inputs that aren't both whole numbers, an empty context per request, and an edited value below
 * the smallest scenario context, which would carry a mistake over to the other input.
 */
export function followContextInputs(
  inputs: Record<ScenarioInput, string>,
  edited: ScenarioInput,
): Record<ScenarioInput, string> {
  const total = inputs.total.trim();
  const perRequest = inputs.perRequest.trim();
  if (!/^\d+$/.test(total) || !/^\d+$/.test(perRequest)) return inputs;
  if (BigInt(perRequest) <= BigInt(total)) return inputs;
  if (Number(edited === "total" ? total : perRequest) < minimumScenarioContextTokens) return inputs;

  return edited === "total" ? { ...inputs, perRequest: total } : { ...inputs, total: perRequest };
}

// The largest value either input accepts: the transport's unsigned 32-bit range.
const maximumScenarioValue = 4_294_967_295;

// The URL keys of the scenario inputs, beside the catalog table view.
const scenarioTotalKey = "ctx";
const scenarioPerRequestKey = "req";
// The URL key of the memory basis, beside the scenario.
const memoryBasisKey = "mem";

function scenarioNumber(value: string): number | undefined {
  if (!/^\d+$/.test(value)) return undefined;
  const number = Number(value);
  return number >= minimumScenarioContextTokens && number <= maximumScenarioValue
    ? number
    : undefined;
}
