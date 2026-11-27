import type { RefObject } from "react";

import { CountStepper } from "@/components/ui/count-stepper";
import { Field } from "@/components/ui/field";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { integerBounds } from "@/domain/parameter-view";
import type { SpeculativeOption } from "@/features/models/profiles/model-row";

import { type SpeculativeChoice, speculativeEnabledMinimum } from "./speculative-choice";

interface SpeculativeDecodingFieldProps {
  choice: SpeculativeChoice;
  countRef: RefObject<HTMLInputElement | null>;
  /** What speculative decoding does and costs, from the catalog. */
  description?: string;
  disabled: boolean;
  error?: string;
  onCountBlur: () => void;
  onCountChange: (count: string) => void;
  onMethodChange: (optionId: string) => void;
  /** The speculative options the selected profile's configuration declares, never empty. */
  options: readonly SpeculativeOption[];
  /** One line telling a newcomer what speculative decoding does, from the catalog. */
  summary?: string;
}

/**
 * Chooses speculative decoding for a deployment: off, or one speculative option with a token count
 * bounded by that option's range. The count input shows only while a counted option is chosen; a
 * switch option has none.
 */
export function SpeculativeDecodingField({
  choice,
  countRef,
  description,
  disabled,
  error,
  onCountBlur,
  onCountChange,
  onMethodChange,
  options,
  summary,
}: SpeculativeDecodingFieldProps) {
  const chosen = options.find((candidate) => candidate.optionId === choice.optionId);
  const option = chosen?.switch ? undefined : chosen;
  const methodId = "speculative-decoding-method";
  const countId = "speculative-decoding-count";

  return (
    <Field
      controlId={methodId}
      error={error}
      facts={
        option ? (
          <p>
            Range{" "}
            {integerBounds({ maximum: option.maximum, minimum: speculativeEnabledMinimum(option) })}{" "}
            tokens
          </p>
        ) : undefined
      }
      help={description || undefined}
      label="Speculative decoding"
      summary={summary || undefined}
    >
      {(control) => (
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <NativeSelect
            disabled={disabled}
            id={control.id}
            onChange={(event) => onMethodChange(event.target.value)}
            value={choice.optionId}
          >
            <NativeSelectOption value="">Off</NativeSelectOption>
            {options.map((candidate) => (
              <NativeSelectOption key={candidate.optionId} value={candidate.optionId}>
                {candidate.displayName}
              </NativeSelectOption>
            ))}
          </NativeSelect>
          {option ? (
            <>
              <CountStepper
                aria-describedby={control["aria-describedby"]}
                aria-invalid={error ? "true" : undefined}
                aria-label="Speculative token count"
                disabled={disabled}
                id={countId}
                inputRef={countRef}
                label="Speculative token count"
                maximum={option.maximum}
                minimum={speculativeEnabledMinimum(option)}
                onBlur={onCountBlur}
                onChange={onCountChange}
                value={choice.count}
              />
              <span className="text-xs text-muted-foreground">tokens</span>
            </>
          ) : null}
        </div>
      )}
    </Field>
  );
}
