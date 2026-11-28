import type { ReactNode } from "react";
import { useId } from "react";

import { FieldHelp } from "@/components/ui/field";

interface FormGroupProps {
  children: ReactNode;
  /** What the fields have in common, shown in a help popover beside the title. */
  help?: ReactNode;
  /** One line telling a newcomer what the group is for, shown under the title. */
  summary?: string;
  title: string;
}

/**
 * A titled section of related form fields, always open: a small heading with an optional help
 * popover over a divider, then the fields at the form's own indentation.
 */
export function FormGroup({ children, help, summary, title }: FormGroupProps) {
  const descriptionId = useId();

  return (
    <div aria-label={title} className={groupClass} role="group">
      <div className="grid min-w-0 gap-0.5">
        <div className="flex min-w-0 items-center gap-1.5 text-xs font-semibold text-muted-foreground">
          <span aria-describedby={help ? descriptionId : undefined}>{title}</span>
          {help ? (
            <FieldHelp descriptionId={descriptionId} label={title}>
              {help}
            </FieldHelp>
          ) : null}
        </div>
        {summary ? <p className="text-xs text-muted-foreground">{summary}</p> : null}
      </div>
      {children}
    </div>
  );
}

/**
 * A section of related form fields turned on and off by a switch on its heading line: `header`
 * holds the switch's field and `children` the fields shown, indented, while it is on.
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
    <div aria-label={label} className={groupClass} role="group">
      <div className="text-xs [&_label]:font-semibold">{header}</div>
      {children ? (
        <div className="ml-1.5 grid min-w-0 gap-3 border-l-2 border-border pl-3">{children}</div>
      ) : null}
    </div>
  );
}

const groupClass = "grid min-w-0 gap-3 border-t border-border pt-3 [h2+&]:border-t-0 [h2+&]:pt-0";
