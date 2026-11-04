import { type ReactNode, useCallback, useEffect, useMemo, useState } from "react";

import { SidebarPreferenceContext } from "@/app/sidebar-preference-context";
import { getBrowserStorage, type StorageLike } from "@/lib/browser-storage";
import {
  readSidebarCollapsed,
  subscribeToSidebarCollapsed,
  writeSidebarCollapsed,
} from "@/lib/console-preferences";

export interface SidebarPreferenceProviderProps {
  children: ReactNode;
  eventTarget?: EventTarget;
  storage?: StorageLike;
}

export function SidebarPreferenceProvider({
  children,
  eventTarget = globalThis.window,
  storage = getBrowserStorage(),
}: SidebarPreferenceProviderProps) {
  const [collapsed, setCurrentCollapsed] = useState(() => readSidebarCollapsed(storage));

  const setCollapsed = useCallback(
    (nextCollapsed: boolean) => {
      setCurrentCollapsed(nextCollapsed);
      writeSidebarCollapsed(storage, nextCollapsed);
    },
    [storage],
  );

  useEffect(
    () => subscribeToSidebarCollapsed(eventTarget, storage, setCurrentCollapsed),
    [eventTarget, storage],
  );

  const value = useMemo(() => ({ collapsed, setCollapsed }), [collapsed, setCollapsed]);

  return <SidebarPreferenceContext value={value}>{children}</SidebarPreferenceContext>;
}
