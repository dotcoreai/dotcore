import type { ReactNode, RefObject } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { cn } from "@/lib/utils";

interface DeploymentDialogFrameProps {
  actions: ReactNode;
  bodyRef?: RefObject<HTMLDivElement | null>;
  children: ReactNode;
  /** Controls at the footer's start, such as a mode toggle; Cancel and actions stay at the end. */
  footerStart?: ReactNode;
  onOpenChange: (open: boolean) => void;
  onSubmit: () => void;
  open: boolean;
  openerRef?: RefObject<HTMLElement | null>;
  pending?: boolean;
  /** Content fixed between the scrolling body and the footer, such as a live summary. */
  persistentContent?: ReactNode;
  progress?: ReactNode;
  resourceColorKey: string;
  resourceName: string;
  submitDisabled?: boolean;
  title: string;
}

/**
 * Provides shared deployment-dialog geometry, heading, footer actions, submission, and dismissal.
 * Source dialogs supply their body, optional persistent content and footer-start controls, and the
 * actions that follow Cancel while retaining their draft and workflow.
 */
export function DeploymentDialogFrame({
  actions,
  bodyRef,
  children,
  footerStart,
  onOpenChange,
  onSubmit,
  open,
  openerRef,
  pending = false,
  persistentContent,
  progress,
  resourceColorKey,
  resourceName,
  submitDisabled = false,
  title,
}: DeploymentDialogFrameProps) {
  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen && pending) return;
        onOpenChange(nextOpen);
      }}
    >
      <DialogContent
        className={cn(
          // Full screen below sm; from sm a centered card of one fixed height, sized for the tallest step so
          // the dialog doesn't resize between steps.
          "inset-0 h-dvh max-h-none w-full max-w-none gap-0 overflow-hidden rounded-none p-0",
          "sm:inset-auto sm:top-1/2 sm:left-1/2 sm:h-[min(44rem,calc(100dvh-4rem))] sm:max-h-[calc(100dvh-2rem)] sm:w-[calc(100vw-2rem)] sm:max-w-[48rem] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl",
          persistentContent
            ? "grid-rows-[auto_minmax(0,1fr)_auto_auto]"
            : "grid-rows-[auto_minmax(0,1fr)_auto]",
        )}
        finalFocus={openerRef}
        render={
          <form
            noValidate
            onSubmit={(event) => {
              event.preventDefault();
              if (!pending && !submitDisabled) onSubmit();
            }}
          />
        }
        showCloseButton={!pending}
        suppressLayoutTransform
        style={{ zIndex: "calc(var(--layer-modal) + 1)" }}
      >
        <DialogHeader className="border-b border-border px-4 py-4 pr-16 sm:px-6 sm:pr-12">
          <DialogTitle
            aria-label={`${resourceName} — ${title}`}
            className="grid grid-cols-[auto_minmax(0,1fr)] items-center gap-x-2 gap-y-1 leading-normal"
          >
            <ResourceMarker className="mr-0" colorKey={resourceColorKey} />
            <span className="truncate text-xs font-semibold tracking-[0.02em] text-muted-foreground uppercase">
              {resourceName}
            </span>
            <span className="col-span-2 text-xl leading-tight font-semibold text-foreground">
              {title}
            </span>
          </DialogTitle>
          {progress}
        </DialogHeader>
        <div
          className="min-h-0 overflow-x-hidden overflow-y-auto px-4 py-4 sm:px-6 sm:py-6"
          data-testid="deployment-dialog-body"
          ref={bodyRef}
        >
          {children}
        </div>
        {persistentContent ? (
          <div
            className="border-t border-border px-4 py-3 sm:px-6"
            data-testid="deployment-dialog-persistent"
          >
            {persistentContent}
          </div>
        ) : null}
        <DialogFooter
          className="m-0 flex-col items-stretch justify-between rounded-none px-4 py-3 sm:rounded-b-xl sm:flex-col sm:justify-between sm:px-6"
          data-testid="deployment-dialog-footer"
        >
          <div className="flex w-full min-w-0 flex-wrap items-center justify-between gap-2">
            {footerStart ? (
              <div
                className="flex min-w-0 items-center gap-2"
                data-testid="deployment-dialog-footer-start"
              >
                {footerStart}
              </div>
            ) : null}
            <div
              className="ml-auto flex min-w-0 items-center justify-end gap-2"
              data-testid="deployment-dialog-actions"
            >
              <Button
                disabled={pending}
                onClick={() => onOpenChange(false)}
                size="lg"
                type="button"
                variant="outline"
              >
                Cancel
              </Button>
              {actions}
            </div>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
