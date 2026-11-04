import { Badge } from "@/components/ui/badge";
import type { LifecycleTone, StatusProjection } from "@/lib/status-projection";
import { cn } from "@/lib/utils";

export interface StatusBadgeProps {
  status: StatusProjection;
}

export interface StatusDotProps {
  className?: string;
  hollow?: boolean;
  status: StatusProjection;
  tooltip?: boolean;
}

export interface StatusTextProps {
  className?: string;
  status: StatusProjection;
}

/**
 * The one way the console renders a resource status. It takes a projection and nothing else, so a
 * surface cannot pair a label with a colour of its own choosing: the tone always arrives already
 * decided by that state's tone function, and one state therefore reads the same colour everywhere.
 *
 * Cards render an aggregate status and inspectors render the underlying states as separate rows, so
 * the two surfaces show different words by design. This component is what keeps them from showing
 * different colours for the same state.
 *
 * A badge that names a property rather than a state — an upstream model's `External`, a profile's
 * `Default` — is not a status and stays a plain `Badge`.
 */
export function StatusBadge({ status }: StatusBadgeProps) {
  return (
    <Badge title={status.attention} variant={status.tone}>
      {status.label}
    </Badge>
  );
}

/**
 * A status reduced to a small dot in its tone's ink, for places too tight for a badge such as a
 * selected chip. A hollow dot draws only the ring, for a state that is not in service. The dot is
 * decorative: the caller carries the status label in text or in an accessible name, and drops the
 * dot's tooltip when it already reveals that label itself.
 */
export function StatusDot({ className, hollow = false, status, tooltip = true }: StatusDotProps) {
  return (
    <span
      aria-hidden="true"
      className={cn(
        "inline-block size-2 shrink-0 rounded-full",
        hollow ? HOLLOW_DOT_TONES[status.tone] : DOT_TONES[status.tone],
        className,
      )}
      data-slot="status-dot"
      data-tone={status.tone}
      title={tooltip ? status.label : undefined}
    />
  );
}

/**
 * A status label in the same tone's ink as its `StatusDot`, for a label shown beside the dot. The
 * tone arrives with the projection, so the label and its dot cannot disagree on colour.
 */
export function StatusText({ className, status }: StatusTextProps) {
  return (
    <span
      className={cn("text-xs font-normal", TEXT_TONES[status.tone], className)}
      data-slot="status-text"
      data-tone={status.tone}
    >
      {status.label}
    </span>
  );
}

const DOT_TONES: Record<LifecycleTone, string> = {
  destructive: "bg-destructive",
  info: "bg-primary",
  neutral: "bg-muted-foreground",
  success: "bg-success-foreground",
  warning: "bg-warning-foreground",
};

const HOLLOW_DOT_TONES: Record<LifecycleTone, string> = {
  destructive: "border-[1.5px] border-destructive",
  info: "border-[1.5px] border-primary",
  neutral: "border-[1.5px] border-muted-foreground",
  success: "border-[1.5px] border-success-foreground",
  warning: "border-[1.5px] border-warning-foreground",
};

const TEXT_TONES: Record<LifecycleTone, string> = {
  destructive: "text-destructive",
  info: "text-primary",
  neutral: "text-muted-foreground",
  success: "text-success-foreground",
  warning: "text-warning-foreground",
};
