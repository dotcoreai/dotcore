import { Pause, Pencil, Play, Trash2 } from "lucide-react";
import type { ReactNode, RefObject } from "react";
import { useRef, useState } from "react";

import type { DeploymentLogOperations } from "@/api/deployment-log-client";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import {
  deploymentStatus,
  isPausableModel,
  isResumableModel,
  isTransitioningModelStatus,
  isUnloadableModel,
  localModelSnapshot,
  modelDeploymentPresentationName,
} from "@/domain/model-projection";
import type { ChatTarget, ConnectTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import type { RouteMention } from "@/features/deployments/panel-mention";
import { deploymentConnectAction } from "@/features/models/deployment-connect-action";
import { type Model } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { type ConfiguredExecutionTarget, type Machine } from "@/gen/dotcore/site/v1/service_pb";

import { DeploymentOverview } from "./deployment-overview";
import { modelInspectorPanelId } from "./model-inspector-panel";
import { ResolvedDeploymentDetails } from "./resolved-deployment-details";

/**
 * The local model deployment panel: its overview and its resolved configuration screen, with
 * Chat or Resume as the primary action, Edit, Pause and Teardown in the menu, and the teardown
 * confirmation.
 */
export function DeploymentModelInspector({
  catalogModel,
  chatTarget,
  connectTarget,
  catalogModelHref,
  containingRoutes,
  deployment,
  error,
  executionTargets,
  teardownError,
  floatingLayer,
  logOpenerRef,
  logs,
  onBack,
  onConnect,
  onOpenConfiguration,
  onOpenLogs,
  onPause,
  onResume,
  onEdit,
  onOpenChange,
  onUnload,
  open,
  openerRef,
  pendingAction,
  memory,
  requests,
  pending,
  placementMachine,
  placementMachineHref,
  screen = "overview",
  suspendDismissal = false,
}: DeploymentModelInspectorProps) {
  const [confirmingTeardown, setConfirmingTeardown] = useState(false);
  const cancelTeardownRef = useRef<HTMLButtonElement>(null);
  const teardownButtonRef = useRef<HTMLElement>(null);
  const model = localModelSnapshot(deployment.snapshot);
  const status = deploymentStatus(deployment);
  const actionDisabled = pending || isTransitioningModelStatus(status);
  const pauseDisabled = pending || isPauseConflictingStatus(status);
  const chat = useChatAction(chatTarget);
  const connect = deploymentConnectAction(connectTarget, onConnect, pending);
  const presentationName = modelDeploymentPresentationName(deployment);
  const screenTitle = deploymentScreenTitle(screen);

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
            {screen === "overview" ? (
              <DeploymentOverview
                catalogModel={catalogModel}
                catalogModelHref={catalogModelHref}
                containingRoutes={containingRoutes}
                deployment={deployment}
                executionTargets={executionTargets}
                logOpenerRef={logOpenerRef}
                onOpenConfiguration={onOpenConfiguration}
                memory={memory}
                requests={requests}
                onOpenLogs={onOpenLogs}
                placementMachine={placementMachine}
                placementMachineHref={placementMachineHref}
                showLogs={Boolean(logs)}
              />
            ) : null}
            {screen === "configuration" && model ? (
              <ResolvedDeploymentDetails snapshot={model} />
            ) : null}
          </div>
        }
        floatingLayer={floatingLayer}
        footer={
          <InspectorActions
            more={[
              {
                disabled:
                  deployment.desiredState === ModelDeploymentDesiredState.TEARDOWN || pending,
                icon: Pencil,
                label: "Edit",
                onSelect: () => onEdit?.(),
              },
              ...(isPausableModel(deployment)
                ? [
                    {
                      disabled: pauseDisabled,
                      icon: Pause,
                      label: "Pause",
                      onSelect: () => onPause?.(),
                      pending: pendingAction === "pause",
                      pendingLabel: "Pausing…",
                    },
                  ]
                : []),
              {
                destructive: true,
                disabled: !isUnloadableModel(deployment) || actionDisabled,
                focusRef: teardownButtonRef,
                icon: Trash2,
                label: "Teardown",
                onSelect: () => setConfirmingTeardown(true),
                pending: pendingAction === "unload" || (pending && !pendingAction),
                pendingLabel: "Tearing down…",
              },
            ]}
            primary={
              isResumableModel(deployment)
                ? {
                    disabled: actionDisabled,
                    icon: Play,
                    label: "Resume",
                    onSelect: () => onResume?.(),
                    pending: pendingAction === "resume",
                    pendingLabel: "Resuming…",
                  }
                : chat && { ...chat, disabled: chat.disabled || pending }
            }
            secondary={connect ? [connect] : []}
          />
        }
        focusKey={`${screen}:${deployment.modelDeploymentId}`}
        id={modelInspectorPanelId}
        navigationDisabledReason={
          pending
            ? "An inspector operation is pending. Inspector navigation is unavailable."
            : undefined
        }
        onBack={onBack}
        onOpenChange={onOpenChange}
        open={open}
        openerRef={openerRef}
        title={
          <ResourceTitle
            colorKey={deployment.modelDeploymentId}
            name={presentationName}
            screen={screenTitle}
          />
        }
        suspendDismissal={confirmingTeardown || suspendDismissal}
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
          aria-describedby="teardown-deployment-description"
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
            <DialogDescription id="teardown-deployment-description">
              This removes model deployment{" "}
              <ResourceMarker colorKey={deployment.modelDeploymentId} />
              {deployment.modelDeploymentId} and its running workload resources.
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
              onClick={() => {
                void onUnload().then((accepted) => {
                  if (accepted) setConfirmingTeardown(false);
                });
              }}
              type="button"
              variant="destructive"
            >
              {pending ? "Tearing down…" : teardownError ? "Try again" : "Teardown"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

export type DeploymentModelInspectorScreen = "overview" | "configuration";

interface DeploymentModelInspectorProps {
  catalogModelHref?: string;
  /** The deployment's catalog model, which names its speculative options; undefined until read. */
  catalogModel?: Model;
  /** Where the Chat action takes the operator; computed from the routes listing the deployment. */
  chatTarget?: ChatTarget;
  /** Where the Connect action takes the operator; computed from the routes listing the deployment. */
  connectTarget?: ConnectTarget;
  /** Opens the connect page of the target route. */
  onConnect?: (routeId: string) => void;
  containingRoutes?: readonly RouteMention[];
  deployment: ModelDeployment;
  error?: string;
  /** The site's configured execution targets, undefined until read; they name the placement. */
  executionTargets?: readonly ConfiguredExecutionTarget[];
  teardownError?: string;
  floatingLayer?: ReactNode;
  logOpenerRef?: RefObject<HTMLButtonElement | null>;
  /** The deployment's memory row, shown in its Deployment section, supplied by the page that reads the site's memory. */
  memory?: ReactNode;
  /** The deployment's Requests section, supplied by the page that reads metrics. */
  requests?: ReactNode;
  logs?: DeploymentLogOperations;
  onBack?: () => void;
  onOpenChange: (open: boolean) => void;
  onOpenConfiguration?: () => void;
  onOpenLogs?: () => void;
  onPause?: () => void;
  onResume?: () => void;
  onEdit?: () => void;
  onUnload: () => Promise<boolean>;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
  pendingAction?: "pause" | "resume" | "unload";
  pending: boolean;
  /** The machine the deployment is placed on, when the site lists it. */
  placementMachine?: Machine;
  /** Link to the placement machine's inspector, when the site lists that machine. */
  placementMachineHref?: string;
  screen?: DeploymentModelInspectorScreen;
  suspendDismissal?: boolean;
}

function deploymentScreenTitle(screen: DeploymentModelInspectorScreen): string | undefined {
  if (screen === "configuration") return "Configuration";
  return undefined;
}

function isPauseConflictingStatus(state: ModelDeploymentStatus): boolean {
  return (
    state === ModelDeploymentStatus.UNLOADING ||
    state === ModelDeploymentStatus.PAUSING ||
    state === ModelDeploymentStatus.PAUSED
  );
}
