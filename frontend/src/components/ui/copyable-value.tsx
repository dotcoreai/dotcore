import { InlineCopyControl } from "@/components/ui/copy-control";
import { cn } from "@/lib/utils";

const defaultHead = 7;
const defaultTail = 6;

interface CopyableValueProps {
  className?: string;
  /** Characters kept from the start of `value`. Defaults to 7, the git short-SHA length. Negative values clamp to zero. */
  head?: number;
  /** Characters kept from the end of `value`, guarding against a shared prefix. Defaults to 6. Negative values clamp to zero. */
  tail?: number;
  value: string;
}

/**
 * Renders a long opaque identifier middle-truncated while copying its full value through the shared
 * inline copy control. The full value remains available through the title and accessible copy name.
 */
function CopyableValue({
  className,
  head = defaultHead,
  tail = defaultTail,
  value,
}: CopyableValueProps) {
  const safeHead = Math.max(0, head);
  const safeTail = Math.max(0, tail);
  const isTruncated = value.length > safeHead + safeTail + 1;
  const displayedValue = isTruncated
    ? `${value.slice(0, safeHead)}…${value.slice(value.length - safeTail)}`
    : value;

  return (
    <InlineCopyControl
      className={cn("justify-end text-[12.5px] font-medium", className)}
      label={value}
      value={value}
    >
      <span title={value}>{displayedValue}</span>
    </InlineCopyControl>
  );
}

export { CopyableValue };
export type { CopyableValueProps };
