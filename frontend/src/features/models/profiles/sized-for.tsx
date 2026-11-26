import { type KeyboardEvent, useEffect, useId, useRef, useState } from "react";

import {
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from "@/components/ui/dropdown-menu";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import type { ContextSliderEnds } from "@/features/models/runtime/context-slider-ends";
import { SizeInput } from "@/features/models/runtime/size-input";
import { useDirtyFields } from "@/hooks/use-dirty-fields";
import { timing } from "@/lib/timing";

import type { MemoryBasis } from "./profile-compatibility";
import {
  sameSizingScenario,
  scenarioFromInputs,
  type ScenarioInput,
  scenarioInputErrors,
  scenarioInputs,
  type SizingScenario,
  sizingScenarioKey,
} from "./sizing-scenario";

/**
 * The Runs on chip's "Sized for" menu section: defaults, or a custom sizing scenario with the
 * context per request, the optional parallel requests that each profile's own replace when empty,
 * and the optional total context that follows the other two when empty. The size sliders span the
 * given ends, which cover every profile the scenario sizes. A custom scenario applies once its
 * inputs hold valid values and typing pauses, and at once when an input loses focus; defaults
 * apply at once.
 */
export function CatalogSizedFor({
  ends,
  onChange,
  scenario,
}: {
  ends: ContextSliderEnds;
  onChange: (scenario: SizingScenario | undefined) => void;
  scenario: SizingScenario | undefined;
}) {
  const [custom, setCustom] = useState(Boolean(scenario));
  const [inputs, setInputs] = useState(() => scenarioInputs(scenario ?? initialCustomScenario));
  const [errors, setErrors] = useState<Partial<Record<ScenarioInput, string>>>({});
  const [shownScenario, setShownScenario] = useState(scenario);
  const dirtyFields = useDirtyFields<ScenarioInput>(true);
  const id = useId();
  const typed = scenarioFromInputs(inputs);

  // A scenario applied elsewhere, such as from browser history, replaces the choice and inputs.
  if (!sameSizingScenario(shownScenario, scenario)) {
    setShownScenario(scenario);
    setCustom(Boolean(scenario));
    if (scenario && !sameSizingScenario(typed, scenario)) {
      setInputs(scenarioInputs(scenario));
      setErrors({});
    }
  }

  // The typed scenario still to apply, which leaving an input applies at once.
  const pending = custom && typed && !sameSizingScenario(typed, scenario) ? typed : undefined;
  const onChangeRef = useRef(onChange);
  const pendingRef = useRef(pending);
  useEffect(() => {
    onChangeRef.current = onChange;
    pendingRef.current = pending;
  });

  const pendingKey = sizingScenarioKey(pending);
  useEffect(() => {
    if (!pendingKey) return;
    const timer = setTimeout(() => {
      if (pendingRef.current) onChangeRef.current(pendingRef.current);
    }, timing().scenarioInputMs);
    return () => clearTimeout(timer);
  }, [pendingKey]);

  const choose = (choice: string) => {
    const nextCustom = choice === customChoice;
    setCustom(nextCustom);
    if (!nextCustom) {
      onChange(undefined);
    } else if (typed) {
      onChange(typed);
    }
  };
  const change = (input: ScenarioInput, value: string) => {
    dirtyFields.markDirty(input);
    const next = { ...inputs, [input]: value };
    setInputs(next);
    // A shown error follows the inputs, since the context per request is bounded by the total.
    if (errors.parallel || errors.perRequest || errors.total) setErrors(shownErrors(next, errors));
  };
  // Leaving an input applies its value before the menu can close or the chip go, so no later write
  // lands after the menu unmounts.
  const blur = (input: ScenarioInput) => {
    if (pending) onChange(pending);
    if (dirtyFields.isDirty(input))
      setErrors((current) => ({ ...current, [input]: scenarioInputErrors(inputs)[input] }));
  };
  const fieldId = (input: ScenarioInput) => `${id}-${input}`;
  const errorId = (input: ScenarioInput) => (errors[input] ? `${fieldId(input)}-error` : undefined);
  const error = (input: ScenarioInput) =>
    errors[input] ? <FieldError id={`${fieldId(input)}-error`}>{errors[input]}</FieldError> : null;
  const label = (input: ScenarioInput, text: string) => (
    <label className="text-xs font-medium text-muted-foreground" htmlFor={fieldId(input)}>
      {text}
    </label>
  );
  const whole = (input: string) => (/^\d+$/.test(input.trim()) ? Number(input.trim()) : undefined);
  const perRequest = whole(inputs.perRequest);
  const parallel = whole(inputs.parallel);
  // Both sliders share one scale, so the total context lines up under the context per request.
  const sliderRange = { maximum: Math.max(ends.perRequest, ends.total), minimum: ends.minimum };

  return (
    <DropdownMenuGroup>
      <DropdownMenuLabel>Sized for</DropdownMenuLabel>
      <DropdownMenuRadioGroup
        onValueChange={(choice: string) => choose(choice)}
        value={custom ? customChoice : defaultsChoice}
      >
        <DropdownMenuRadioItem value={defaultsChoice}>Defaults</DropdownMenuRadioItem>
        <DropdownMenuRadioItem value={customChoice}>Custom</DropdownMenuRadioItem>
      </DropdownMenuRadioGroup>
      {custom ? (
        <div className="mx-1.5 grid gap-2 border-l border-border pt-1 pb-2 pl-3">
          {label("perRequest", "Context per request")}
          <SizeInput
            describedBy={errorId("perRequest")}
            id={fieldId("perRequest")}
            invalid={Boolean(errors.perRequest)}
            label="Context per request"
            range={{ maximum: ends.perRequest, minimum: ends.minimum }}
            onBlur={() => blur("perRequest")}
            onChange={(value) => change("perRequest", value)}
            onKeyDown={keepKeys}
            sliderRange={sliderRange}
            unit="tokens"
            value={inputs.perRequest}
          />
          {error("perRequest")}
          {label("parallel", "Parallel requests")}
          <Input
            aria-describedby={errorId("parallel")}
            aria-invalid={Boolean(errors.parallel) || undefined}
            className="w-28"
            id={fieldId("parallel")}
            inputMode="numeric"
            onBlur={() => blur("parallel")}
            onChange={(event) => change("parallel", event.target.value)}
            onKeyDown={keepKeys}
            placeholder="Profile default"
            value={inputs.parallel}
          />
          {error("parallel")}
          {label("total", "Total context")}
          <SizeInput
            describedBy={errorId("total")}
            emptyValue={
              perRequest !== undefined && parallel !== undefined ? perRequest * parallel : undefined
            }
            id={fieldId("total")}
            invalid={Boolean(errors.total)}
            label="Total context"
            range={{ maximum: ends.total, minimum: ends.minimum }}
            onBlur={() => blur("total")}
            onChange={(value) => change("total", value)}
            onKeyDown={keepKeys}
            sliderRange={sliderRange}
            unit="tokens"
            value={inputs.total}
          />
          {error("total")}
        </div>
      ) : null}
    </DropdownMenuGroup>
  );
}

/**
 * The Runs on chip's memory basis menu section: whether profiles are judged against the memory free
 * now beside the running models, or against the machine's total memory.
 */
export function CatalogMemoryBasis({
  basis,
  onChange,
}: {
  basis: MemoryBasis;
  onChange: (basis: MemoryBasis) => void;
}) {
  return (
    <DropdownMenuGroup>
      <DropdownMenuLabel>Memory</DropdownMenuLabel>
      <DropdownMenuRadioGroup
        onValueChange={(choice: string) => onChange(choice === "total" ? "total" : "free")}
        value={basis}
      >
        <DropdownMenuRadioItem value="free">Free now</DropdownMenuRadioItem>
        <DropdownMenuRadioItem value="total">Total</DropdownMenuRadioItem>
      </DropdownMenuRadioGroup>
    </DropdownMenuGroup>
  );
}

const defaultsChoice = "defaults";
const customChoice = "custom";

// The scenario Custom starts from when none is set.
const initialCustomScenario: SizingScenario = { contextTokensPerRequest: 32768 };

// The inputs take their own keys, so typing and arrow keys don't move the menu's highlight; Escape
// still closes the menu.
function keepKeys(event: KeyboardEvent) {
  if (event.key !== "Escape") event.stopPropagation();
}

// The errors of the inputs that show one, re-validated against the inputs.
function shownErrors(
  inputs: Record<ScenarioInput, string>,
  shown: Partial<Record<ScenarioInput, string>>,
): Partial<Record<ScenarioInput, string>> {
  const errors = scenarioInputErrors(inputs);
  return {
    ...(shown.parallel ? { parallel: errors.parallel } : {}),
    ...(shown.perRequest ? { perRequest: errors.perRequest } : {}),
    ...(shown.total ? { total: errors.total } : {}),
  };
}
