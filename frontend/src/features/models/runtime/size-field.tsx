import type { ReactNode, RefObject } from "react";

import { RangeFact } from "@/components/parameters/parameters";
import { Field } from "@/components/ui/field";
import type { ParameterDefinition } from "@/gen/dotcore/catalog/v1/parameter_pb";
import { isSizeUnit } from "@/lib/format";

import { SizeInput, type SizeLargestFit, type SizeRange } from "./size-input";

interface SizeFieldProps {
  disabled?: boolean;
  /** The value an empty box stands for, for an optional parameter resolution fills in. */
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
  largestFit?: SizeLargestFit;
  onBlur: (parameter: ParameterDefinition) => void;
  /** Receives each change, and whether it is settled rather than typing in progress. */
  onChange: (parameter: ParameterDefinition, input: string, settled: boolean) => void;
  /** An unsigned-integer parameter in tokens or a byte unit; its schema range bounds the box. */
  parameter: ParameterDefinition;
  /**
   * The values the slider and size chips span, such as up to the largest fit; without it, only the
   * box shows.
   */
  range?: SizeRange;
  /** The range the slider is drawn over, shared with sibling fields so they line up. */
  sliderRange?: SizeRange;
  /** One line telling a newcomer what the parameter does, shown under the label. */
  summary?: string;
}

/**
 * Edits a size engine parameter, a context size or a memory size, with the shared size input,
 * under its name and a help popover holding the description, the parameter's schema range and its
 * unit. The box owns validation and focus.
 */
export function SizeField({
  actions,
  disabled = false,
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
  range,
  sliderRange,
  summary,
}: SizeFieldProps) {
  if (parameter.parameterType.case !== "unsignedInteger") return null;

  const { maximum, minimum, unit } = parameter.parameterType.value;
  if (!isSizeUnit(unit)) return null;

  const inputId = `engine-parameter-${parameter.parameterId}`;
  return (
    <Field
      actions={actions}
      className="min-w-0 gap-2"
      controlId={inputId}
      error={error}
      facts={
        <>
          <RangeFact maximum={maximum} minimum={minimum} unit={unit} />
          {facts?.map((fact) => (
            <p key={fact}>{fact}</p>
          ))}
        </>
      }
      help={help}
      label={parameter.displayName}
      summary={summary}
    >
      {(control) => (
        <SizeInput
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
          onBlur={() => onBlur(parameter)}
          onChange={(value, settled) => onChange(parameter, value, settled)}
          range={range}
          sliderRange={sliderRange}
          unit={unit}
          value={input}
        />
      )}
    </Field>
  );
}
