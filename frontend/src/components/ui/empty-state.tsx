import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

const ghostCardCount = 4;

/**
 * The placeholder a console collection shows when it has nothing to list: faded outlines of the
 * cards that will appear, with an icon, a title, one line of help and the action that creates the
 * first item layered in front.
 */
function EmptyState({
  action,
  className,
  description,
  icon: Icon,
  title,
}: {
  /** The call to action, normally an `AddAction`. */
  action: ReactNode;
  className?: string;
  description: string;
  /** Decorative icon, hidden from assistive technology. */
  icon: LucideIcon;
  title: string;
}) {
  return (
    <div
      data-slot="empty-state"
      className={cn("overflow-hidden rounded-lg border bg-card text-center", className)}
      role="status"
    >
      {/* Same column template as the real card grid, so the outlines match the cards they stand in
          for. Fixed height clips the grid to its first row at every width. */}
      <div
        aria-hidden="true"
        className="inspector-card-grid pointer-events-none h-28 overflow-hidden p-3 opacity-60 mask-[linear-gradient(to_bottom,black,transparent_85%)]"
      >
        {Array.from({ length: ghostCardCount }, (_, index) => (
          <GhostCard key={index} />
        ))}
      </div>
      <div className="-mt-14 flex flex-col items-center gap-1 px-6 pb-8">
        <div className="mb-2 grid size-11 place-items-center rounded-xl border bg-accent text-primary">
          <Icon aria-hidden="true" className="size-5" />
        </div>
        <h3 className="m-0 text-base font-semibold text-foreground">{title}</h3>
        <p className="m-0 mb-3 max-w-md text-sm text-muted-foreground">{description}</p>
        {action}
      </div>
    </div>
  );
}

function GhostCard() {
  return (
    <div className="flex h-22 flex-col gap-2 rounded-lg bg-muted p-3">
      <span className="h-2 w-[55%] rounded-full bg-foreground/15" />
      <span className="h-2 w-[80%] rounded-full bg-foreground/15" />
      <span className="h-2 w-[35%] rounded-full bg-foreground/15" />
    </div>
  );
}

export { EmptyState };
