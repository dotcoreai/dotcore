import type { ReactNode } from "react";

import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/** A cell value with an optional muted sub-label beneath it, such as "128k" over "default". */
export function Metric({ label, value }: { label?: string; value: ReactNode }) {
  return (
    <div>
      <span className="font-medium tabular-nums">{value}</span>
      {label ? <span className="block text-[11.5px] text-muted-foreground">{label}</span> : null}
    </div>
  );
}

/** A cell listing values as badges, or a dash without any. Mono values read as identifiers. */
export function Tags({ mono = false, values }: { mono?: boolean; values: readonly string[] }) {
  if (!values.length) return <span className="text-muted-foreground">—</span>;

  return (
    <div className="flex flex-wrap gap-1">
      {values.map((value) => (
        <Badge
          className={cn("rounded-md font-normal text-foreground", mono && "font-mono")}
          key={value}
          variant="neutral"
        >
          {value}
        </Badge>
      ))}
    </div>
  );
}

/**
 * A pulsing placeholder for a value still loading, so late results don't read as final. The label
 * names the pending value for screen readers.
 */
export function PendingValue({ label }: { label: string }) {
  return (
    <>
      <span
        aria-hidden="true"
        className="inline-block h-3.5 w-16 rounded-md bg-muted align-middle motion-safe:animate-pulse"
      />
      <span className="sr-only">{label}</span>
    </>
  );
}
