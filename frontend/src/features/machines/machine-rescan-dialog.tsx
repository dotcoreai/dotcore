import { timestampDate } from "@bufbuild/protobuf/wkt";
import { ConnectError } from "@connectrpc/connect";
import { Check, LoaderCircle, RefreshCw } from "lucide-react";
import { type RefObject, useState } from "react";

import { useClients } from "@/api/clients";
import type { SiteRescanClient } from "@/api/site-rescan-client";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { Notice } from "@/components/ui/notice";
import { executionTargetKey } from "@/domain/execution-target";
import { AttemptLog, type AttemptRow } from "@/features/machines/add-machine/attempt-log";
import { CommandTerminal } from "@/features/machines/add-machine/command-terminal";
import { FlowLayout } from "@/features/machines/add-machine/flow-layout";
import { CountdownPill, ExpiredBadge } from "@/features/machines/add-machine/join-countdown";
import { formatClock } from "@/features/machines/add-machine/join-format";
import { Step } from "@/features/machines/add-machine/join-steps";
import type { OciPlatform } from "@/gen/dotcore/catalog/v1/oci_pb";
import type { RescanAwaitingApproval, RescanProposal } from "@/gen/dotcore/site/v1/rescan_pb";
import type { ExecutionTarget } from "@/gen/dotcore/site/v1/site_pb";

import { type MachineRescanSession, useMachineRescan } from "./use-machine-rescan";

/**
 * Replaces one machine's whole execution target set after its hardware or runtimes changed: it
 * starts the re-scan invitation, shows the command to run on that machine, and lets the operator
 * review and allow the replacement it proposes. Closing it ends a re-scan that has not finished.
 *
 * A recorded replacement is only in force once the machine restarts, so the outcome states the
 * restart the machine owes rather than presenting the new targets as running.
 */
