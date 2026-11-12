import { create } from "@bufbuild/protobuf";
import { FieldMaskSchema } from "@bufbuild/protobuf/wkt";
import { type ReactNode, useCallback, useRef, useState } from "react";

import { useClients } from "@/api/clients";
import { ConsoleSettingsSchema } from "@/gen/dotcore/site/v1/site_pb";
import { describeMutationError } from "@/lib/mutation-errors";
import { useWatchedConsoleSettings } from "@/resource-watch/resource-watch";

import { GetStartedHiddenContext } from "./use-get-started-hidden";

/**
 * Reads the site's Get started setting from the console settings watch and writes it through the
 * site service. An accepted write shows at once; the watch takes over at its next change.
 */
export function GetStartedHiddenProvider({ children }: { children: ReactNode }) {
  const { siteClient } = useClients();
  const watched = useWatchedConsoleSettings()?.value;
  const watchedHidden = watched ? (watched.settings?.getStartedHidden ?? false) : undefined;
  const [accepted, setAccepted] = useState<boolean>();
  const [lastWatched, setLastWatched] = useState(watchedHidden);
  const [error, setError] = useState<string>();
  const pendingRef = useRef(false);
  // An accepted value holds until the watch next changes.
  if (lastWatched !== watchedHidden) {
    setLastWatched(watchedHidden);
    setAccepted(undefined);
  }
  const hidden = accepted ?? watchedHidden;

  const setHidden = useCallback(
    async (next: boolean) => {
      setError(undefined);
      pendingRef.current = true;
      try {
        const response = await siteClient.updateConsoleSettings({
          settings: create(ConsoleSettingsSchema, { getStartedHidden: next }),
          updateMask: create(FieldMaskSchema, { paths: ["get_started_hidden"] }),
        });
        setAccepted(response.settings?.getStartedHidden ?? next);
        return true;
      } catch (failure) {
        setError(
          describeMutationError(
            "Saving the Get started setting",
            failure,
            () => "Check the site, then try again.",
          ),
        );
        return false;
      } finally {
        pendingRef.current = false;
      }
    },
    [siteClient],
  );

  // A failed write is not retried on its own: the next explicit change clears the error.
  const hideOnce = useCallback(() => {
    if (hidden !== false || error !== undefined || pendingRef.current) return;

    void setHidden(true);
  }, [error, hidden, setHidden]);

  return (
    <GetStartedHiddenContext value={{ error, hideOnce, hidden, setHidden }}>
      {children}
    </GetStartedHiddenContext>
  );
}
