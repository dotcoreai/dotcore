import { useState } from "react";

/** The fields of one form the operator has changed. */
export interface DirtyFields<Field extends string> {
  /** Whether the operator changed the field since the form opened or was reset. */
  isDirty: (field: Field) => boolean;
  /** Records an operator change to the field; it stays dirty until the form reopens or resets. */
  markDirty: (field: Field) => void;
  /** Returns every field to pristine. */
  reset: () => void;
}

/**
 * Tracks which fields of a form the operator has changed, for the form validation timing rule in
 * `frontend/AGENTS.md`. Every launch starts with no dirty field, even when the form stays mounted
 * between launches.
 */
export function useDirtyFields<Field extends string>(open: boolean): DirtyFields<Field> {
  const [dirtyFields, setDirtyFields] = useState<ReadonlySet<Field>>(() => new Set());
  const [shownOpen, setShownOpen] = useState(open);

  // Adjusting state during render clears the previous launch before it can gate a blur.
  if (shownOpen !== open) {
    setShownOpen(open);
    if (open) setDirtyFields(new Set());
  }

  return {
    isDirty: (field) => dirtyFields.has(field),
    markDirty: (field) =>
      setDirtyFields((fields) => (fields.has(field) ? fields : new Set(fields).add(field))),
    reset: () => setDirtyFields(new Set()),
  };
}
