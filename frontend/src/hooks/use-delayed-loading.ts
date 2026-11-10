import { useLayoutEffect, useRef, useState } from "react";

import { timing } from "@/lib/timing";

export function useDelayedLoading(loading: boolean): boolean {
  const [visible, setVisible] = useState(false);
  const visibleSinceRef = useRef<number | null>(null);
  const revealTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const hideTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useLayoutEffect(() => {
    if (visible) {
      visibleSinceRef.current = performance.now();
    }
  }, [visible]);

  useLayoutEffect(() => {
    if (loading) {
      if (!visible) {
        revealTimerRef.current = setTimeout(() => {
          setVisible(true);
        }, timing().loadingRevealMs);
      }
    } else if (visible) {
      const elapsed = performance.now() - (visibleSinceRef.current ?? performance.now());
      const remaining = timing().loadingMinimumVisibleMs - elapsed;

      if (remaining <= 0) {
        visibleSinceRef.current = null;
        setVisible(false);
      } else {
        hideTimerRef.current = setTimeout(() => {
          visibleSinceRef.current = null;
          setVisible(false);
        }, remaining);
      }
    } else {
      visibleSinceRef.current = null;
    }

    return () => {
      clearTimer(revealTimerRef);
      clearTimer(hideTimerRef);
    };
  }, [loading, visible]);

  return visible;
}

function clearTimer(timerRef: { current: ReturnType<typeof setTimeout> | null }) {
  if (timerRef.current !== null) {
    clearTimeout(timerRef.current);
    timerRef.current = null;
  }
}
