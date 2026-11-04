import type { KeyboardEvent, Ref } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { formatQuantity } from "@/lib/format";

import { contextSizePresets } from "./engine-parameter-values";

interface ContextSizeInputProps {
  /** Ids of the texts that describe the number input, such as its description and error. */
  describedBy?: string;
  disabled?: boolean;
  /**
   * The value an empty input stands for, such as one resolution fills in: the number input shows
   * it as its placeholder and the slider sits at it.
   */
  emptyValue?: number;
  /** The number input's id, which the caller's label names. */
  id: string;
  inputRef?: Ref<HTMLInputElement>;
  invalid?: boolean;
  /** The accessible name of the slider and, with "presets", of the preset group. */
  label: string;
  maximum: number;
  minimum: number;
  onBlur: () => void;
  /**
   * Receives each change, and whether it is settled, such as from the slider or a preset, rather
   * than typing in progress.
   */
  onChange: (input: string, settled: boolean) => void;
  /** Receives the slider's and the number input's keys, such as to keep them from a menu. */
  onKeyDown?: (event: KeyboardEvent) => void;
  /**
   * The range the slider is drawn over, when wider than the input's own, so inputs sharing it line
   * up. The slider stops at the input's own bounds and marks the stretch beyond them unreachable.
   */
  sliderRange?: ContextSizeRange;
  unit: string;
  /** The number input's text, which may not be a valid number while the operator types. */
  value: string;
}

/** A range of context sizes, in tokens. */
export interface ContextSizeRange {
  maximum: number;
  minimum: number;
}

/**
 * A context size control: a log-scale slider, an exact number input, and preset chips, all bounded
 * by the given range. The number input takes any text, so its owner validates it; the slider and
 * presets only write values within the range.
 */
export function ContextSizeInput({
  describedBy,
  disabled = false,
  emptyValue,
  id,
  inputRef,
  invalid = false,
  label,
  maximum,
  minimum,
  onBlur,
  onChange,
  onKeyDown,
  sliderRange = { maximum, minimum },
  unit,
  value: input,
}: ContextSizeInputProps) {
  const scale = sliderScale(
    Math.min(minimum, sliderRange.minimum),
    Math.max(maximum, sliderRange.maximum),
  );
  const clamp = (size: number) => Math.min(Math.max(size, minimum), maximum);
  const value = /^\d+$/.test(input) ? Number(input) : undefined;
  const shownValue = value ?? (input === "" ? emptyValue : undefined);
  const presets = contextSizePresets.filter((preset) => preset >= minimum && preset <= maximum);
  const reachableEnd = sliderPosition(scale, maximum) / SLIDER_STEPS;
  const reachableStart = sliderPosition(scale, minimum) / SLIDER_STEPS;

  return (
    <>
      <div className="flex min-w-0 items-center gap-4">
        {scale.low < scale.max ? (
          <div className="relative flex min-w-0 flex-1 items-center">
            <input
              aria-label={label}
              aria-valuetext={
                shownValue === undefined ? undefined : `${formatQuantity(shownValue, "")} ${unit}`
              }
              className="h-5 w-0 min-w-0 flex-1 accent-primary"
              disabled={disabled}
              max={SLIDER_STEPS}
              min={0}
              onBlur={onBlur}
              onChange={(event) =>
                onChange(String(clamp(sliderValue(scale, Number(event.target.value)))), true)
              }
              onKeyDown={onKeyDown}
              type="range"
              value={sliderPosition(scale, clamp(shownValue ?? scale.low))}
            />
            {reachableStart > 0 ? <UnreachableStretch from={0} to={reachableStart} /> : null}
            {reachableEnd < 1 ? <UnreachableStretch from={reachableEnd} to={1} /> : null}
          </div>
        ) : null}
        <div className="flex items-center gap-2">
          <Input
            aria-describedby={describedBy}
            aria-invalid={invalid ? "true" : undefined}
            className="w-28 text-right"
            disabled={disabled}
            id={id}
            inputMode="numeric"
            onBlur={onBlur}
            onChange={(event) => onChange(event.target.value, false)}
            onKeyDown={onKeyDown}
            placeholder={emptyValue === undefined ? undefined : String(emptyValue)}
            ref={inputRef}
            value={input}
          />
          <span className="text-xs text-muted-foreground">{unit}</span>
        </div>
      </div>
      {presets.length ? (
        <div aria-label={`${label} presets`} className="flex flex-wrap gap-1.5" role="group">
          {presets.map((preset) => (
            <Button
              aria-pressed={value === preset}
              className="rounded-full aria-pressed:border-primary aria-pressed:text-primary"
              disabled={disabled}
              key={preset}
              onClick={() => onChange(String(preset), true)}
              size="xs"
              type="button"
              variant="outline"
            >
              {formatQuantity(preset, unit)}
            </Button>
          ))}
        </div>
      ) : null}
    </>
  );
}

/**
 * A striped band over the slider track between two fractions of its travel, marking values the
 * slider can't reach. The thumb travels between its half widths at either end, and a band reaching
 * either end of the travel runs on to the track's edge.
 */
function UnreachableStretch({ from, to }: { from: number; to: number }) {
  const offset = (fraction: number) =>
    `calc(${SLIDER_THUMB_HALF}px + (100% - ${2 * SLIDER_THUMB_HALF}px) * ${fraction})`;

  return (
    <span
      aria-hidden="true"
      className="pointer-events-none absolute h-1.5 rounded-full bg-[repeating-linear-gradient(135deg,var(--muted-foreground)_0_3px,transparent_3px_6px)] opacity-60"
      style={{
        left: from > 0 ? offset(from) : 0,
        right: to < 1 ? offset(1 - to) : 0,
      }}
    />
  );
}

interface SliderScale {
  /** The slider's leftmost value: the minimum, raised to a practical floor. */
  low: number;
  max: number;
  min: number;
}

/** Slider resolution: positions along the log scale between the low end and the maximum. */
const SLIDER_STEPS = 1000;

// Half the native range thumb's width: the track's travel starts and ends this far in.
const SLIDER_THUMB_HALF = 8;

// Contexts under a few hundred tokens are not practical, so the log scale starts there instead of
// spending most of its travel below it; the number input still accepts the full range.
const SLIDER_FLOOR = 512;

function sliderScale(min: number, max: number): SliderScale {
  return { low: Math.min(Math.max(min, SLIDER_FLOOR), max), max, min };
}

function sliderPosition({ low, max }: SliderScale, value: number): number {
  const clamped = Math.min(Math.max(value, low), max);
  return Math.round((Math.log(clamped / low) / Math.log(max / low)) * SLIDER_STEPS);
}

// Values from 1024 tokens up snap to whole multiples of 1024, so the slider lands on round sizes.
function sliderValue({ low, max, min }: SliderScale, position: number): number {
  const raw = low * (max / low) ** (position / SLIDER_STEPS);
  const snapped = raw >= 1024 ? Math.round(raw / 1024) * 1024 : Math.round(raw);
  return Math.min(Math.max(snapped, min), max);
}
