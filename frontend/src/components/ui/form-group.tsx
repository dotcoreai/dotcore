import { Collapsible } from "@base-ui/react/collapsible";
import { ChevronRightIcon } from "lucide-react";
import { type ReactNode, useId, useState } from "react";

import { FieldHelp } from "@/components/ui/field";
import type { FieldErrors } from "@/lib/form-validation";

interface FormGroupProps {
  children: ReactNode;
  /** Whether the group starts open; a group holding an invalid field starts open regardless. */
  defaultOpen?: boolean;
  errors: FieldErrors;
  /** The error keys of the fields inside. */
  fields?: readonly string[];
  /** What the fields have in common, shown in a help popover beside the title. */
  help?: ReactNode;
  /** The errors of the latest blocked navigation; a new value flagging a field inside opens it. */
  revealedErrors?: FieldErrors;
  title: string;
}

/**
 * A collapsible box of related form fields, titled with an optional help popover. It opens when a
 * blocked navigation flags one of its fields, so the field renders before the form moves focus to
 * it; the operator can collapse it again.
 */
export function FormGroup({
  children,
  defaultOpen = true,
  errors,
  fields = [],
  help,
  revealedErrors,
  title,
}: FormGroupProps) {
  const descriptionId = useId();
  const [open, setOpen] = useState(() => defaultOpen || fields.some((field) => errors.has(field)));
  const [shownReveal, setShownReveal] = useState(revealedErrors);

  if (revealedErrors !== shownReveal) {
    setShownReveal(revealedErrors);
    if (fields.some((field) => revealedErrors?.has(field))) setOpen(true);
  }

  return (
    <Collapsible.Root
      aria-label={title}
      className={formGroupBox}
      onOpenChange={setOpen}
      open={open}
      role="group"
    >
      <div className="flex min-w-0 items-center gap-1.5 px-4 py-3">
        <Collapsible.Trigger
          aria-describedby={help ? descriptionId : undefined}
          className="group/section flex min-w-0 items-center gap-2 rounded-sm text-left text-sm font-semibold outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
        >
          <ChevronRightIcon
            aria-hidden="true"
            className="size-4 shrink-0 text-muted-foreground transition-transform group-data-panel-open/section:rotate-90 motion-reduce:transition-none"
          />
          <span>{title}</span>
        </Collapsible.Trigger>
        {help ? (
          <FieldHelp descriptionId={descriptionId} label={title}>
            {help}
          </FieldHelp>
        ) : null}
      </div>
      <Collapsible.Panel className={formGroupBody}>{children}</Collapsible.Panel>
    </Collapsible.Root>
  );
}

/**
 * A box of related form fields opened and closed by a switch on its title line, with no collapse
 * toggle: `header` holds the switch's field and `children` the fields shown while it is on.
 */
export function SwitchedFormGroup({
  children,
  header,
  label,
}: {
  children?: ReactNode;
  header: ReactNode;
  label: string;
}) {
  return (
    <div aria-label={label} className={formGroupBox} role="group">
      <div className="px-4 py-3 text-sm [&_label]:font-semibold">{header}</div>
      {children ? <div className={formGroupBody}>{children}</div> : null}
    </div>
  );
}

const formGroupBox = "min-w-0 rounded-lg border border-border bg-card";
const formGroupBody = "grid min-w-0 gap-3 px-4 pb-4";
