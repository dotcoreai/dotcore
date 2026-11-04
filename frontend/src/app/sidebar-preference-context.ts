import { createContext, useContext } from "react";

export interface SidebarPreferenceContextValue {
  collapsed: boolean;
  setCollapsed: (collapsed: boolean) => void;
}

export const SidebarPreferenceContext = createContext<SidebarPreferenceContextValue | null>(null);

export function useSidebarPreference(): SidebarPreferenceContextValue {
  const context = useContext(SidebarPreferenceContext);

  if (!context) {
    throw new Error("useSidebarPreference must be used within a SidebarPreferenceProvider");
  }

  return context;
}
