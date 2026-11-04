import { type KeyboardEvent, useEffect, useId, useRef, useState } from "react";

import {
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
} from "@/components/ui/dropdown-menu";
import { FieldError } from "@/components/ui/field-error";
import { useDirtyFields } from "@/hooks/use-dirty-fields";

import { ContextSizeInput } from "./context-size-input";
import type { MemoryBasis } from "./profile-compatibility";
import {
  followContextInputs,
  minimumScenarioContextTokens,
  sameSizingScenario,
  scenarioFromInputs,
  type ScenarioInput,
  scenarioInputErrors,
  scenarioInputs,
  type SizingScenario,
  sizingScenarioKey,
} from "./sizing-scenario";

/**
 * The Runs on chip's "Sized for" menu section: defaults, or a custom sizing scenario with the shared
 * context size input for the total context and the optional context per request. A custom scenario
 * applies once its inputs hold valid values and typing pauses, and at once when an input loses
 * focus; defaults apply at once.
 */
export function CatalogSizedFor({
  onChange,
  scenario,
}: {
  onChange: (scenario: SizingScenario | undefined) => void;
  scenario: SizingScenario | undefined;
}) {
  const [custom, setCustom] = useState(Boolean(scenario));
  const [inputs, setInputs] = useState(() => scenarioInputs(scenario ?? initialCustomScenario));
  const [errors, setErrors] = useState<Partial<Record<ScenarioInput, string>>>({});
  // The input the operator last edited, which the other follows once the edit settles.
  const [edited, setEdited] = useState<ScenarioInput>("total");
  const [shownScenario, setShownScenario] = useState(scenario);
  const dirtyFields = useDirtyFields<ScenarioInput>(true);
  const id = useId();
  const followed = followContextInputs(inputs, edited);
  const typed = scenarioFromInputs(followed);

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
  const followedRef = useRef(followed);
  useEffect(() => {
    onChangeRef.current = onChange;
    pendingRef.current = pending;
    followedRef.current = followed;
  });

  const pendingKey = sizingScenarioKey(pending);
  useEffect(() => {
    if (!pendingKey) return;
    const timer = setTimeout(() => {
      setInputs(followedRef.current);
      if (pendingRef.current) onChangeRef.current(pendingRef.current);
    }, scenarioInputDelayMs);
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
  // A settled value, from the slider or a preset, moves the other input at once; typing moves it
  // once it pauses or the input is left.
  const change = (input: ScenarioInput, value: string, settled: boolean) => {
    dirtyFields.markDirty(input);
    const next = { ...inputs, [input]: value };
    setInputs(settled ? followContextInputs(next, input) : next);
    setEdited(input);
    // A shown error follows the inputs, since the context per request is bounded by the total.
    if (errors.total || errors.perRequest)
      setErrors(shownErrors(followContextInputs(next, input), errors));
  };
  // Leaving an input applies its value before the menu can close or the chip go, so no later write
  // lands after the menu unmounts.
  const blur = (input: ScenarioInput) => {
    if (pending) onChange(pending);
    setInputs(followed);
    if (dirtyFields.isDirty(input))
      setErrors((current) => ({ ...current, [input]: scenarioInputErrors(followed)[input] }));
  };
  const totalId = `${id}-total`;
  const perRequestId = `${id}-per-request`;
  const total = /^\d+$/.test(inputs.total.trim()) ? Number(inputs.total.trim()) : undefined;

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
          <label className="text-xs font-medium text-muted-foreground" htmlFor={totalId}>
            Total context
          </label>
          <ContextSizeInput
            describedBy={errors.total ? `${totalId}-error` : undefined}
            id={totalId}
            invalid={Boolean(errors.total)}
            label="Total context"
            maximum={sliderMaximum}
            minimum={minimumScenarioContextTokens}
            onBlur={() => blur("total")}
            onChange={(value, settled) => change("total", value, settled)}
            onKeyDown={keepKeys}
            unit="tokens"
            value={inputs.total}
          />
          {errors.total ? <FieldError id={`${totalId}-error`}>{errors.total}</FieldError> : null}
          <label className="text-xs font-medium text-muted-foreground" htmlFor={perRequestId}>
            Context per request
          </label>
          <ContextSizeInput
            describedBy={errors.perRequest ? `${perRequestId}-error` : undefined}
            emptyValue={total}
            id={perRequestId}
            invalid={Boolean(errors.perRequest)}
            label="Context per request"
            maximum={sliderMaximum}
            minimum={minimumScenarioContextTokens}
            onBlur={() => blur("perRequest")}
            onChange={(value, settled) => change("perRequest", value, settled)}
            onKeyDown={keepKeys}
            unit="tokens"
            value={inputs.perRequest}
          />
          {errors.perRequest ? (
            <FieldError id={`${perRequestId}-error`}>{errors.perRequest}</FieldError>
          ) : null}
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
const initialCustomScenario: SizingScenario = { totalContextTokens: 131072 };

// The slider's right end; the number input accepts larger contexts.
const sliderMaximum = 1_048_576;

// How long typing pauses before a scenario applies, so a read doesn't follow every keystroke.
const scenarioInputDelayMs = 500;

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
    ...(shown.total ? { total: errors.total } : {}),
    ...(shown.perRequest ? { perRequest: errors.perRequest } : {}),
  };
}
