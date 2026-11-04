import { Button } from "@/components/ui/button";
import { MemoryView } from "@/gen/dotcore/console/v1/preferences_pb";
import { cn } from "@/lib/utils";

/**
 * The two-way "All machines / By machine" choice for rows that merge every machine or keep each
 * machine's share. Pages that offer it share one preference (`useMemoryView`), so the choice
 * follows the viewer between them. `label` names the group for assistive technology.
 */
export function MemoryViewSwitch({
  label,
  onChange,
  view,
}: {
  label: string;
  onChange: (view: MemoryView) => void;
  view: MemoryView;
}) {
  return (
    <div
      aria-label={label}
      className="inline-flex rounded-lg border border-border p-0.5"
      role="group"
    >
      {viewOptions.map((option) => (
        <Button
          aria-pressed={view === option.view}
          className={cn(
            "text-muted-foreground",
            "aria-pressed:bg-foreground/10 aria-pressed:text-foreground",
          )}
          key={option.view}
          onClick={() => onChange(option.view)}
          size="xs"
          type="button"
          variant="ghost"
        >
          {option.label}
        </Button>
      ))}
    </div>
  );
}

/**
 * The place of a `MemoryViewSwitch` that is not offered, such as while the machines are counted or
 * on a single machine, so the heading row keeps the switch's height either way.
 */
export function MemoryViewSwitchPlace() {
  return (
    <div aria-hidden="true" className="box-content h-control-xs border border-transparent p-0.5" />
  );
}

const viewOptions = [
  { label: "All machines", view: MemoryView.ALL_MACHINES },
  { label: "By machine", view: MemoryView.BY_MACHINE },
] as const;
