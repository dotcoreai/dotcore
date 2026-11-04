import { createContext, useContext, useEffect, useRef } from "react";

export interface MetricPauseRegistration {
  register: (token: symbol, holding: boolean) => void;
}

export const MetricPauseContext = createContext(false);
export const MetricPauseRegistrationContext = createContext<MetricPauseRegistration | null>(null);

/** Whether a log viewer is open, which pauses metric panels. */
export function useMetricsPausedByLogs(): boolean {
  return useContext(MetricPauseContext);
}

/** Holds metric panels paused for as long as `open` is true; a no-op outside the provider. */
export function useMetricPauseWhile(open: boolean): void {
  const registration = useContext(MetricPauseRegistrationContext);
  const token = useRef(Symbol("metric-pause"));

  useEffect(() => {
    if (!registration || !open) return;

    const held = token.current;
    registration.register(held, true);
    return () => registration.register(held, false);
  }, [open, registration]);
}
