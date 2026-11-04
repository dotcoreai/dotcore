import type { RefObject } from "react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { usePanelDirtyGuard, usePanelNavigation } from "@/app/console-layout";
import { usePanelPendingNavigation } from "@/app/panel-navigation-pending";
import { Button } from "@/components/ui/button";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { FieldError } from "@/components/ui/field-error";
import { Input } from "@/components/ui/input";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceTitle } from "@/components/ui/resource-marker";
import type { DeploymentMetadata, DeploymentMetadataUpdate } from "@/lib/deployment-metadata";
import { validateDeploymentName } from "@/lib/deployment-name";
import { popPanelLocation } from "@/lib/panel-stack";

export interface DeploymentNameSectionProps {
  draft: string;
  error?: string;
  inputRef: RefObject<HTMLInputElement | null>;
  onChange: (value: string) => void;
  onSave: () => void;
  pending: boolean;
  terminal: boolean;
}

/** Renders the source-neutral Name section used by every deployment editor. */
export function DeploymentNameSection({
  draft,
  error,
  inputRef,
  onChange,
  onSave,
  pending,
  terminal,
}: DeploymentNameSectionProps) {
  return (
    <section className="mt-6">
      <div className="grid gap-1">
        <label className="text-xs font-medium" htmlFor="deployment-name">
          Name
        </label>
        <Input
          ref={inputRef}
          aria-describedby={error ? "deployment-name-error" : undefined}
          aria-invalid={error ? "true" : undefined}
          disabled={pending || terminal}
          id="deployment-name"
          onChange={(event) => onChange(event.target.value)}
          value={draft}
        />
        {error ? <FieldError id="deployment-name-error">{error}</FieldError> : null}
      </div>
      <Button
        className="mt-4"
        disabled={pending || terminal}
        onClick={onSave}
        size="console"
        type="button"
      >
        {pending ? "Saving…" : "Save"}
      </Button>
    </section>
  );
}

export interface DeploymentEditorProps {
  /** Identity-marker color key of the deployment shown in the `<name> · Edit` title. */
  colorKey: string;
  deploymentId: string;
  metadata: DeploymentMetadata;
  /** Presentation name of the deployment shown in the `<name> · Edit` title. */
  name: string;
  onClose: () => void;
  onSaveMetadata: (update: DeploymentMetadataUpdate) => Promise<DeploymentMetadata>;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
  terminal?: boolean;
}

/** Edits owner-neutral deployment metadata through an owner-specific save adapter. */
export function DeploymentEditor({
  colorKey,
  deploymentId,
  metadata,
  name,
  onClose,
  onSaveMetadata,
  open,
  openerRef,
  terminal = false,
}: DeploymentEditorProps) {
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const displayName = metadata.displayName;
  const [baseline, setBaseline] = useState(metadata);
  const [draft, setDraft] = useState(displayName);
  const [error, setError] = useState<string>();
  const [pending, setPending] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const terminalRef = useRef(terminal);
  const dirty = draft !== baseline.displayName;

  useLayoutEffect(() => {
    terminalRef.current = terminal;
  }, [terminal]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      if (draft === baseline.displayName && draft !== displayName) {
        setDraft(displayName);
      }
    });
    return () => window.clearTimeout(timer);
  }, [baseline.displayName, displayName, draft]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      if (baseline.displayName !== displayName) {
        setBaseline({ displayName });
      }
    });
    return () => window.clearTimeout(timer);
  }, [baseline.displayName, displayName]);

  usePanelDirtyGuard(dirty && !terminal, () => {
    setDraft(baseline.displayName);
    setError(undefined);
  });
  usePanelPendingNavigation(pending, "Saving changes. Inspector navigation is unavailable.");

  const save = async () => {
    const validationError = validateDeploymentName(draft);
    if (validationError) {
      setError(validationError);
      inputRef.current?.focus();
      return;
    }

    setPending(true);
    setError(undefined);
    try {
      const accepted = await onSaveMetadata({
        fields: ["displayName"],
        metadata: { displayName: draft.trim() },
      });
      if (terminalRef.current) return;
      setBaseline(accepted);
      setDraft(accepted.displayName);
      panelNavigation.navigatePanel(popPanelLocation(location.pathname, location.search), {
        bypassDirtyGuard: true,
        bypassPendingGuard: true,
        replace: true,
      });
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : "Name could not be saved.");
    } finally {
      setPending(false);
    }
  };

  return (
    <InspectorShell
      body={
        <div className="inspector-sections">
          <DetailList>
            <Detail label="Deployment ID" value={deploymentId} />
          </DetailList>
          {terminal ? (
            <Notice role="status">
              This deployment is tearing down and can no longer be edited.
            </Notice>
          ) : null}
          <DeploymentNameSection
            draft={draft}
            error={error}
            inputRef={inputRef}
            onChange={(value) => {
              setDraft(value);
              setError(undefined);
            }}
            onSave={() => void save()}
            pending={pending}
            terminal={terminal}
          />
        </div>
      }
      focusKey={`edit:${deploymentId}`}
      id="deployment-edit-panel"
      navigationDisabledReason={
        pending ? "Saving changes. Inspector navigation is unavailable." : undefined
      }
      onBack={panelNavigation.back}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onClose();
      }}
      open={open}
      openerRef={openerRef}
      title={<ResourceTitle colorKey={colorKey} name={name} screen="Edit" />}
    />
  );
}
