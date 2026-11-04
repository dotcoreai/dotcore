import type { ComponentProps } from "react";

import { cn } from "@/lib/utils";

type FieldErrorProps = ComponentProps<"p">;

/**
 * Renders one local validation message immediately beside its owning form field.
 * Callers give it a stable ID and reference that ID from the invalid field's `aria-describedby`.
 */
function FieldError({ className, ...props }: FieldErrorProps) {
  return <p className={cn("text-sm text-destructive", className)} role="alert" {...props} />;
}

export { FieldError };
export type { FieldErrorProps };
