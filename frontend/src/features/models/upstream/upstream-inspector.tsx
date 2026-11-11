import { Pencil, Trash2 } from "lucide-react";
import { type RefObject, useMemo, useRef, useState } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Detail, DetailList } from "@/components/ui/detail-list";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import { UPSTREAM_MODEL_NAME } from "@/domain/upstream-presentation";
import type { ChatTarget, ConnectTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import { type RouteMention, RouteMentions } from "@/features/deployments/panel-mention";
import { deploymentRequestCards } from "@/features/metrics/request-card-specs";
import { RequestCards } from "@/features/metrics/request-cards";
import { deploymentConnectAction } from "@/features/models/deployment-connect-action";
import { modelInspectorPanelId } from "@/features/models/inspector/model-inspector-panel";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * Shows an upstream model deployment: its routes, its upstream source and its request metrics,
 * with Chat as the primary action and Edit and Teardown in the menu.
 */
export function UpstreamInspector({
  chatTarget,
  connectTarget,
  deployment,
  open,
  onBack,
  onConnect,
  onOpenChange,
  openerRef,
  onEdit,
  onUnload,
  error,
  teardownError,
  containingRoutes,
  initialRange,
  metricsClient,
  pending = false,
}: {
  /** Where the Chat action takes the operator; computed from the routes listing the deployment. */
  chatTarget?: ChatTarget;
  /** Where the Connect action takes the operator; computed from the routes listing the deployment. */
  connectTarget?: ConnectTarget;
  /** Opens the connect page of the target route. */
  onConnect?: (routeId: string) => void;
  deployment: ModelDeployment;
  open: boolean;
  onBack?: () => void;
  onOpenChange: (open: boolean) => void;
  openerRef: RefObject<HTMLElement | null>;
  onEdit: () => void;
  onUnload: () => Promise<boolean>;
  error?: string;
  teardownError?: string;
  containingRoutes?: readonly RouteMention[];
  /** The range label the request cards start at, such as the range of the page it opened from. */
  initialRange?: string;
  metricsClient: MetricsClient;
  pending?: boolean;
}) {
  const requestSpecs = useMemo(
    () => deploymentRequestCards(deployment.modelDeploymentId),
    [deployment.modelDeploymentId],
  );
  const [confirmingTeardown, setConfirmingTeardown] = useState(false);
  const cancelTeardownRef = useRef<HTMLButtonElement>(null);
  const teardownButtonRef = useRef<HTMLElement>(null);
  const config = deployment.upstreamDesiredConfiguration;
  const presentationName = deployment.modelDeploymentName || UPSTREAM_MODEL_NAME;
  const chat = useChatAction(chatTarget);
  const connect = deploymentConnectAction(connectTarget, onConnect, pending);

  return (
    <>
      <InspectorShell
        body={
          <div className="inspector-sections">
            {error && !(confirmingTeardown && teardownError) ? (
              <Notice role="alert" tone="destructive">
                {error}
              </Notice>
            ) : null}
            <section aria-labelledby="upstream-deployment-heading">
              <InspectorSectionHeader headingId="upstream-deployment-heading" title="Deployment" />
              <DetailList>
                <Detail label="Deployment ID" value={deployment.modelDeploymentId} />
                <Detail label="Routes" value={<RouteMentions routes={containingRoutes} />} />
              </DetailList>
            </section>
            <section aria-labelledby="upstream-source-heading">
              <InspectorSectionHeader
                action={<Badge variant="info">External</Badge>}
                headingId="upstream-source-heading"
                title="Source"
              />
              <DetailList>
                <Detail label="Source" value={UPSTREAM_MODEL_NAME} />
                <Detail label="Upstream model ID" value={config?.upstreamModelId ?? "Unknown"} />
                <Detail label="Base URL" value={config?.endpoint?.baseUrl ?? "Unknown"} />
              </DetailList>
            </section>
            <RequestCards
              client={metricsClient}
              headingId="upstream-requests-heading"
              initialRange={initialRange}
              key={deployment.modelDeploymentId}
              specs={requestSpecs}
            />
          </div>
        }
        footer={
          <InspectorActions
            more={[
              { disabled: pending, icon: Pencil, label: "Edit", onSelect: onEdit },
              {
                destructive: true,
                focusRef: teardownButtonRef,
                icon: Trash2,
                label: "Teardown",
                onSelect: () => setConfirmingTeardown(true),
                pending,
                pendingLabel: "Tearing down…",
              },
            ]}
            primary={chat && { ...chat, disabled: chat.disabled || pending }}
            secondary={connect ? [connect] : []}
          />
        }
        focusKey={`deployment:${deployment.modelDeploymentId}`}
        id={modelInspectorPanelId}
        navigationDisabledReason={
          pending
            ? "An inspector operation is pending. Inspector navigation is unavailable."
            : undefined
        }
        title={
          <ResourceTitle
            colorKey={deployment.modelDeploymentId}
            name={deployment.modelDeploymentName || UPSTREAM_MODEL_NAME}
          />
        }
        open={open}
        onBack={onBack}
        onOpenChange={onOpenChange}
        openerRef={openerRef}
        suspendDismissal={confirmingTeardown}
      />
      <Dialog
        open={confirmingTeardown}
        modal
        disablePointerDismissal
        onOpenChange={(nextOpen) => {
          if (pending) return;
          setConfirmingTeardown(nextOpen);
        }}
      >
        <DialogContent
          aria-describedby="upstream-teardown-description"
          finalFocus={teardownButtonRef}
          initialFocus={cancelTeardownRef}
          showCloseButton={false}
        >
          <DialogHeader>
            <DialogTitle>Teardown {presentationName}?</DialogTitle>
            {teardownError ? (
              <Notice role="alert" tone="destructive">
                {teardownError}
              </Notice>
            ) : null}
            <DialogDescription id="upstream-teardown-description">
              This removes upstream model deployment{" "}
              <ResourceMarker colorKey={deployment.modelDeploymentId} />
              {deployment.modelDeploymentId}.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              disabled={pending}
              ref={cancelTeardownRef}
              type="button"
              variant="outline"
              onClick={() => setConfirmingTeardown(false)}
            >
              Cancel
            </Button>
            <Button
              disabled={pending}
              type="button"
              variant="destructive"
              onClick={() => {
                void onUnload().then((accepted) => {
                  if (accepted) setConfirmingTeardown(false);
                });
              }}
            >
              {pending ? "Tearing down…" : teardownError ? "Try again" : "Teardown"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
