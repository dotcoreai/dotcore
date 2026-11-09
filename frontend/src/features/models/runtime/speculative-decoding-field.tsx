import type { RefObject } from "react";

import { FieldError } from "@/components/ui/field-error";
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
  const descriptionId = "speculative-decoding-count-description";
  const errorId = "speculative-decoding-count-error";

  return (
    <div className="grid min-w-0 gap-2 rounded-lg border border-border bg-card p-4">
      <label className="text-sm font-semibold" htmlFor={methodId}>
        Speculative decoding
      </label>
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <NativeSelect
          disabled={disabled}
          id={methodId}
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
              aria-describedby={error ? `${descriptionId} ${errorId}` : descriptionId}
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
      {option ? (
        <span className="text-xs text-muted-foreground" id={descriptionId}>
          {`Draft tokens per step. ${speculativeEnabledMinimum(option)}–${option.maximum}`}
        </span>
      ) : null}
      {error ? <FieldError id={errorId}>{error}</FieldError> : null}
    </div>
  );
}
