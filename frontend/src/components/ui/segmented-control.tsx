import { Radio } from "@base-ui/react/radio";
import { RadioGroup } from "@base-ui/react/radio-group";

import { cn } from "@/lib/utils";

/** One choice of a segmented control: the value it selects and its visible text. */
interface SegmentedControlOption<Value extends string> {
  label: string;
  value: Value;
}

/**
 * A compact single choice among a few short options, drawn as joined segments. It is a radio
 * group: arrow keys move the selection, and the selected option is announced as checked. A
 * disabled control keeps its selection and ignores input.
 */
function SegmentedControl<Value extends string>({
  "aria-label": ariaLabel,
  className,
  disabled,
  onValueChange,
  options,
  value,
}: {
  "aria-label": string;
  className?: string;
  disabled?: boolean;
  onValueChange: (value: Value) => void;
  options: readonly SegmentedControlOption<Value>[];
  value: Value | undefined;
}) {
  return (
    <RadioGroup
      aria-label={ariaLabel}
      className={cn(
        "inline-flex items-center gap-0.5 rounded-md border border-input bg-card p-0.5",
        className,
      )}
      data-slot="segmented-control"
      disabled={disabled}
      onValueChange={(next) => onValueChange(next as Value)}
      value={value ?? null}
    >
      {options.map((option) => (
        <Radio.Root
          className="inline-flex h-control-xs cursor-pointer items-center rounded-sm px-2 text-xs text-muted-foreground outline-none select-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50 data-checked:bg-accent data-checked:font-medium data-checked:text-accent-foreground data-disabled:cursor-not-allowed data-disabled:opacity-50"
          key={option.value}
          value={option.value}
        >
          {option.label}
        </Radio.Root>
      ))}
    </RadioGroup>
  );
}

export { SegmentedControl };
export type { SegmentedControlOption };
