import type { ParameterValue } from "@/gen/dotcore/catalog/v1/parameter_pb";
import type { MemoryParameterFit } from "@/gen/dotcore/model/v1/service_pb";
import { safeNumber } from "@/lib/safe-integer";

/**
 * What an estimate on one target says about the engine's memory parameters, in each parameter's
 * own unit: the largest value that still fits beside the configuration's other values, and the
 * value sizing picks for a parameter left empty.
 */
export interface MemoryParameterSizes {
  /** The largest fitting value by parameter ID; a parameter without one has no slider. */
  fits: ReadonlyMap<string, number>;
  /** The value sizing picks by parameter ID, for the parameters it derives. */
  sized: ReadonlyMap<string, number>;
}

/** The memory parameter sizes of an estimate's fits and derived engine parameter values. */
export function memoryParameterSizes(
  fits: readonly MemoryParameterFit[],
  derivedValues: readonly ParameterValue[] | undefined,
): MemoryParameterSizes {
  return {
    fits: numbers(fits.map((fit) => [fit.parameterId, fit.largestFit])),
    sized: numbers(
      (derivedValues ?? []).flatMap((value) =>
        value.value.case === "unsignedInteger"
          ? [[value.parameterId, value.value.value] as const]
          : [],
      ),
    ),
  };
}

// Values a number can't hold exactly are left out, so their field shows no slider or placeholder.
function numbers(entries: readonly (readonly [string, bigint])[]): Map<string, number> {
  return new Map(
    entries.flatMap(([parameterId, value]) => {
      const number = safeNumber(value);
      return number === undefined ? [] : [[parameterId, number] as const];
    }),
  );
}
