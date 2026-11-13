import type { ReactNode, RefObject } from "react";

import { Field } from "@/components/ui/field";
import type { ParameterDefinition } from "@/gen/dotcore/catalog/v1/parameter_pb";

import {
  ContextSizeInput,
  type ContextSizeLargestFit,
  type ContextSizeRange,
} from "./context-size-input";

interface ContextSizeFieldProps {
  disabled: boolean;
  /** The value an empty input stands for, for an optional parameter resolution fills in. */
  emptyValue?: number;
  error?: string;
  /** Buttons that change the value, shown under the input. */
  actions?: ReactNode;
  /** Facts about the value in the help popover, such as the largest value that fits. */
  facts?: readonly string[];
  /** What the parameter does, shown in the help popover. */
  help?: string;
  input: string;
  inputRefs: RefObject<Map<string, HTMLElement | null>>;
  /** The largest size that fits the selected target, offered as the input's last chip. */
  largestFit?: ContextSizeLargestFit;
  onBlur: (parameter: ParameterDefinition) => void;
  /** Receives each change, and whether it is settled rather than typing in progress. */
  onChange: (parameter: ParameterDefinition, input: string, settled: boolean) => void;
  /** An unsigned-integer context size parameter; its schema range bounds every control. */
  parameter: ParameterDefinition;
  /** The range the slider is drawn over, shared with other context fields so they line up. */
  sliderRange?: ContextSizeRange;
}

/**
 * Edits a context size engine parameter with the shared context size input, bounded by the
 * parameter's schema range, under its name and a help popover holding the description, range and
 * unit. The number input owns validation and focus.
 */
export function ContextSizeField({
  actions,
  disabled,
  emptyValue,
  error,
  facts,
  help,
  input,
  inputRefs,
  largestFit,
  onBlur,
  onChange,
  parameter,
  sliderRange,
}: ContextSizeFieldProps) {
  if (parameter.parameterType.case !== "unsignedInteger") return null;

  const { maximum, minimum, unit } = parameter.parameterType.value;
  const inputId = `engine-parameter-${parameter.parameterId}`;
  return (
    <Field
      actions={actions}
      className="min-w-0 gap-2"
      controlId={inputId}
      error={error}
      facts={
        <>
          <p>
            Range {`${minimum}–${maximum}`}
            {unit ? ` ${unit}` : ""}
          </p>
          {facts?.map((fact) => (
            <p key={fact}>{fact}</p>
          ))}
        </>
      }
      help={help}
      label={parameter.displayName}
    >
      {(control) => (
        <ContextSizeInput
          describedBy={control["aria-describedby"]}
          disabled={disabled}
          emptyValue={emptyValue}
          id={control.id}
          inputRef={(element) => {
            inputRefs.current.set(parameter.parameterId, element);
          }}
          invalid={Boolean(error)}
          largestFit={largestFit}
          label={parameter.displayName}
          maximum={Number(maximum)}
          minimum={Number(minimum)}
          onBlur={() => onBlur(parameter)}
          onChange={(value, settled) => onChange(parameter, value, settled)}
          sliderRange={sliderRange}
          unit={unit}
          value={input}
        />
      )}
    </Field>
  );
}
