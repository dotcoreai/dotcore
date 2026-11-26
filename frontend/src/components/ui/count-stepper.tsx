import { MinusIcon, PlusIcon } from "lucide-react";
import type { KeyboardEvent, ReactNode, Ref } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

interface CountStepperProps {
  "aria-describedby"?: string;
  /** Names the number box when no label element does. */
  "aria-label"?: string;
  "aria-invalid"?: "true";
  className?: string;
  disabled?: boolean;
  id: string;
  inputRef?: Ref<HTMLInputElement>;
  /** Names the − and + buttons, such as "Parallel requests". */
  label: string;
  maximum?: number;
  minimum: number;
  onBlur?: () => void;
  /** `settled` marks a discrete change, from a button or an arrow key, as opposed to typing. */
  onChange: (value: string, change?: { settled: boolean }) => void;
  onKeyDown?: (event: KeyboardEvent<HTMLInputElement>) => void;
  placeholder?: string;
  /** The typed text, which may be empty or not a number. */
  value: string;
}

/**
 * A small whole-number control: a narrow typeable box between − and + buttons. The buttons and
 * the arrow keys step by one and report a settled change; a step lands within the bounds, and an
 * empty or invalid text steps up to the minimum. Typed text is reported as typed.
 */
export function CountStepper({
  className,
  disabled,
  inputRef,
  label,
  maximum,
  minimum,
  onBlur,
  onChange,
  onKeyDown,
  value,
  ...inputProps
}: CountStepperProps) {
  const trimmed = value.trim();
  const current = /^\d+$/.test(trimmed) ? Number(trimmed) : undefined;
  const canDecrease = current !== undefined && current > minimum;
  const canIncrease = current === undefined || maximum === undefined || current < maximum;
  const step = (next: number) => {
    const bounded = Math.max(minimum, maximum === undefined ? next : Math.min(maximum, next));
    onChange(String(bounded), { settled: true });
  };
  const increase = () => step(current === undefined ? minimum : current + 1);
  const decrease = () => step((current ?? minimum) - 1);

  return (
    <div
      className={cn(
        "inline-flex h-control-md w-fit items-stretch rounded-lg border border-input bg-card has-[input[aria-invalid=true]]:border-destructive dark:bg-input/30",
        className,
      )}
      data-slot="count-stepper"
    >
      <StepButton
        disabled={disabled || !canDecrease}
        label={`Decrease ${label}`}
        onClick={decrease}
        position="start"
      >
        <MinusIcon aria-hidden="true" />
      </StepButton>
      <Input
        {...inputProps}
        autoComplete="off"
        className="h-full w-12 rounded-none border-0 bg-transparent px-1 text-center focus-visible:z-10 focus-visible:ring-inset dark:bg-transparent"
        disabled={disabled}
        inputMode="numeric"
        onBlur={onBlur}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          onKeyDown?.(event);
          if (event.defaultPrevented || disabled) return;
          if (event.key === "ArrowUp" && canIncrease) {
            event.preventDefault();
            increase();
          } else if (event.key === "ArrowDown" && canDecrease) {
            event.preventDefault();
            decrease();
          }
        }}
        ref={inputRef}
        spellCheck={false}
        value={value}
      />
      <StepButton
        disabled={disabled || !canIncrease}
        label={`Increase ${label}`}
        onClick={increase}
        position="end"
      >
        <PlusIcon aria-hidden="true" />
      </StepButton>
    </div>
  );
}

function StepButton({
  children,
  disabled,
  label,
  onClick,
  position,
}: {
  children: ReactNode;
  disabled: boolean;
  label: string;
  onClick: () => void;
  position: "end" | "start";
}) {
  return (
    <Button
      aria-label={label}
      onKeyDown={(event) => {
        // The arrows belong to the button's own step, not to an enclosing menu's highlight.
        if (event.key === "ArrowUp" || event.key === "ArrowDown") event.stopPropagation();
      }}
      className={cn(
        "h-full w-control-md rounded-none text-muted-foreground focus-visible:z-10 focus-visible:ring-inset",
        position === "start" ? "rounded-l-[7px] border-r-input" : "rounded-r-[7px] border-l-input",
      )}
      disabled={disabled}
      onClick={onClick}
      type="button"
      variant="ghost"
    >
      {children}
    </Button>
  );
}
