import { createContext, useContext, useEffect, useRef } from "react";

export interface WatchPauseRegistration {
  register: (token: symbol, holding: boolean) => void;
}

export const WatchPauseContext = createContext(false);
export const WatchPauseRegistrationContext = createContext<WatchPauseRegistration | null>(null);

/** Whether something is holding live reads paused, such as an open log viewer. */
export function useWatchPaused(): boolean {
  return useContext(WatchPauseContext);
}

/** Holds live reads paused for as long as `open` is true; a no-op outside the provider. */
export function useWatchPausedWhile(open: boolean): void {
  const registration = useContext(WatchPauseRegistrationContext);
  const token = useRef(Symbol("watch-pause"));

  useEffect(() => {
    if (!registration || !open) return;

    const held = token.current;
    registration.register(held, true);
    return () => registration.register(held, false);
  }, [open, registration]);
}
