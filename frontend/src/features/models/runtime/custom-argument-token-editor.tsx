import { ChevronDownIcon, ChevronUpIcon, Trash2Icon } from "lucide-react";
import { useEffect, useRef } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

/** Edits the exact, ordered custom engine argument tokens, keeping focus on the edited row. */
export function CustomArgumentTokenEditor({
  disabled,
  onAdd,
  onMove,
  onRemove,
  onUpdate,
  tokens,
}: {
  disabled: boolean;
  onAdd: () => void;
  onMove: (index: number, direction: -1 | 1) => void;
  onRemove: (index: number) => void;
  onUpdate: (index: number, value: string) => void;
  tokens: readonly string[];
}) {
  const addButtonRef = useRef<HTMLButtonElement>(null);
  const inputRefs = useRef<Array<HTMLInputElement | null>>([]);
  const pendingFocusIndex = useRef<number | undefined>(undefined);

  useEffect(() => {
    if (pendingFocusIndex.current === undefined) return;
    const input = inputRefs.current[pendingFocusIndex.current];
    pendingFocusIndex.current = undefined;
    if (input) input.focus();
    else addButtonRef.current?.focus();
  });

  return (
    <section aria-labelledby="custom-engine-arguments-heading" className="grid gap-2.5 pt-1">
      <div className="flex items-center justify-between gap-3">
        <p className="text-sm font-medium" id="custom-engine-arguments-heading">
          Custom arguments
        </p>
        <Button
          disabled={disabled}
          onClick={() => {
            pendingFocusIndex.current = tokens.length;
            onAdd();
          }}
          ref={addButtonRef}
          size="sm"
          type="button"
          variant="outline"
        >
          Add token
        </Button>
      </div>
      {tokens.length ? (
        <ol className="grid gap-2" aria-label="Custom engine argument tokens">
          {tokens.map((token, index) => (
            <li className="flex min-w-0 items-center gap-2" key={index}>
              <Input
                aria-label={`Custom token ${index + 1}`}
                disabled={disabled}
                onChange={(event) => onUpdate(index, event.target.value)}
                ref={(input) => {
                  inputRefs.current[index] = input;
                }}
                value={token}
              />
              <div className="flex shrink-0 gap-1">
                <Button
                  aria-label={`Move custom token ${index + 1} up`}
                  disabled={disabled || index === 0}
                  onClick={() => {
                    pendingFocusIndex.current = index - 1;
                    onMove(index, -1);
                  }}
                  size="icon-sm"
                  type="button"
                  variant="outline"
                >
                  <ChevronUpIcon aria-hidden="true" />
                </Button>
                <Button
                  aria-label={`Move custom token ${index + 1} down`}
                  disabled={disabled || index === tokens.length - 1}
                  onClick={() => {
                    pendingFocusIndex.current = index + 1;
                    onMove(index, 1);
                  }}
                  size="icon-sm"
                  type="button"
                  variant="outline"
                >
                  <ChevronDownIcon aria-hidden="true" />
                </Button>
                <Button
                  aria-label={`Remove custom token ${index + 1}`}
                  disabled={disabled}
                  onClick={() => {
                    pendingFocusIndex.current = Math.min(index, tokens.length - 2);
                    onRemove(index);
                  }}
                  size="icon-sm"
                  type="button"
                  variant="outline"
                >
                  <Trash2Icon aria-hidden="true" />
                </Button>
              </div>
            </li>
          ))}
        </ol>
      ) : null}
    </section>
  );
}
