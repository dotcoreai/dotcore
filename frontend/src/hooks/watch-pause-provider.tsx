import { type ReactNode, useMemo, useState } from "react";

import {
  WatchPauseContext,
  type WatchPauseRegistration,
  WatchPauseRegistrationContext,
} from "./use-watch-pause";

/**
 * Pauses every live read below it while anything holds a pause (such as an open log viewer), so
 * the streams that work competes with are closed and the readers keep the values they last read.
 */
export function WatchPauseProvider({ children }: { children: ReactNode }) {
  const [holders, setHolders] = useState<ReadonlySet<symbol>>(() => new Set());
  const registration = useMemo<WatchPauseRegistration>(
    () => ({
      register: (token, holding) =>
        setHolders((current) => {
          if (current.has(token) === holding) return current;

          const next = new Set(current);
          if (holding) next.add(token);
          else next.delete(token);
          return next;
        }),
    }),
    [],
  );

  return (
    <WatchPauseRegistrationContext value={registration}>
      <WatchPauseContext value={holders.size > 0}>{children}</WatchPauseContext>
    </WatchPauseRegistrationContext>
  );
}
