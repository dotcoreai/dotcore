import type { ReactNode } from "react";

interface DetailProps {
  label: string;
  title?: string;
  value: ReactNode;
}

interface DetailListProps {
  children: ReactNode;
}

/**
 * A label/value definition list, laid out as a two-column grid with the label on the left and the
 * value right-aligned. Composes with any number of `Detail` rows.
 */
function DetailList({ children }: DetailListProps) {
  return (
    <dl className="mt-3 grid grid-cols-[minmax(100px,1fr)_minmax(130px,1.4fr)] gap-x-3.5 gap-y-2">
      {children}
    </dl>
  );
}

/** One label/value row within a `DetailList`. `title` sets a tooltip on the value, e.g. a full timestamp behind a relative-time value. */
function Detail({ label, title, value }: DetailProps) {
  return (
    <>
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="break-words text-right text-[12.5px] font-medium" title={title}>
        {value}
      </dd>
    </>
  );
}

export { Detail, DetailList };
export type { DetailListProps, DetailProps };
