import type { RefObject } from "react";

import { Field } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import type { SpeculativeOption } from "@/features/models/profiles/model-row";

import { type SpeculativeChoice, speculativeEnabledMinimum } from "./speculative-choice";

interface SpeculativeDecodingFieldProps {
  choice: SpeculativeChoice;
  countRef: RefObject<HTMLInputElement | null>;
  disabled: boolean;
  error?: string;
  onCountBlur: () => void;
  onCountChange: (count: string) => void;
  onMethodChange: (optionId: string) => void;
  /** The speculative options the selected profile's configuration declares, never empty. */
  options: readonly SpeculativeOption[];
}

/**
 * Chooses speculative decoding for a deployment: off, or one speculative option with a token count
 * bounded by that option's range. The count input shows only while an option is chosen.
 */
export function SpeculativeDecodingField({
  choice,
  countRef,
  disabled,
  error,
  onCountBlur,
  onCountChange,
  onMethodChange,
  options,
}: SpeculativeDecodingFieldProps) {
  const option = options.find((candidate) => candidate.optionId === choice.optionId);
  const methodId = "speculative-decoding-method";
  const countId = "speculative-decoding-count";

  return (
    <Field
      className="min-w-0 gap-2 rounded-lg border border-border bg-card p-4"
      controlId={methodId}
      error={error}
      facts={
        option ? (
          <p>
            Range {speculativeEnabledMinimum(option)}–{option.maximum} tokens
          </p>
        ) : undefined
      }
      help={option ? "Draft tokens per step." : undefined}
      label="Speculative decoding"
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
              <Input
                aria-describedby={control["aria-describedby"]}
                aria-invalid={error ? "true" : undefined}
                aria-label="Speculative token count"
                className="w-20 text-right"
                disabled={disabled}
                id={countId}
                inputMode="numeric"
                onBlur={onCountBlur}
                onChange={(event) => onCountChange(event.target.value)}
                ref={countRef}
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
