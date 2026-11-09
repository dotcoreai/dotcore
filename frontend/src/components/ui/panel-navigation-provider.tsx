import type { ReactNode } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InspectorDismissalSuspension } from "@/components/ui/inspector-shell";
import { PanelNavigationState, usePanelDiscardPrompt } from "@/panels/panel-navigation";
import type { PanelTitleLookup } from "@/panels/panel-titles-context";

/**
 * Panel navigation with its unsaved-changes question: leaving a dirty panel asks before discarding
 * its edits, and inspectors stay open while the question is up. `titleLookup` names a parent
 * resource no page has published yet.
 */
export function PanelNavigationProvider({
  children,
  titleLookup,
}: {
  children: ReactNode;
  titleLookup?: PanelTitleLookup;
}) {
  return (
    <PanelNavigationState titleLookup={titleLookup}>
      <PanelDiscardGuard>{children}</PanelDiscardGuard>
    </PanelNavigationState>
  );
}

function PanelDiscardGuard({ children }: { children: ReactNode }) {
  const prompt = usePanelDiscardPrompt();
  if (!prompt) return children;

  return (
    <InspectorDismissalSuspension active={prompt.open} refusals={prompt.refusals}>
      {children}
      <Dialog
        open={prompt.open}
        onOpenChange={(open) => {
          if (!open) prompt.cancel();
        }}
      >
        <DialogContent showCloseButton={false}>
          <DialogHeader>
            <DialogTitle>Discard changes?</DialogTitle>
            <DialogDescription>
              Your unsaved changes will be lost if you leave this editor.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button onClick={prompt.cancel} type="button" variant="outline">
              Keep editing
            </Button>
            <Button onClick={prompt.confirm} type="button" variant="destructive">
              Discard changes
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </InspectorDismissalSuspension>
  );
}
