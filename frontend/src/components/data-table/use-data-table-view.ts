import { useCallback, useMemo, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import type { DataTableView, DataTableViewCodec } from "@/lib/data-table-view";

/**
 * A data table view, read from and written to the URL through its codec. View changes replace the
 * current history entry and keep its panel history state. The panel navigation provider carries the
 * view across panel Back and close, so every location's own view is the one to show. Each codec
 * must be stable, such as one built at module level; a table may switch between stable codecs, and
 * its location is then read again with the new one.
 */
export function useDataTableView<C extends string, F extends string>(
  codec: DataTableViewCodec<C, F>,
): {
  set: (view: DataTableView<C, F>) => void;
  state: DataTableView<C, F>;
} {
  const location = useLocation();
  const navigate = useNavigate();
  const urlView = useMemo(() => codec.parse(location.search), [codec, location.search]);
  // A write shows at once, before its navigation lands; each new location shows its own view.
  const [current, setCurrent] = useState({ codec, key: location.key, view: urlView });
  let state = current.view;
  if (current.key !== location.key || current.codec !== codec) {
    state = urlView;
    setCurrent({ codec, key: location.key, view: urlView });
  }

  const set = useCallback(
    (next: DataTableView<C, F>) => {
      setCurrent({ codec, key: location.key, view: next });
      void navigate(
        { pathname: location.pathname, search: codec.apply(location.search, next) },
        { replace: true, state: location.state as unknown },
      );
    },
    [codec, location, navigate],
  );

  return { set, state };
}
