import { type Timestamp, timestampDate } from "@bufbuild/protobuf/wkt";
import { LoaderCircle, X } from "lucide-react";

import { formatTime } from "./join-format";

/** One answer that ended without going through: when it ended, and why in the caller's words. */
export interface AttemptRow {
  at?: Timestamp;
  text: string;
}

/**
 * The attempts made so far, each with the time it ended and why it did not go through, followed by
 * the wait for the next one. Without attempts it is only the wait. A reason keeps the line breaks
 * the manager gave it, so a reason listing several items stays readable.
 */
export function AttemptLog({
  attempts,
  pendingLabel,
}: {
  attempts: AttemptRow[];
  pendingLabel: string;
}) {
  return (
    <div className="flex flex-col gap-2 text-[13px]">
      {attempts.length > 0 ? (
        <ul className="flex flex-col gap-2">
          {attempts.map((attempt, index) => (
            <li
              key={index}
              className="grid grid-cols-[3.75rem_1rem_minmax(0,1fr)] items-start gap-2"
            >
              <time
                className="pt-px font-mono text-xs text-muted-foreground"
                dateTime={attempt.at ? timestampDate(attempt.at).toISOString() : undefined}
              >
                {attempt.at ? formatTime(timestampDate(attempt.at)) : ""}
              </time>
              <X aria-hidden="true" className="mt-0.5 size-4 text-destructive" />
              <span className="break-words whitespace-pre-line">{attempt.text}</span>
            </li>
          ))}
        </ul>
      ) : null}
      <p className="flex items-center gap-2 text-muted-foreground" role="status">
        <LoaderCircle
          aria-hidden="true"
          className="size-4 animate-spin motion-reduce:animate-none"
        />
        {attempts.length > 0 ? "Waiting for the next try" : pendingLabel}
      </p>
    </div>
  );
}
