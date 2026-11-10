import type { RefObject } from "react";
import { useEffect, useRef } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Notice } from "@/components/ui/notice";
import { DesiredArgumentList } from "@/features/models/runtime/desired-arguments";
import { desiredArgv } from "@/features/models/runtime/desired-argv";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

/** Shows the backend-resolved launch command of the current deployment draft. */
export function CommandPreviewDialog({
  error,
  loadingMessageVisible,
  onOpenChange,
  onRetry,
  open,
  openerRef,
  pending,
  snapshot,
}: {
  error?: string;
  loadingMessageVisible: boolean;
  onOpenChange: (open: boolean) => void;
  onRetry: () => void;
  open: boolean;
  openerRef: RefObject<HTMLButtonElement | null>;
  pending: boolean;
  snapshot?: ResolvedLocalModelDeploymentSnapshot;
}) {
  const closeButtonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (open) closeButtonRef.current?.focus();
  }, [open]);

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      onOpenChangeComplete={(nextOpen) => {
        if (!nextOpen) requestAnimationFrame(() => openerRef.current?.focus());
      }}
    >
      <DialogContent
        className="max-w-sm"
        finalFocus={false}
        initialFocus={closeButtonRef}
        style={{ zIndex: "calc(var(--layer-modal) + 2)" }}
      >
        <DialogHeader>
          <DialogTitle>Command preview</DialogTitle>
          <DialogDescription>Backend-resolved ordered command.</DialogDescription>
        </DialogHeader>
        {error ? (
          <Notice role="alert" tone="destructive">
            {error}
          </Notice>
        ) : loadingMessageVisible ? (
          <p className="text-sm text-muted-foreground" role="status">
            Resolving command…
          </p>
        ) : pending ? null : snapshot ? (
          <DesiredPlan bounded snapshot={snapshot} />
        ) : (
          <p className="text-sm text-muted-foreground" role="status">
            No command is available.
          </p>
        )}
        <DialogFooter>
          {error ? (
            <Button onClick={onRetry} type="button">
              Retry
            </Button>
          ) : null}
          <Button
            aria-label="Close command preview"
            onClick={() => onOpenChange(false)}
            ref={closeButtonRef}
            type="button"
            variant="outline"
          >
            Close
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function DesiredPlan({
  bounded = false,
  snapshot,
}: {
  bounded?: boolean;
  snapshot: ResolvedLocalModelDeploymentSnapshot;
}) {
  const argv = desiredArgv(snapshot);
  return (
    <section
      aria-labelledby="desired-plan-heading"
      className={`min-w-0 ${bounded ? "max-h-[min(24rem,60dvh)] overflow-x-hidden overflow-y-auto" : ""}`}
    >
      <h2 className="text-sm font-semibold" id="desired-plan-heading">
        Desired arguments
      </h2>
      <DesiredArgumentList argv={argv} className="mt-2" />
    </section>
  );
}
