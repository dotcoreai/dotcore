export type FieldErrors = ReadonlyMap<string, string>;
export type FieldControls = ReadonlyMap<string, HTMLElement | null | undefined>;

/**
 * Builds an ordered map of local field errors from a form's validation results.
 * Entries without a message are omitted so callers can retain declaration order while validating
 * only fields that apply to the submitted form state.
 */
export function toFieldErrors(
  entries: Iterable<readonly [string, string | undefined]>,
): FieldErrors {
  const errors = new Map<string, string>();

  for (const [field, message] of entries) {
    if (message) errors.set(field, message);
  }

  return errors;
}

/**
 * Moves focus to the first invalid registered field, preserving validation declaration order.
 * Form-local callers invoke this after storing errors; transport and backend errors do not use it.
 */
export function focusFirstInvalidField(errors: FieldErrors, controls: FieldControls): void {
  for (const field of errors.keys()) {
    const control = controls.get(field);
    if (control) {
      control.focus();
      return;
    }
  }
}
