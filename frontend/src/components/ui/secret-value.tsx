import { EyeIcon, EyeOffIcon } from "lucide-react";
import { type ReactNode, useState } from "react";

import { Button } from "@/components/ui/button";
import { IconCopyControl } from "@/components/ui/copy-control";

interface SecretValueProps {
  /** What the secret is, used in the button names ("Reveal <label>"). */
  label: string;
  /** Muted tags shown after the value, such as `generated`. */
  tags?: ReactNode;
  value: string;
}

/**
 * Shows a secret as a fixed mask until the operator reveals it, with a copy button that copies
 * without revealing. The value is kept out of the DOM and the accessible names while masked.
 */
export function SecretValue({ label, tags, value }: SecretValueProps) {
  const [revealed, setRevealed] = useState(false);

  return (
    <span className="inline-flex flex-wrap items-center justify-end gap-x-1.5 gap-y-1">
      {tags}
      <span className="inline-flex min-w-0 items-center gap-1">
        {revealed ? (
          <span className="min-w-0 font-mono break-all select-all">{value}</span>
        ) : (
          <span className="font-mono">••••••••</span>
        )}
        <Button
          aria-label={`${revealed ? "Hide" : "Reveal"} ${label}`}
          aria-pressed={revealed}
          className="shrink-0 cursor-pointer"
          onClick={() => setRevealed((current) => !current)}
          size="icon-xs"
          title={`${revealed ? "Hide" : "Reveal"} ${label}`}
          type="button"
          variant="ghost"
        >
          {revealed ? <EyeOffIcon aria-hidden="true" /> : <EyeIcon aria-hidden="true" />}
        </Button>
        <IconCopyControl label={label} value={value} />
      </span>
    </span>
  );
}

/** A muted tag marking how a value came to be, such as `default` or `generated`. */
export function ValueTag({ children }: { children: ReactNode }) {
  return (
    <span className="rounded-full bg-muted px-2 py-0.5 text-[11px] font-medium text-muted-foreground">
      {children}
    </span>
  );
}
