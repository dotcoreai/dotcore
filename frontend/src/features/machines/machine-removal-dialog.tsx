import { type RefObject, useCallback, useEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import type { SiteMembershipClient } from "@/api/site-membership-client";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker } from "@/components/ui/resource-marker";
import {
  type MachineRemovalDeployment,
  MachineRemovalDeploymentKind,
  type MachineRemovalPlan,
  type MachineRemovalRefusal,
  MachineRemovalRefusalReason,
  type RemoveMachineResponse,
} from "@/gen/dotcore/site/v1/membership_pb";
import { formatCount } from "@/lib/format";
import { PanelLink } from "@/panels/panel-navigation";
import { appendPanelHref, type PanelEntry } from "@/panels/panel-stack";

/**
 * The confirmation for removing a worker machine from the site. Its content comes from the
 * removal preview, so it names exactly what the removal changes: the deployments that block it,
 * or the deployments and routes a forced removal of a disconnected machine drops.
 */
export function MachineRemovalDialog({
  client,
  disconnected,
  finalFocus,
  machineId,
  machineName,
  onOpenChange,
  onRemoved,
  open,
}: {
  client: SiteMembershipClient;
  /** Whether the manager sees the machine disconnected, which allows forgetting its deployments. */
  disconnected: boolean;
  finalFocus: RefObject<HTMLElement | null>;
  machineId: string;
  machineName: string;
  onOpenChange: (open: boolean) => void;
  /** Called once the machine is gone from the site. */
  onRemoved: () => void;
  open: boolean;
}) {
  return (
    <Dialog modal disablePointerDismissal onOpenChange={onOpenChange} open={open}>
      <DialogContent finalFocus={finalFocus} showCloseButton={false}>
        <RemovalFlow
          client={client}
          disconnected={disconnected}
          machineId={machineId}
          machineName={machineName}
          onClose={() => onOpenChange(false)}
          onRemoved={onRemoved}
        />
      </DialogContent>
    </Dialog>
  );
}

type RemovalStep =
  | { kind: "loading" }
  | { forget: boolean; kind: "confirm"; plan: MachineRemovalPlan }
  | { kind: "blocked"; refusal: MachineRemovalRefusal }
  | { kind: "failed"; message: string };

/** The dialog body. It mounts with the open dialog, so every opening previews afresh. */
function RemovalFlow({
  client,
  disconnected,
  machineId,
  machineName,
  onClose,
  onRemoved,
}: {
  client: SiteMembershipClient;
  disconnected: boolean;
  machineId: string;
  machineName: string;
  onClose: () => void;
  onRemoved: () => void;
}) {
  const [step, setStep] = useState<RemovalStep>({ kind: "loading" });
  const [removing, setRemoving] = useState(false);
  const latestRequest = useRef(0);

  // Applies a call's result unless a newer call or the dialog closing has replaced it.
  const settle = useCallback(
    (request: number, result: Promise<RemovalStep | "removed">) => {
      void result.then((next) => {
        if (request !== latestRequest.current) return;
        setRemoving(false);
        if (next === "removed") onRemoved();
        else setStep(next);
      });
    },
    [onRemoved],
  );

  // Shows the pending state, then runs a preview or the removal itself.
  const start = (forget: boolean, validateOnly: boolean) => {
    setRemoving(!validateOnly);
    if (validateOnly) setStep({ kind: "loading" });
    settle(++latestRequest.current, callRemoval(client, machineId, forget, validateOnly));
  };

  useEffect(() => {
    settle(++latestRequest.current, callRemoval(client, machineId, false, true));
    return () => {
      latestRequest.current += 1;
    };
  }, [client, machineId, settle]);

  const cancel = (
    <DialogClose
      render={
        <Button type="button" variant="outline">
          {step.kind === "blocked" || step.kind === "failed" ? "Close" : "Cancel"}
        </Button>
      }
    />
  );

  if (step.kind === "confirm") {
    const forgotten = step.plan.forgottenDeployments;
    const label = step.forget
      ? `Remove and forget ${formatCount(forgotten.length, "deployment")}`
      : "Remove";

    return (
      <>
        <DialogHeader>
          <DialogTitle>
            {step.forget
              ? `Remove ${machineName} and forget its deployments?`
              : `Remove ${machineName}?`}
          </DialogTitle>
          <DialogDescription>
            <ResourceMarker colorKey={machineId} />
            {machineName} is told to stop and leaves the site. Run <code>dotc node leave</code> on
            it to clean its folder.
          </DialogDescription>
        </DialogHeader>
        {step.forget ? <ForgottenDetails plan={step.plan} /> : null}
        <DialogFooter>
          {cancel}
          <Button
            disabled={removing}
            onClick={() => start(step.forget, false)}
            type="button"
            variant="destructive"
          >
            {removing ? "Removing…" : label}
          </Button>
        </DialogFooter>
      </>
    );
  }

  if (step.kind === "blocked") {
    const deployments = step.refusal.deployments;
    const placed =
      step.refusal.reason === MachineRemovalRefusalReason.DEPLOYMENTS_PLACED &&
      deployments.length > 0;
    const forgettable = placed && disconnected;

    return (
      <>
        <DialogHeader>
          <DialogTitle>
            {placed ? `${machineName} has deployments` : "Can't remove machine"}
          </DialogTitle>
          <DialogDescription>
            {placed
              ? "Remove its deployments first, then remove the machine."
              : step.refusal.message}
          </DialogDescription>
        </DialogHeader>
        {deployments.length > 0 ? (
          <DeploymentList deployments={deployments} linked onNavigate={onClose} />
        ) : null}
        {step.refusal.sharedRouteNames.length > 0 ? (
          <NameList label="Routes that stay" names={step.refusal.sharedRouteNames} />
        ) : null}
        {step.refusal.linkedApplicationNames.length > 0 ? (
          <NameList
            label="Applications bound to them"
            names={step.refusal.linkedApplicationNames}
          />
        ) : null}
        {forgettable ? (
          <p className="text-[12.5px] text-muted-foreground">
            The machine is disconnected, so its deployments can be forgotten instead.
          </p>
        ) : null}
        <DialogFooter>
          {cancel}
          {forgettable ? (
            <Button onClick={() => start(true, true)} type="button" variant="destructive">
              Remove and forget {formatCount(deployments.length, "deployment")}…
            </Button>
          ) : null}
        </DialogFooter>
      </>
    );
  }

  if (step.kind === "failed") {
    return (
      <>
        <DialogHeader>
          <DialogTitle>Can&apos;t remove machine</DialogTitle>
          <DialogDescription>{step.message}</DialogDescription>
        </DialogHeader>
        <DialogFooter>{cancel}</DialogFooter>
      </>
    );
  }

  return (
    <>
      <DialogHeader>
        <DialogTitle>Remove machine?</DialogTitle>
        <DialogDescription>Checking what the removal changes…</DialogDescription>
      </DialogHeader>
      <DialogFooter>{cancel}</DialogFooter>
    </>
  );
}

