import type { ReactNode, RefObject } from "react";

import { FieldError } from "@/components/ui/field-error";
import type { EngineParameterDefinition } from "@/gen/dotcore/catalog/v1/engine_release_pb";

import { ContextSizeInput, type ContextSizeRange } from "./context-size-input";

interface ContextSizeFieldProps {
  disabled: boolean;
  /** The value an empty input stands for, for an optional parameter resolution fills in. */
  emptyValue?: number;
  error?: string;
  /** A short line under the input, such as what the value means or an action on it. */
  help?: ReactNode;
  /** A short fact beside the name, such as the largest value that fits the selected target. */
  hint?: string;
  input: string;
  inputRefs: RefObject<Map<string, HTMLElement | null>>;
  onBlur: (parameter: EngineParameterDefinition) => void;
  /** Receives each change, and whether it is settled rather than typing in progress. */
  onChange: (parameter: EngineParameterDefinition, input: string, settled: boolean) => void;
  /** An unsigned-integer context size parameter; its schema range bounds every control. */
  parameter: EngineParameterDefinition;
  /** The range the slider is drawn over, shared with other context fields so they line up. */
  sliderRange?: ContextSizeRange;
  /** Where the value comes from, shown after the name. */
  source?: ReactNode;
}

/**
 * Edits a context size engine parameter with the shared context size input, bounded by the
 * parameter's schema range, under its name and above an optional help line. The number input owns
 * validation and focus.
 */
export function ContextSizeField({
  disabled,
  emptyValue,
  error,
  help,
  hint,
  input,
  inputRefs,
  onBlur,
  onChange,
  parameter,
  sliderRange,
  source,
}: ContextSizeFieldProps) {
  if (parameter.parameterType.case !== "unsignedInteger") return null;

  const { maximum, minimum, unit } = parameter.parameterType.value;
  const inputId = `engine-parameter-${parameter.parameterId}`;
  const helpId = `${parameter.parameterId}-help`;
  const errorId = `${parameter.parameterId}-error`;
  const hintId = `${parameter.parameterId}-hint`;
  const describedBy = [hint ? hintId : "", help ? helpId : "", error ? errorId : ""]
    .filter(Boolean)
    .join(" ");

  return (
    <div className="grid min-w-0 gap-2">
      <div className="flex min-w-0 flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
          <label htmlFor={inputId}>{parameter.displayName}</label>
          {source}
        </span>
        {hint ? (
          <span className="text-xs text-muted-foreground" id={hintId}>
            {hint}
          </span>
        ) : null}
      </div>
      <ContextSizeInput
        describedBy={describedBy}
        disabled={disabled}
        emptyValue={emptyValue}
        id={inputId}
        inputRef={(control) => {
          inputRefs.current.set(parameter.parameterId, control);
        }}
        invalid={Boolean(error)}
        label={parameter.displayName}
        maximum={Number(maximum)}
        minimum={Number(minimum)}
        onBlur={() => onBlur(parameter)}
        onChange={(value, settled) => onChange(parameter, value, settled)}
        sliderRange={sliderRange}
        unit={unit}
        value={input}
      />
      {help ? (
        <span className="text-xs text-muted-foreground" id={helpId}>
          {help}
        </span>
      ) : null}
      {error ? <FieldError id={errorId}>{error}</FieldError> : null}
    </div>
  );
}
