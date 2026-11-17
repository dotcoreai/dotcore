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
  | { kind: "secret"; maximumLength?: number; minimumLength: number }
  | { kind: "stringEnum"; values: readonly string[] }
  | { kind: "text"; maximumLength?: number; minimumLength: number }
  | { kind: "unsignedInteger"; maximum?: bigint; minimum: bigint; unit: string };

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

/**
 * Whether an input leaves the parameter's value out of the request. An empty secret is left out
 * so the server generates one at deploy.
 */
export function omitsParameterValue(view: ParameterView, input: string): boolean {
  return view.type.kind === "secret" && input === "";
}

/** Why an input is not a valid value for its parameter, or `undefined` when it is. */
export function parameterInputError(view: ParameterView, input: string): string | undefined {
  if (omitsParameterValue(view, input)) return undefined;
  if (parameterInputValue(view, input) !== undefined) return undefined;

  switch (view.type.kind) {
    case "boolean":
      return `${view.label} must be enabled or disabled.`;
    case "stringEnum":
      return `Select a ${view.label.toLocaleLowerCase()}.`;
    case "text":
    case "secret": {
      const forbidden = textValueError(view.label, input);
      if (forbidden) return forbidden;
      return `${view.label} must be ${lengthBounds(view.type) ?? "valid"}.`;
    }
    case "unsignedInteger": {
      const { maximum, minimum, unit } = view.type;
      const bounds =
        maximum === undefined
          ? `of ${formatQuantity(minimum, "")} or more`
          : `from ${formatQuantity(minimum, "")} to ${formatQuantity(maximum, "")}`;
      return `${view.label} must be an integer ${bounds}${unit ? ` ${unit}` : ""}.`;
    }
  }
}

/**
 * The typed value an input holds, or `undefined` when it holds none: it is not valid for the
 * parameter, or it leaves the value out.
 */
export function parameterInputValue(
  view: ParameterView,
  input: string,
): ParameterViewValue | undefined {
  if (omitsParameterValue(view, input)) return undefined;

  switch (view.type.kind) {
    case "boolean":
      return input === "true" ? true : input === "false" ? false : undefined;
    case "stringEnum":
      return view.type.values.includes(input) ? input : undefined;
    case "text":
    case "secret": {
      const length = utf8.encode(input).length;
      const { maximumLength = Infinity, minimumLength } = view.type;
      const valid =
        length >= minimumLength && length <= maximumLength && !hasForbiddenTextCharacters(input);
      return valid ? input : undefined;
    }
    case "unsignedInteger": {
      if (!/^\d+$/.test(input.trim())) return undefined;
      const value = BigInt(input.trim());
      const { maximum, minimum } = view.type;
      return value >= minimum && (maximum === undefined || value <= maximum) ? value : undefined;
    }
  }
}

/**
 * The accepted values of an unsigned integer with digit separators, such as "1–2,048", or
 * "256 or more" when it has no maximum.
 */
export function integerBounds(type: {
  maximum?: bigint | number;
  minimum: bigint | number;
}): string {
  const minimum = formatQuantity(type.minimum, "");
  return type.maximum === undefined
    ? `${minimum} or more`
    : `${minimum}–${formatQuantity(type.maximum, "")}`;
}

/**
 * The accepted length of a text or secret value in UTF-8 bytes, such as "1 to 72 bytes", or
 * `undefined` when any length is accepted.
 */
export function lengthBounds(type: {
  maximumLength?: number;
  minimumLength: number;
}): string | undefined {
  const { maximumLength, minimumLength } = type;
  if (maximumLength !== undefined)
    return minimumLength > 0
      ? `${minimumLength} to ${maximumLength} bytes`
      : `at most ${maximumLength} bytes`;
  if (minimumLength > 0)
    return `at least ${minimumLength} ${minimumLength === 1 ? "byte" : "bytes"}`;
  return undefined;
}

/**
 * Why a text or secret value is refused regardless of its length bounds: it contains a control
 * character. Undefined when it is accepted.
 */
export function textValueError(label: string, input: string): string | undefined {
  return hasForbiddenTextCharacters(input)
    ? `${label} can't contain control characters.`
    : undefined;
}

/** Whether a text or secret value holds a control character, which no parameter accepts. */
export function hasForbiddenTextCharacters(input: string): boolean {
  return controlCharacters.test(input);
}

// Lengths count UTF-8 bytes, as the server does.
const utf8 = new TextEncoder();

// eslint-disable-next-line no-control-regex
const controlCharacters = /[\u0000-\u001f\u007f-\u009f]/;
