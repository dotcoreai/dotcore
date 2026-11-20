import { type KeyboardEvent, type Ref, useState } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  bytesPerUnit,
  formatExactSize,
  formatExactTokens,
  formatQuantity,
  parseSize,
  type SizeUnit,
} from "@/lib/format";
import { parseSafeInteger } from "@/lib/safe-integer";

import { contextSizePresets } from "./engine-parameter-values";

interface SizeInputProps {
  /** Ids of the texts that describe the box, such as its description and error. */
  describedBy?: string;
  disabled?: boolean;
  /**
   * The value an empty box stands for, such as the one sizing picks: the box shows it as its
   * placeholder and the slider sits at it.
   */
  emptyValue?: number;
  /** The box's id, which the caller's label names. */
  id: string;
  inputRef?: Ref<HTMLInputElement>;
  invalid?: boolean;
  /**
   * The largest size that fits where the model runs, offered as the last chip. The size chips stop
   * below it. Its click sets the value like a size chip.
   */
  largestFit?: SizeLargestFit;
  /** The accessible name of the slider and of the chip group. */
  label: string;
  onBlur: () => void;
  /**
   * Receives each change as digits in the unit, or the typed text when it isn't a size, and whether
   * it is settled, such as from the slider or a chip, rather than typing in progress.
   */
  onChange: (input: string, settled: boolean) => void;
  /** Receives the slider's and the box's keys, such as to keep them from a menu. */
  onKeyDown?: (event: KeyboardEvent) => void;
  /**
   * The values the slider and size chips write; the box takes any. Without it, only the box shows.
   */
  range?: SizeRange;
  /**
   * The range the slider is drawn over, when wider than the input's own, so inputs sharing it line
   * up. The slider stops at the input's own bounds and marks the stretch beyond them unreachable.
   */
  sliderRange?: SizeRange;
  /** The unit of the value: tokens get a log-scale slider, byte units a linear one. */
  unit: SizeUnit;
  /** The owner's value: digits in the unit, empty, or typed text that isn't a size. */
  value: string;
}

/** The largest size that fits. */
export interface SizeLargestFit {
  value: number;
}

/** A range of sizes, in the input's unit. */
export interface SizeRange {
  maximum: number;
  minimum: number;
}

/**
 * A size control: a slider, a box showing the size in the largest exact unit, and size chips with
 * an optional largest fit chip last. The box reads typed sizes with unit suffixes and hands its
 * owner digits in the input's unit; the owner validates them. A chip is pressed only right after
 * it is clicked, until the slider moves or the operator types.
 */