/** What a forced removal drops: the deployments withdrawn without cleanup and the routes deleted. */
function ForgottenDetails({ plan }: { plan: MachineRemovalPlan }) {
  return (
    <div className="flex flex-col gap-3 text-[12.5px]">
      <Notice tone="warning">
        These deployments are withdrawn without cleanup and may keep running on the machine until it
        is cleaned.
      </Notice>
      <DeploymentList deployments={plan.forgottenDeployments} />
      {plan.deletedRoutes.length > 0 ? (
        <NameList
          label="Routes deleted with them"
          names={plan.deletedRoutes.map((route) => route.name)}
        />
      ) : null}
    </div>
  );
}

/**
 * Deployments of a removal, each marked as tearing down when it is. With `linked` each one links
 * to its inspector, and following a link closes the dialog through `onNavigate`.
 */
function DeploymentList({
  deployments,
  linked = false,
  onNavigate,
}: {
  deployments: readonly MachineRemovalDeployment[];
  linked?: boolean;
  onNavigate?: () => void;
}) {
  const location = useLocation();

  return (
    <ul aria-label="Deployments" className="flex flex-col gap-1 text-[12.5px]">
      {deployments.map((deployment) => {
        const label = (
          <>
            <ResourceMarker colorKey={deployment.deploymentId} />
            {deployment.name || deployment.deploymentId}
          </>
        );

        return (
          <li key={`${deployment.kind}:${deployment.deploymentId}`}>
            {linked ? (
              <PanelLink
                className="text-primary underline-offset-4 hover:underline"
                onClick={onNavigate}
                to={appendPanelHref(location, deploymentEntry(deployment))}
              >
                {label}
              </PanelLink>
            ) : (
              label
            )}
            {deployment.tearingDown ? (
              <span className="text-muted-foreground"> · Tearing down</span>
            ) : null}
          </li>
        );
      })}
    </ul>
  );
}

function NameList({ label, names }: { label: string; names: readonly string[] }) {
  return (
    <p className="text-[12.5px]">
      <span className="text-muted-foreground">{label}: </span>
      {names.join(", ")}
    </p>
  );
}

function deploymentEntry(deployment: MachineRemovalDeployment): PanelEntry {
  return {
    page: deployment.kind === MachineRemovalDeploymentKind.APPLICATION ? "apps" : "models",
    resourceId: deployment.deploymentId,
    screen: "deployment",
  };
}

/** Runs one removal call and reduces its outcome to the dialog step it leads to. */
async function callRemoval(
  client: SiteMembershipClient,
  machineId: string,
  forget: boolean,
  validateOnly: boolean,
): Promise<RemovalStep | "removed"> {
  let response: RemoveMachineResponse;
  try {
    response = await client.removeMachine({
      forgetDeployments: forget,
      nodeId: machineId,
      validateOnly,
    });
  } catch (error) {
    return {
      kind: "failed",
      message: error instanceof Error ? error.message : "The removal request failed.",
    };
  }

  const outcome = response.outcome;
  if (outcome.case === "refused") return { kind: "blocked", refusal: outcome.value };
  if (outcome.case === "wouldRemove" && validateOnly) {
    return { forget, kind: "confirm", plan: outcome.value };
  }

  return "removed";
}
