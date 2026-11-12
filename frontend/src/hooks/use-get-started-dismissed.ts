import { useSyncExternalStore } from "react";

import { getBrowserStorage } from "@/lib/browser-storage";
import {
  readGetStartedDismissed,
  subscribeToGetStartedDismissed,
  writeGetStartedDismissed,
} from "@/lib/console-preferences";

/**
 * Whether the operator dismissed Get started, with a setter. The preference is remembered per
 * browser and followed across tabs; every user of the hook updates together.
 */
export function useGetStartedDismissed(): [boolean, (dismissed: boolean) => void] {
  const dismissed = useSyncExternalStore(subscribe, getSnapshot);

  return [dismissed, setDismissed];
}

function getSnapshot(): boolean {
  return readGetStartedDismissed(getBrowserStorage());
}

function subscribe(onChange: () => void): () => void {
  return subscribeToGetStartedDismissed(globalThis.window, getBrowserStorage(), onChange);
}

function setDismissed(dismissed: boolean): void {
  writeGetStartedDismissed(getBrowserStorage(), dismissed);
}