export function SizeInput({
  describedBy,
  disabled = false,
  emptyValue,
  id,
  inputRef,
  invalid = false,
  largestFit,
  label,
  onBlur,
  onChange,
  onKeyDown,
  range,
  sliderRange = range,
  unit,
  value: input,
}: SizeInputProps) {
  // The text being typed, kept while the owner still holds the value it was read as.
  const [draft, setDraft] = useState<{ input: string; text: string }>();
  const [pressed, setPressed] = useState<number>();
  const value = parseSafeInteger(input.trim());
  const shownValue = value ?? (input.trim() === "" ? emptyValue : undefined);
  const text =
    draft?.input === input ? draft.text : value === undefined ? input : exactSize(value, unit);
  const chips = range
    ? sizeChips(unit, range).filter((chip) => largestFit === undefined || chip < largestFit.value)
    : [];

  const type = (typed: string) => {
    const trimmed = typed.trim();
    const parsed = trimmed === "" ? "" : parseSize(trimmed, unit);
    const next = parsed === undefined ? typed : String(parsed);
    setDraft({ input: next, text: typed });
    setPressed(undefined);
    onChange(next, false);
  };
  const choose = (chip: number) => {
    setDraft(undefined);
    setPressed(chip);
    onChange(String(chip), true);
  };

  return (
    <>
      <div className="flex min-w-0 items-center gap-4">
        {range && sliderRange ? (
          <SizeSlider
            disabled={disabled}
            label={label}
            onBlur={onBlur}
            onChange={(size) => {
              setDraft(undefined);
              setPressed(undefined);
              onChange(String(size), true);
            }}
            onKeyDown={onKeyDown}
            range={range}
            sliderRange={sliderRange}
            unit={unit}
            value={shownValue}
          />
        ) : null}
        <Input
          aria-describedby={describedBy}
          aria-invalid={invalid ? "true" : undefined}
          className="w-28 text-right placeholder:italic"
          disabled={disabled}
          id={id}
          onBlur={() => {
            setDraft(undefined);
            onBlur();
          }}
          onChange={(event) => type(event.target.value)}
          onKeyDown={onKeyDown}
          placeholder={emptyValue === undefined ? undefined : exactSize(emptyValue, unit)}
          ref={inputRef}
          value={text}
        />
      </div>
      {chips.length || largestFit ? (
        <div aria-label={`${label} presets`} className="flex flex-wrap gap-1.5" role="group">
          {chips.map((chip) => (
            <SizeChip
              disabled={disabled}
              key={chip}
              label={formatQuantity(chip, unit)}
              onClick={() => choose(chip)}
              pressed={pressed === chip && value === chip}
            />
          ))}
          {largestFit ? (
            <SizeChip
              ariaLabel={`${formatQuantity(largestFit.value, unit)}, largest fit`}
              disabled={disabled}
              label={formatQuantity(largestFit.value, unit)}
              largestFit
              onClick={() => {
                setDraft(undefined);
                setPressed(largestFit.value);
                if (value !== largestFit.value) onChange(String(largestFit.value), true);
              }}
              pressed={pressed === largestFit.value && value === largestFit.value}
            />
          ) : null}
        </div>
      ) : null}
    </>
  );
}

/**
 * The size slider, drawn over the shared slider range and stopping at the input's own range. It
 * sits at the shown value, or at the range's start without one, and writes only values within the
 * input's range.
 */
function SizeSlider({
  disabled,
  label,
  onBlur,
  onChange,
  onKeyDown,
  range,
  sliderRange,
  unit,
  value,
}: {
  disabled: boolean;
  label: string;
  onBlur: () => void;
  onChange: (size: number) => void;
  onKeyDown?: (event: KeyboardEvent) => void;
  range: SizeRange;
  sliderRange: SizeRange;
  unit: SizeUnit;
  value: number | undefined;
}) {
  const scale = sliderScale(
    unit,
    Math.min(range.minimum, sliderRange.minimum),
    Math.max(range.maximum, sliderRange.maximum),
  );
  if (scale.low >= scale.max) return null;

  const clamp = (size: number) => Math.min(Math.max(size, range.minimum), range.maximum);
  const reachableEnd = scale.position(range.maximum) / SLIDER_STEPS;
  const reachableStart = scale.position(range.minimum) / SLIDER_STEPS;

  return (
    <div className="relative flex min-w-0 flex-1 items-center">
      <input
        aria-label={label}
        aria-valuetext={value === undefined ? undefined : spokenSize(value, unit)}
        className="h-5 w-0 min-w-0 flex-1 accent-primary"
        disabled={disabled}
        max={SLIDER_STEPS}
        min={0}
        onBlur={onBlur}
        onChange={(event) => onChange(clamp(scale.value(Number(event.target.value))))}
        onKeyDown={onKeyDown}
        type="range"
        value={scale.position(clamp(value ?? scale.low))}
      />
      {reachableStart > 0 ? <UnreachableStretch from={0} to={reachableStart} /> : null}
      {reachableEnd < 1 ? <UnreachableStretch from={reachableEnd} to={1} /> : null}
    </div>
  );
}

