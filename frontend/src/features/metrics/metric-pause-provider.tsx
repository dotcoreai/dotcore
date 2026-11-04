import { type ReactNode, useMemo, useState } from "react";

import {
  MetricPauseContext,
  type MetricPauseRegistration,
  MetricPauseRegistrationContext,
} from "./metric-pause";

/**
 * Pauses every metric panel below it while any log viewer is open, so the streams a viewer's
 * reads compete with are closed and the panels keep the values they last read.
 */
export function MetricPauseProvider({ children }: { children: ReactNode }) {
  const [holders, setHolders] = useState<ReadonlySet<symbol>>(() => new Set());
  const registration = useMemo<MetricPauseRegistration>(
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
    <MetricPauseRegistrationContext value={registration}>
      <MetricPauseContext value={holders.size > 0}>{children}</MetricPauseContext>
    </MetricPauseRegistrationContext>
  );
}
