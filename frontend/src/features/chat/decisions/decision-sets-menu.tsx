import { Bookmark, Trash2 } from "lucide-react";
import { useId, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import { useChatDecisions } from "@/features/chat/chat-session-context";

/**
 * The named question sets saved in this browser: load or delete one, or save the draft under a
 * name. The list is read again each time the menu opens, so sets saved from another tab show up.
 * Saving needs a draft that builds a request, and says why otherwise.
 */
export function DecisionSetsMenu() {
  const { deleteSet, loadSet, problem, refreshSets, saveSet, sets } = useChatDecisions();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const reasonId = useId();
  const trimmed = name.trim();

  return (
    <Popover
      onOpenChange={(next) => {
        if (next) {
          refreshSets();
          setName("");
        }
        setOpen(next);
      }}
      open={open}
    >
      <PopoverTrigger render={<Button type="button" variant="outline" />}>
        <Bookmark data-icon="inline-start" />
        Saved sets
        <Badge variant="outline">{sets.length}</Badge>
      </PopoverTrigger>
      <PopoverContent align="start" aria-label="Saved sets" className="gap-1.5 p-1.5">
        {sets.length === 0 ? (
          <p className="px-1.5 py-1 text-xs text-muted-foreground">
            No saved sets in this browser.
          </p>
        ) : (
          <ul className="grid gap-0.5">
            {sets.map((set) => (
              <li className="flex items-center gap-1" key={set.name}>
                <Button
                  className="min-w-0 flex-1 justify-start"
                  onClick={() => {
                    loadSet(set.body);
                    setOpen(false);
                  }}
                  size="sm"
                  type="button"
                  variant="ghost"
                >
                  <span className="truncate">{set.name}</span>
                </Button>
                <Button
                  aria-label={`Delete ${set.name}`}
                  onClick={() => deleteSet(set.name)}
                  size="icon-sm"
                  type="button"
                  variant="ghost"
                >
                  <Trash2 />
                </Button>
              </li>
            ))}
          </ul>
        )}
        <Separator />
        <form
          className="flex items-center gap-1.5"
          onSubmit={(event) => {
            event.preventDefault();
            if (!trimmed || problem) return;
            saveSet(trimmed);
            setName("");
          }}
        >
          <Input
            aria-label="Set name"
            onChange={(event) => setName(event.target.value)}
            placeholder="Name this set"
            value={name}
          />
          <Button
            aria-describedby={problem ? reasonId : undefined}
            disabled={!trimmed || problem !== undefined}
            type="submit"
            variant="outline"
          >
            Save
          </Button>
        </form>
        {problem ? (
          <p className="px-1.5 text-xs text-muted-foreground" id={reasonId}>
            {problem}
          </p>
        ) : null}
      </PopoverContent>
    </Popover>
  );
}
