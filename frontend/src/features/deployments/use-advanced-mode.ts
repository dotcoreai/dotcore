import { useState } from "react";

import { formatCount } from "@/lib/format";

/** Advanced mode state of a deployment dialog, with the announcement the shell reads out. */
export interface AdvancedMode {
  advanced: boolean;
  announcement: string;
  /** Turns Advanced on for a reason the operator must hear, such as an error on a hidden step. */
  enable: (announcement: string) => void;
  /** Sets Advanced and announces the resulting step count. */
  toggle: (next: boolean, stepCount: number) => void;
}

/**
 * Tracks whether a deployment dialog shows its detail steps. Every launch starts in basic mode, even
 * when the dialog stays mounted between launches; entered values live with the dialog, so toggling
 * never discards them.
 */
export function useAdvancedMode(open: boolean): AdvancedMode {
  const [advanced, setAdvanced] = useState(false);
  const [announcement, setAnnouncement] = useState("");
  const [shownOpen, setShownOpen] = useState(open);

  // Adjusting state during render resets the mode before the stale one can render.
  if (shownOpen !== open) {
    setShownOpen(open);
    if (open) {
      setAdvanced(false);
      setAnnouncement("");
    }
  }

  return {
    advanced,
    announcement,
    enable: (message) => {
      setAdvanced(true);
      setAnnouncement(message);
    },
    toggle: (next, stepCount) => {
      setAdvanced(next);
      setAnnouncement(`Advanced ${next ? "on" : "off"}: ${formatCount(stepCount, "step")}`);
    },
  };
}
