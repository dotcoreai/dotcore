import { type ReactNode, useState } from "react";

import { createMemoryHoverStore, MemoryHoverContext } from "./memory-hover-store";

/**
 * Links a hovered model deployment between memory bars and the cards that list the same
 * deployments. Subscribers read a derived value, so a hover re-renders only the elements whose
 * highlight changes, never the page that provides the link.
 */
export function MemoryHoverProvider({ children }: { children: ReactNode }) {
  const [store] = useState(createMemoryHoverStore);
  return <MemoryHoverContext.Provider value={store}>{children}</MemoryHoverContext.Provider>;
}
