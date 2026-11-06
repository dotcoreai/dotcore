import type { LucideIcon } from "lucide-react";
import type * as React from "react";

import { cn } from "@/lib/utils";

/**
 * Top row of a page: its heading and optional page actions. It renders a `div` because the page
 * sits inside the console `main` landmark, where a `header` element adds no semantics.
 */
function PageHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="page-header"
      className={cn(
        // The minimum height shares the `console` button's density token, so headers with and
        // without page actions have the same height and page content starts at the same position.
        // Page actions share the heading's row whenever they fit, which keeps that height equal;
        // only actions too wide for the row wrap below the heading, instead of overflowing the page.
        "flex min-h-control-action flex-wrap items-center justify-between gap-x-4 gap-y-3",
        className,
      )}
      {...props}
    />
  );
}

function PageHeaderHeading({
  className,
  children,
  icon: Icon,
  ...props
}: React.ComponentProps<"h1"> & {
  /** Decorative icon rendered before the heading text, hidden from assistive technology. */
  icon?: LucideIcon;
}) {
  return (
    <h1
      data-slot="page-header-heading"
      className={cn(
        "flex items-center gap-2 font-heading text-2xl leading-tight font-semibold tracking-tight",
        className,
      )}
      {...props}
    >
      {Icon ? <Icon aria-hidden="true" className="size-5 shrink-0 text-muted-foreground" /> : null}
      {children}
    </h1>
  );
}

function PageHeaderDescription({ className, ...props }: React.ComponentProps<"p">) {
  return (
    <p
      data-slot="page-header-description"
      className={cn("text-sm text-muted-foreground", className)}
      {...props}
    />
  );
}

function PageActions({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="page-actions"
      className={cn("flex flex-wrap items-center gap-2", className)}
      {...props}
    />
  );
}

/**
 * Title of a section within a page, one level below the page heading. Inspector sections keep
 * their own smaller headings.
 */
function PageSectionHeading({ className, children, ...props }: React.ComponentProps<"h2">) {
  return (
    <h2
      data-slot="page-section-heading"
      className={cn("text-[17px] leading-6 font-semibold", className)}
      {...props}
    >
      {children}
    </h2>
  );
}

export { PageActions, PageHeader, PageHeaderDescription, PageHeaderHeading, PageSectionHeading };
