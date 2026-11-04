import { Timer } from "lucide-react";
import { Link } from "react-router-dom";

import { ProfileMenu } from "@/app/profile-menu";
import { Badge } from "@/components/ui/badge";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { useDeveloperSettings } from "@/hooks/use-developer-settings";

/**
 * Top bar spanning the full viewport width above the sidebar. The logo mark and the
 * `dotcore` wordmark occupy the left slot; below the `md` breakpoint a sidebar trigger precedes
 * them, since the off-canvas drawer has no control of its own to open it. A reserved empty center
 * awaits a future search or chat control, and the profile menu sits on the right.
 */
export function ConsoleHeader() {
  return (
    <header className="z-10 flex h-(--header-height) shrink-0 items-center gap-4.5 border-b border-border bg-card pl-3.25 pr-6.5 max-md:gap-2 max-md:px-3.5">
      <div className="flex min-w-0 items-center gap-3">
        <SidebarTrigger className="md:hidden" />
        <svg
          data-slot="console-mark"
          className="size-5.5 flex-none"
          viewBox="0 0 44 44"
          aria-hidden="true"
        >
          <rect
            x="1.5"
            y="1.5"
            width="41"
            height="41"
            rx="11.5"
            fill="none"
            stroke="currentColor"
            strokeWidth="3"
          />
          <circle className="fill-primary" cx="22" cy="22" r="6" />
        </svg>
        <span className="flex-none text-[15.5px] font-semibold tracking-[0.2px]">dotcore</span>
      </div>
      <div className="flex-1" />
      <LatencySimulationIndicator />
      <ProfileMenu />
    </header>
  );
}

/** Link to Preferences shown while synthetic latency is on, so a slow Console is never a mystery. */
function LatencySimulationIndicator() {
  const { latencyEnabled, latencyMinMs, latencyMaxMs } = useDeveloperSettings();

  if (!latencyEnabled) {
    return null;
  }

  const range =
    latencyMinMs === latencyMaxMs ? `${latencyMinMs} ms` : `${latencyMinMs}–${latencyMaxMs} ms`;

  return (
    <Link
      aria-label={`Simulated latency ${range}. Open preferences`}
      className="flex-none rounded-full focus-visible:outline-[3px] focus-visible:outline-primary focus-visible:outline-offset-2"
      to="/preferences"
    >
      <Badge className="border-warning" variant="outline">
        <Timer aria-hidden="true" />
        <span className="max-md:sr-only">Latency {range}</span>
      </Badge>
    </Link>
  );
}
