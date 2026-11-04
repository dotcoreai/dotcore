import { Moon, ScrollText } from "lucide-react";
import { useRef, useState } from "react";
import { useNavigate } from "react-router-dom";

import type { DiagnosticLogOperations } from "@/api/diagnostic-log-client";
import { diagnosticLogOperations } from "@/api/diagnostic-log-client";
import { PreferencesSectionIcon } from "@/app/console-section-icons";
import { useTheme } from "@/app/theme-context";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { DiagnosticLogViewer } from "@/features/diagnostic-logs/diagnostic-log-viewer";
import { ColorScheme } from "@/gen/dotcore/console/v1/preferences_pb";

export function ProfileMenu({
  diagnostics = diagnosticLogOperations,
}: {
  diagnostics?: DiagnosticLogOperations;
}) {
  const { colorScheme, setColorScheme } = useTheme();
  const navigate = useNavigate();
  const [menuOpen, setMenuOpen] = useState(false);
  const [logsOpen, setLogsOpen] = useState(false);
  const openLogsAfterMenuCloseRef = useRef(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const darkModeEnabled = colorScheme === ColorScheme.DARK;

  return (
    <>
      <DropdownMenu
        onOpenChange={setMenuOpen}
        onOpenChangeComplete={(open) => {
          if (open || !openLogsAfterMenuCloseRef.current) return;

          openLogsAfterMenuCloseRef.current = false;
          setLogsOpen(true);
        }}
        open={menuOpen}
      >
        <DropdownMenuTrigger
          aria-label="Open profile menu"
          className="console-operator relative inline-flex flex-none touch-target cursor-pointer items-center gap-2.25 border-0 bg-transparent p-0 text-foreground focus-visible:rounded-[5px] focus-visible:outline-[3px] focus-visible:outline-primary focus-visible:outline-offset-2"
          onMouseDown={(event) => {
            event.preventBaseUIHandler();
            setMenuOpen((wasOpen) => !wasOpen);
          }}
          ref={triggerRef}
          type="button"
        >
          <span className="console-profile-copy text-right leading-tight max-md:sr-only">
            <strong className="block text-sm">Local user</strong>
            <span className="block text-[11.5px] text-console-subtle">Administrator</span>
          </span>
          <span
            aria-hidden="true"
            className="console-profile-avatar inline-flex h-7.5 w-7.5 items-center justify-center rounded-full bg-info text-[11px] font-bold text-primary"
          >
            LU
          </span>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="end"
          className="w-[min(12rem,calc(100vw-16px))] border border-border"
          sideOffset={8}
        >
          <DropdownMenuItem onClick={() => void navigate("/preferences")}>
            <PreferencesSectionIcon aria-hidden="true" />
            Preferences
          </DropdownMenuItem>
          <DropdownMenuItem
            onClick={() => {
              openLogsAfterMenuCloseRef.current = true;
              setMenuOpen(false);
            }}
          >
            <ScrollText aria-hidden="true" />
            Site logs
          </DropdownMenuItem>
          <DropdownMenuCheckboxItem
            checked={darkModeEnabled}
            closeOnClick={false}
            onCheckedChange={(checked) =>
              setColorScheme(checked ? ColorScheme.DARK : ColorScheme.LIGHT)
            }
          >
            <Moon aria-hidden="true" />
            Dark mode
          </DropdownMenuCheckboxItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <DiagnosticLogViewer
        onOpenChange={setLogsOpen}
        open={logsOpen}
        openerRef={triggerRef}
        operations={diagnostics}
      />
    </>
  );
}
