import type { RefObject } from "react";

import type { DiagnosticLogOperations } from "@/api/diagnostic-log-client";
import { LogViewerModal } from "@/components/log-viewer/log-viewer-modal";

export interface DiagnosticLogViewerProps {
  onOpenChange: (open: boolean) => void;
  open: boolean;
  openerRef?: RefObject<HTMLElement | null>;
  operations: DiagnosticLogOperations;
}

export function DiagnosticLogViewer({
  onOpenChange,
  open,
  openerRef,
  operations,
}: DiagnosticLogViewerProps) {
  return (
    <LogViewerModal
      copy={diagnosticCopy}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      operations={operations}
      perSourceReconnect
      sessionKey="site-logs"
      showSourceFilters
      title="Site logs"
    />
  );
}

const diagnosticCopy = {
  ariaLabel: "Site log output",
  continuityWarning:
    "Continuity boundary: rotation or reconnect can omit or repeat machine output.",
  empty: "No retained backend logs are available.",
  unavailable: "Site logs are unavailable.",
};