/** A round size chip; the largest fit chip keeps a dashed border while not pressed. */
function SizeChip({
  ariaLabel,
  disabled,
  label,
  largestFit = false,
  onClick,
  pressed,
}: {
  ariaLabel?: string;
  disabled: boolean;
  label: string;
  largestFit?: boolean;
  onClick: () => void;
  pressed: boolean;
}) {
  return (
    <Button
      aria-label={ariaLabel}
      aria-pressed={pressed}
      className="rounded-full data-[largest-fit]:aria-[pressed=false]:border-dashed aria-pressed:border-primary aria-pressed:text-primary"
      data-largest-fit={largestFit ? "" : undefined}
      disabled={disabled}
      onClick={onClick}
      size="xs"
      type="button"
      variant="outline"
    >
      {label}
    </Button>
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

/** Maps sizes to slider positions and back, between the scale's low end and its maximum. */
interface SliderScale {
  /** The slider's leftmost value: the minimum, or 1 for a log scale's minimum of 0. */
  low: number;
  max: number;
  position: (size: number) => number;
  value: (position: number) => number;
}

/** Slider resolution: positions between the low end and the maximum. */
const SLIDER_STEPS = 1000;

// Half the native range thumb's width: the track's travel starts and ends this far in.
const SLIDER_THUMB_HALF = 8;

// Byte chips are powers of two GiB, and drags snap to a quarter GiB where the unit can hold it.
const GIB = 1024 ** 3;
const QUARTER_GIB = GIB / 4;

/**
 * The slider's scale over a range: log for tokens, so small and large contexts both get room, and
 * linear for byte sizes. Both ends map exactly; between them, tokens snap to multiples of 1024 from
 * 1024 up and byte sizes to a quarter GiB, so drags land on round sizes.
 */
function sliderScale(unit: SizeUnit, min: number, max: number): SliderScale {
  if (unit === "tokens") {
    const low = Math.min(Math.max(min, 1), max);
    return {
      low,
      max,
      position: (size) => {
        const clamped = Math.min(Math.max(size, low), max);
        return Math.round((Math.log(clamped / low) / Math.log(max / low)) * SLIDER_STEPS);
      },
      value: (position) => {
        if (position >= SLIDER_STEPS) return max;

        const raw = low * (max / low) ** (position / SLIDER_STEPS);
        const snapped = raw >= 1024 ? Math.round(raw / 1024) * 1024 : Math.round(raw);
        return Math.min(Math.max(snapped, min), max);
      },
    };
  }

  const step = Math.max(1, Math.floor(QUARTER_GIB / bytesPerUnit(unit)));
  return {
    low: min,
    max,
    position: (size) => {
      const clamped = Math.min(Math.max(size, min), max);
      return Math.round(((clamped - min) / (max - min)) * SLIDER_STEPS);
    },
    value: (position) => {
      if (position <= 0) return min;
      if (position >= SLIDER_STEPS) return max;

      const snapped = Math.round((min + ((max - min) * position) / SLIDER_STEPS) / step) * step;
      return Math.min(Math.max(snapped, min), max);
    },
  };
}

// Token chips are the common context sizes; byte chips are powers of two GiB, both within range.
function sizeChips(unit: SizeUnit, { maximum, minimum }: SizeRange): number[] {
  const sizes =
    unit === "tokens"
      ? contextSizePresets
      : powersOfTwo(GIB, maximum * bytesPerUnit(unit)).map((bytes) => bytes / bytesPerUnit(unit));
  return sizes.filter((size) => Number.isInteger(size) && size >= minimum && size <= maximum);
}

function powersOfTwo(from: number, upTo: number): number[] {
  const powers: number[] = [];
  for (let power = from; power <= upTo; power *= 2) powers.push(power);
  return powers;
}

function exactSize(size: number, unit: SizeUnit): string {
  return unit === "tokens" ? formatExactTokens(size) : formatExactSize(size, unit);
}

function spokenSize(size: number, unit: SizeUnit): string {
  return unit === "tokens" ? `${formatQuantity(size, "")} tokens` : formatExactSize(size, unit);
}
