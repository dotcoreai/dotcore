import { createContext, useContext } from "react";

export type PanelPresentation = "continue" | "enter" | "idle" | "restore";

export const PanelPresentationContext = createContext<PanelPresentation>("idle");

export function usePanelPresentation(): PanelPresentation {
  return useContext(PanelPresentationContext);
}
