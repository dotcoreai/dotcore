import type { RefObject } from "react";

import { usePanelTitle } from "@/app/panel-titles-context";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import type { PanelEntry } from "@/lib/panel-stack";

/**
 * The inspector a page shows while the panel URL names a resource its list has not read yet. It
 * names the resource from the titles published earlier in the session, else `fallbackTitle`, so a
 * panel arriving from another page stays open while the page reads its list. The page replaces it
 * with its own inspector once the resource is read.
 */
export function InspectorPlaceholder({
  entry,
  error,
  fallbackTitle,
  id,
  onBack,
  onOpenChange,
  openerRef,
}: {
  entry: PanelEntry;
  /** The page's failed first read, stated in place of the loading feedback. */
  error?: string;
  fallbackTitle: string;
  id: string;
  onBack: () => void;
  onOpenChange: (open: boolean) => void;
  openerRef: RefObject<HTMLElement | null>;
}) {
  const publishedTitle = usePanelTitle(entry);
  const loadingVisible = useDelayedLoading(!error);

  return (
    <InspectorShell
      body={
        error ? (
          <p className="text-sm text-destructive" role="alert">
            {error}
          </p>
        ) : loadingVisible ? (
          <p className="text-sm text-muted-foreground" role="status">
            Loading…
          </p>
        ) : null
      }
      focusKey={`${entry.screen}:${entry.resourceId}`}
      id={id}
      onBack={onBack}
      onOpenChange={onOpenChange}
      open
      openerRef={openerRef}
      title={publishedTitle ?? fallbackTitle}
    />
  );
}
