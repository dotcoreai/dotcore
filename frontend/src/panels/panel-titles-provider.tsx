import { type ReactNode, useState } from "react";

import { PanelTitleStore } from "./panel-titles";
import {
  type PanelTitleLookup,
  PanelTitleLookupContext,
  PanelTitleStoreContext,
} from "./panel-titles-context";

/**
 * Owns the session's panel titles. `lookup` names a resource no page has published yet, for kinds the
 * backend can read one at a time.
 */
export function PanelTitlesProvider({
  children,
  lookup,
}: {
  children: ReactNode;
  lookup?: PanelTitleLookup;
}) {
  const [store] = useState(() => new PanelTitleStore());

  return (
    <PanelTitleStoreContext.Provider value={store}>
      <PanelTitleLookupContext.Provider value={lookup}>{children}</PanelTitleLookupContext.Provider>
    </PanelTitleStoreContext.Provider>
  );
}
