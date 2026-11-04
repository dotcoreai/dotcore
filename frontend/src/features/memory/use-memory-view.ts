import { useCallback, useEffect, useState } from "react";

import type { MemoryView } from "@/gen/dotcore/console/v1/preferences_pb";
import { getBrowserStorage } from "@/lib/browser-storage";
import { readMemoryView, subscribeToMemoryView, writeMemoryView } from "@/lib/console-preferences";

/**
 * The viewer's memory view preference, remembered per browser and followed across tabs. It reads
 * as all machines until the viewer picks otherwise.
 */
export function useMemoryView(): [MemoryView, (memoryView: MemoryView) => void] {
  const [memoryView, setMemoryView] = useState(() => readMemoryView());

  useEffect(
    () =>
      subscribeToMemoryView(
        typeof window === "undefined" ? undefined : window,
        getBrowserStorage(),
        setMemoryView,
      ),
    [],
  );

  const select = useCallback((next: MemoryView) => {
    setMemoryView(next);
    writeMemoryView(getBrowserStorage(), next);
  }, []);

  return [memoryView, select];
}