export function MachineRescanDialog({
  finalFocus,
  machineId,
  machineName,
  onOpenChange,
  open,
}: {
  /** Where focus lands when the dialog closes: the inspector action that opened the re-scan. */
  finalFocus: RefObject<HTMLElement | null>;
  machineId: string;
  machineName: string;
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        finalFocus={finalFocus}
        className="max-h-[calc(100dvh-2rem)] grid-rows-[auto_minmax(0,1fr)_auto] gap-0 p-0 sm:max-w-[46rem]"
        // Centres the close button on the title row and aligns its icon with the header gutter.
        closeButtonClassName="top-4 right-4 max-sm:top-2.5"
      >
        {/* Mounted only while the dialog is open, so each opening starts its own re-scan. */}
        {open ? (
          <RescanFlow
            machineId={machineId}
            machineName={machineName}
            onClose={() => onOpenChange(false)}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function RescanFlow({
  machineId,
  machineName,
  onClose,
}: {
  machineId: string;
  machineName: string;
  onClose: () => void;
}) {
  const { siteRescanClient } = useClients();
  const { restart, session } = useMachineRescan(siteRescanClient, machineId);
  const { started, state } = session;
  const title = `Re-scan ${machineName}`;
  const expiresAt = started?.expiresAt;
  const status =
    expiresAt && session.startedAt ? (
      <CountdownPill expiresAt={expiresAt} startedAt={session.startedAt} />
    ) : undefined;

  if (state?.state.case === "awaitingApproval" && !session.error) {
    return (
      <ReviewPhase
        // A new proposal starts a fresh decision.
        key={state.state.value.proposalId}
        awaiting={state.state.value}
        client={siteRescanClient}
        machineName={machineName}
        rescanId={started?.rescanId ?? ""}
        session={session}
        title={title}
      />
    );
  }

  if (session.error) {
    // A watch that broke off leaves a re-scan that was started; a start error leaves none.
    return started ? (
      <FlowLayout footer={<Button onClick={restart}>Try again</Button>} title={title}>
        <Step number={1} status="done" title={`Re-scan run on ${machineName}`} />
        <Step number={2} status="warning" title={`${machineName} answers`}>
          <Notice tone="destructive">{session.error}</Notice>
        </Step>
        <Step number={3} status="todo" title="Review the replacement" />
      </FlowLayout>
    ) : (
      <FlowLayout footer={<Button onClick={restart}>Start again</Button>} title={title}>
        <Step number={1} status="warning" title={`Run the re-scan on ${machineName}`}>
          <Notice tone="destructive">{session.error}</Notice>
        </Step>
        <Step number={2} status="todo" title={`${machineName} answers`} />
        <Step number={3} status="todo" title="Review the replacement" />
      </FlowLayout>
    );
  }

  switch (state?.state.case) {
    case "committed": {
      const committed = state.state.value;
      return (
        <FlowLayout
          footer={
            <>
              <Button className="sm:mr-auto" onClick={restart} variant="ghost">
                <RefreshCw aria-hidden="true" />
                Re-scan again
              </Button>
              <Button onClick={onClose}>Done</Button>
            </>
          }
          title={title}
        >
          <Step number={1} status="done" title={`Re-scan run on ${machineName}`} />
          <Step number={2} status="done" title={`${machineName} proposed its replacement`} />
          <Step number={3} status="done" title="Replacement recorded">
            <DetailList className={REVIEW_DETAILS}>
              <Detail label="Recorded" value={<TargetList targets={committed.targets} />} />
            </DetailList>
            {committed.restartRequired ? (
              <Notice tone="warning">
                {machineName} serves its previous targets until it restarts. Restart it to serve
                under the recorded replacement.
              </Notice>
            ) : null}
          </Step>
        </FlowLayout>
      );
    }
    case "noOp":
      return (
        <FlowLayout footer={<Button onClick={onClose}>Done</Button>} title={title}>
          <Step number={1} status="done" title={`Re-scan run on ${machineName}`} />
          <Step number={2} status="done" title={`${machineName} proposed its replacement`} />
          <Step number={3} status="done" title="Nothing to replace">
            <Notice tone="info">
              {machineName} proposed what the site already records: nothing was written and no
              restart is needed.
            </Notice>
          </Step>
        </FlowLayout>
      );
    case "expired":
      return (
        <FlowLayout
          footer={
            <>
              <Button className="sm:mr-auto" onClick={onClose} variant="ghost">
                Close
              </Button>
              <Button onClick={restart}>
                <RefreshCw aria-hidden="true" />
                Get a new command
              </Button>
            </>
          }
          status={<ExpiredBadge />}
          title={title}
        >
          <Step number={1} status="warning" title={`Run the re-scan on ${machineName}`}>
            <CommandTerminal
              command={RESCAN_COMMAND}
              copyLabel="re-scan command"
              dimmed
              title={`on ${machineName}`}
            />
            <Notice tone="destructive">
              {expiresAt
                ? `This command expired at ${formatClock(timestampDate(expiresAt))}.`
                : "This command expired."}{" "}
              A node that runs it now is refused.
            </Notice>
          </Step>
          <Step number={2} status="todo" title={`${machineName} answers`} />
          <Step number={3} status="todo" title="Review the replacement" />
        </FlowLayout>
      );
    case "cancelled":
      return (
        <FlowLayout footer={<Button onClick={restart}>Start again</Button>} title={title}>
          <Step number={1} status="warning" title={`Run the re-scan on ${machineName}`}>
            <CommandTerminal
              command={RESCAN_COMMAND}
              copyLabel="re-scan command"
              dimmed
              title={`on ${machineName}`}
            />
            <Notice tone="warning">The re-scan was cancelled.</Notice>
          </Step>
          <Step number={2} status="todo" title={`${machineName} answers`} />
          <Step number={3} status="todo" title="Review the replacement" />
        </FlowLayout>
      );
    case "failed":
      return (
        <FlowLayout footer={<Button onClick={restart}>Start again</Button>} title={title}>
          <Step number={1} status="done" title={`Re-scan run on ${machineName}`} />
          <Step number={2} status="warning" title={`${machineName} answers`}>
            <Notice tone="destructive">{state.state.value.reason}</Notice>
          </Step>
          <Step number={3} status="todo" title="Review the replacement" />
        </FlowLayout>
      );
    default:
      return (
        <FlowLayout status={status} title={title}>
          <Step number={1} status="active" title={`Run the re-scan on ${machineName}`}>
            {started ? (
              <>
                <CommandTerminal
                  command={RESCAN_COMMAND}
                  copyLabel="re-scan command"
                  title={`on ${machineName}`}
                />
                <p className="text-[12.5px] text-muted-foreground">
                  Run it in a terminal on {machineName}, where its Dotcore folder lives and its
                  server is stopped. It scans that machine and proposes the whole replacement here.
                </p>
              </>
            ) : (
              <p className="flex items-center gap-2 text-muted-foreground" role="status">
                <LoaderCircle
                  aria-hidden="true"
                  className="size-4 animate-spin motion-reduce:animate-none"
                />
                Starting…
              </p>
            )}
          </Step>
          <Step
            number={2}
            status={!started ? "todo" : session.attempts.length > 0 ? "warning" : "active"}
            title={`${machineName} answers`}
          >
            {started ? (
              <AttemptLog
                attempts={rescanAttemptRows(session.attempts)}
                pendingLabel={`Waiting for ${machineName}…`}
              />
            ) : null}
          </Step>
          <Step number={3} status="todo" title="Review the replacement" />
        </FlowLayout>
      );
  }
}

// The proposal under review; keyed by the caller so each proposal starts a fresh decision.
function ReviewPhase({
  awaiting,
  client,
  machineName,
  rescanId,
  session,
  title,
}: {
  awaiting: RescanAwaitingApproval;
  client: SiteRescanClient;
  machineName: string;
  rescanId: string;
  session: MachineRescanSession;
  title: string;
}) {
  const [deciding, setDeciding] = useState(false);
  const [error, setError] = useState<string>();
  const expiresAt = session.started?.expiresAt;

  async function decide(decision: "approveRescan" | "rejectRescan") {
    setDeciding(true);
    setError(undefined);

    try {
      await client[decision]({ proposalId: awaiting.proposalId, rescanId });
      // The watch moves the dialog on; keep the buttons off until it does.
    } catch (failure) {
      setError(ConnectError.from(failure).rawMessage);
      setDeciding(false);
    }
  }

  return (
    <FlowLayout
      footer={
        <>
          <Button
            disabled={deciding}
            onClick={() => void decide("rejectRescan")}
            variant="destructive"
          >
            Reject
          </Button>
          <Button disabled={deciding} onClick={() => void decide("approveRescan")}>
            <Check aria-hidden="true" />
            Approve the replacement
          </Button>
        </>
      }
      status={
        expiresAt && session.startedAt ? (
          <CountdownPill expiresAt={expiresAt} startedAt={session.startedAt} />
        ) : undefined
      }
      title={title}
    >
      <Step number={1} status="done" title={`Re-scan run on ${machineName}`} />
      <Step number={2} status="done" title={`${machineName} proposed its replacement`} />
      <Step number={3} status="active" title="Review the replacement">
        {awaiting.proposal ? <ProposalReview proposal={awaiting.proposal} /> : null}
        {error ? <Notice tone="destructive">{error}</Notice> : null}
      </Step>
    </FlowLayout>
  );
}

/**
 * The replacement as the operator decides on it: every target the re-scan classifies, under the
 * heading of its change, so an empty kind says the replacement leaves that kind alone.
 */
function ProposalReview({ proposal }: { proposal: RescanProposal }) {
  return (
    <DetailList className={REVIEW_DETAILS}>
      <Detail label="Unchanged" value={<TargetList targets={proposal.unchanged} />} />
      <Detail label="Changed" value={<TargetList targets={proposal.changed} />} />
      <Detail label="Added" value={<TargetList targets={proposal.added} />} />
      <Detail label="Removed" value={<TargetList targets={proposal.removed} />} />
    </DetailList>
  );
}

/** One target per badge, named first, then what it runs and where. An empty kind says "None". */
function TargetList({ targets }: { targets: ExecutionTarget[] }) {
  if (targets.length === 0) return "None";

  return (
    <ul className="flex flex-wrap gap-1.5">
      {targets.map((target) => (
        <li key={executionTargetKey(target)}>
          <Badge
            className="h-auto max-w-full rounded-md border-border font-normal whitespace-normal break-all"
            variant="neutral"
          >
            {targetLabel(target)}
          </Badge>
        </li>
      ))}
    </ul>
  );
}

// The name leads, then the facts that tell two targets apart, so a wrapped badge keeps the name.
function targetLabel(target: ExecutionTarget): string {
  const facts = [
    workloadLabel(target),
    runtimeLabel(target),
    platformLabel(target.platform),
    target.acceleratorDeviceIds.join(", "),
  ].filter(Boolean);

  const name = target.displayName || target.executionTargetId;
  return facts.length === 0 ? name : `${name} — ${facts.join(" · ")}`;
}

function workloadLabel(target: ExecutionTarget): string {
  switch (target.runtimeBinding?.case) {
    case "modelRuntime":
      return "Model";
    case "applicationRuntime":
      return "Application";
    default:
      return "Workload";
  }
}
// The adapter names the runtime a target runs its workloads on, whichever workload it carries.
function runtimeLabel(target: ExecutionTarget): string {
  switch (target.runtimeBinding?.value?.adapter?.case) {
    case "dockerRuntime":
      return "Docker";
    case "nativeRuntime":
      return "Native";
    case "simulatedRuntime":
      return "Simulated";
    default:
      return "";
  }
}

function platformLabel(platform: OciPlatform | undefined): string {
  if (!platform) return "";
  const base = `${platform.operatingSystem}/${platform.architecture}`;
  return platform.variant ? `${base}/${platform.variant}` : base;
}

// The node's answers that ended without a replacement, in the words the operator sees for each.
function rescanAttemptRows(attempts: MachineRescanSession["attempts"]): AttemptRow[] {
  return attempts.map((attempt) => ({ at: attempt.at, text: attemptText(attempt) }));
}

function attemptText(attempt: MachineRescanSession["attempts"][number]): string {
  switch (attempt.outcome.case) {
    case "failed":
      return attempt.outcome.value;
    case "rejected":
      return "You refused this replacement.";
    default:
      return "The answer ended.";
  }
}

const REVIEW_DETAILS =
  "mt-0 grid-cols-[7.5rem_minmax(0,1fr)] gap-x-4 gap-y-2.5 max-sm:grid-cols-1 max-sm:gap-y-0.5 " +
  "max-sm:[&_dd]:mb-2 [&_dd]:text-left [&_dd]:font-normal [&_dt]:text-[13px] [&_dd]:text-[13px]";

/** The command that re-scans the node, run where its Dotcore folder lives. */
const RESCAN_COMMAND = "dotc node rescan";
