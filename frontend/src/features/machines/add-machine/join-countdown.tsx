import type { Timestamp } from "@bufbuild/protobuf/wkt";
import { timestampDate } from "@bufbuild/protobuf/wkt";
import { Clock } from "lucide-react";
import { useEffect, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { timing } from "@/lib/timing";
import { cn } from "@/lib/utils";

const WARNING_REMAINING_MS = 2 * 60 * 1_000;

/**
 * The time left before the join command expires, with a ring that drains as it runs out. It turns
 * to the warning tone in the last two minutes.
 */
export function CountdownPill({ expiresAt, startedAt }: { expiresAt: Timestamp; startedAt: Date }) {
  const expiry = timestampDate(expiresAt).getTime();
  const total = Math.max(expiry - startedAt.getTime(), 1);
  const remaining = useRemaining(expiry);
  const warning = remaining <= WARNING_REMAINING_MS;

  return (
    <Badge
      className={cn(
        "gap-1.5 py-1 text-xs tabular-nums",
        // The last two minutes swap the info colors for the warning ones.
        warning && "bg-warning text-warning-foreground",
      )}
      data-tone={warning ? "warning" : "info"}
      variant="info"
    >
      <Ring fraction={remaining / total} />
      <span>{formatRemaining(remaining)} left</span>
    </Badge>
  );
}

/** The badge that replaces the countdown once the join command has expired. */
export function ExpiredBadge() {
  return (
    <Badge className="gap-1.5 py-1 text-xs" variant="destructive">
      <Clock aria-hidden="true" />
      Expired
    </Badge>
  );
}

function Ring({ fraction }: { fraction: number }) {
  const radius = 6;
  const circumference = 2 * Math.PI * radius;
  const left = Math.min(Math.max(fraction, 0), 1);

  return (
    <svg aria-hidden="true" className="size-4 -rotate-90" viewBox="0 0 16 16">
      <circle
        cx="8"
        cy="8"
        fill="none"
        opacity="0.22"
        r={radius}
        stroke="currentColor"
        strokeWidth="2"
      />
      <circle
        cx="8"
        cy="8"
        fill="none"
        r={radius}
        stroke="currentColor"
        strokeDasharray={circumference}
        strokeDashoffset={circumference * (1 - left)}
        strokeWidth="2"
      />
    </svg>
  );
}

// Ticks from the timing module until the expiry passes; the timer stops on unmount.
function useRemaining(expiry: number): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;

    function tick() {
      const current = Date.now();
      setNow(current);
      if (current < expiry) timer = setTimeout(tick, timing().commandCountdownTickMs);
    }

    timer = setTimeout(tick, timing().commandCountdownTickMs);
    return () => clearTimeout(timer);
  }, [expiry]);

  return Math.max(expiry - now, 0);
}

function formatRemaining(ms: number): string {
  const seconds = Math.ceil(ms / 1_000);
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  const rest = String(seconds % 60).padStart(2, "0");

  return hours > 0 ? `${hours}:${String(minutes).padStart(2, "0")}:${rest}` : `${minutes}:${rest}`;
}
