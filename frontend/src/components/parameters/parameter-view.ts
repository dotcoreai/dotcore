import { formatQuantity } from "@/lib/format";

/** One typed parameter as the shared form and details render it, independent of its owner. */
export interface ParameterView {
  /** A short line under the field; absent when the label and value say enough. */
  hint?: string;
  id: string;
  label: string;
  type: ParameterViewType;
}

/** The value type and constraints of a parameter view. */
export type ParameterViewType =
  | { kind: "boolean" }
  | { kind: "stringEnum"; values: readonly string[] }
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
      return typeof value === "string" ? value : undefined;
  }
}
