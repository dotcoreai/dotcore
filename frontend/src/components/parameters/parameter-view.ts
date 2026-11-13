import { formatQuantity } from "@/lib/format";

/** One typed parameter as the shared form and details render it, independent of its owner. */
export interface ParameterView {
  /** What the parameter does, shown in the field's help; absent when the label says enough. */
  description?: string;
  id: string;
  label: string;
  type: ParameterViewType;
}

/** The value type and constraints of a parameter view. */
export type ParameterViewType =
  | { kind: "boolean" }
  | { kind: "secret"; minimumLength: number }
  | { kind: "stringEnum"; values: readonly string[] }
  | { kind: "text"; maximumLength: number; minimumLength: number }
  | { kind: "unsignedInteger"; maximum: bigint; minimum: bigint; unit: string };

/** A typed parameter value, matched to its view by type. */
export type ParameterViewValue = bigint | boolean | string;

/** A value as the console shows it, or `undefined` when it does not match the parameter's type. */
export function parameterDisplayValue(
  parameter: ParameterView,
  value: ParameterViewValue | undefined,
): string | undefined {
  switch (parameter.type.kind) {
    case "boolean":
      return typeof value === "boolean" ? (value ? "Enabled" : "Disabled") : undefined;
    case "unsignedInteger":
      return typeof value === "bigint" ? formatQuantity(value, parameter.type.unit) : undefined;
    case "stringEnum":
    case "text":
    case "secret":
      return typeof value === "string" ? value : undefined;
  }
}

/** Why a typed text or secret value is refused whatever the parameter's bounds, if it is. */
export function textValueError(label: string, input: string): string | undefined {
  if (input.includes("=")) return `${label} can't contain "=".`;
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f-\u009f]/.test(input))
    return `${label} can't contain control characters.`;
  return undefined;
}
