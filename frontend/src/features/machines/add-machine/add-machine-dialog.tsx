import { timestampDate } from "@bufbuild/protobuf/wkt";
import { ConnectError } from "@connectrpc/connect";
import { Check, LoaderCircle, Plus, RefreshCw } from "lucide-react";
import { type ReactNode, useState } from "react";

import { useClients } from "@/api/clients";
import type { SiteMembershipClient } from "@/api/site-membership-client";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Notice } from "@/components/ui/notice";
import type { JoinAwaitingApproval } from "@/gen/dotcore/site/v1/membership_pb";

import { ApproveStep } from "./approve-step";
import { AttemptLog } from "./attempt-log";
import { CommandTerminal } from "./command-terminal";
import { CountdownPill, ExpiredBadge } from "./join-countdown";
import { formatClock, peerHost } from "./join-format";
import { Step, StepList } from "./join-steps";
import { type JoinSession, useJoinSession } from "./use-join-session";

/**
 * Adds a machine to the running site: starts a join, shows the command to run on the new machine,
 * and lets the operator review and allow the machine that answers, step by step. Closing it ends a
 * join that has not finished.
 */
export function AddMachineDialog({
  onOpenChange,
  open,
}: {
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="max-h-[calc(100dvh-2rem)] grid-rows-[auto_minmax(0,1fr)_auto] gap-0 p-0 sm:max-w-[46rem]"
        // Centres the close button on the title row and aligns its icon with the header gutter.
        closeButtonClassName="top-4 right-4 max-sm:top-2.5"
      >
        <JoinFlow onClose={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}

// Mounted only while the dialog is open, so each opening starts its own join.
function JoinFlow({ onClose }: { onClose: () => void }) {
  const { siteMembershipClient } = useClients();
  const { restart, session } = useJoinSession(siteMembershipClient);
  const { started, state } = session;

  if (state?.state.case === "awaitingApproval" && !session.error) {
    return (
      <ReviewPhase
        // A new proposal starts a fresh decision.
        key={state.state.value.proposalId}
        awaiting={state.state.value}
        client={siteMembershipClient}
        joinId={started?.joinId ?? ""}
        session={session}
      />
    );
  }

  const name = session.proposal?.displayName ?? "the new machine";
  const expiresAt = started?.expiresAt;

  if (session.error) {
    // A watch that broke off leaves a join that was started; a start error leaves none.
    return started ? (
      <JoinLayout footer={<Button onClick={restart}>Try again</Button>}>
        <Step number={1} status="done" title={`Command run on ${name}`} />
        <Step number={2} status="warning" title="The machine connects">
          <Notice tone="destructive">{session.error}</Notice>
        </Step>
        <Step number={3} status="todo" title="Approve the machine" />
      </JoinLayout>
    ) : (
      <JoinLayout footer={<Button onClick={restart}>Start again</Button>}>
        <Step number={1} status="warning" title="Run this on the new machine">
          <Notice tone="destructive">{session.error}</Notice>
        </Step>
        <Step number={2} status="todo" title="The machine connects" />
        <Step number={3} status="todo" title="Approve the machine" />
      </JoinLayout>
    );
  }

  switch (state?.state.case) {
    case "committed":
      return (
        <JoinLayout
          footer={
            <>
              <Button className="sm:mr-auto" onClick={restart} variant="ghost">
                <Plus aria-hidden="true" />
                Add another
              </Button>
              <Button onClick={onClose}>Done</Button>
            </>
          }
        >
          <Step number={1} status="done" title={`Command run on ${name}`} />
          <Step number={2} status="done" title={connectedTitle(session)} />
          <Step
            number={3}
            status="done"
            title={`${session.proposal?.displayName ?? "The machine"} added to the site`}
          />
        </JoinLayout>
      );
    case "expired":
      return (
        <JoinLayout
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
        >
          <Step number={1} status="warning" title="Run this on the new machine">
            <CommandTerminal command={joinCommand(session)} dimmed />
            <Notice tone="destructive">
              {expiresAt
                ? `This command expired at ${formatClock(timestampDate(expiresAt))}.`
                : "This command expired."}{" "}
              A machine that runs it now is refused.
            </Notice>
          </Step>
          <Step number={2} status="todo" title="The machine connects" />
          <Step number={3} status="todo" title="Approve the machine" />
        </JoinLayout>
      );
    case "cancelled":
      return (
        <JoinLayout footer={<Button onClick={restart}>Start again</Button>}>
          <Step number={1} status="warning" title="Run this on the new machine">
            <CommandTerminal command={joinCommand(session)} dimmed />
            <Notice tone="warning">The join was cancelled.</Notice>
          </Step>
          <Step number={2} status="todo" title="The machine connects" />
          <Step number={3} status="todo" title="Approve the machine" />
        </JoinLayout>
      );
    case "failed":
      return (
        <JoinLayout footer={<Button onClick={restart}>Start again</Button>}>
          <Step number={1} status="done" title={`Command run on ${name}`} />
          <Step number={2} status="warning" title="The machine connects">
            <Notice tone="destructive">{state.state.value.reason}</Notice>
          </Step>
          <Step number={3} status="todo" title="Approve the machine" />
        </JoinLayout>
      );
    default:
      return (
        <JoinLayout
          status={
            expiresAt && session.startedAt ? (
              <CountdownPill expiresAt={expiresAt} startedAt={session.startedAt} />
            ) : undefined
          }
        >
          <Step number={1} status="active" title="Run this on the new machine">
            {started ? (
              <>
                <CommandTerminal command={joinCommand(session)} />
                <p className="text-[12.5px] text-muted-foreground">Needs dotc on that machine.</p>
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
            title="The machine connects"
          >
            {started ? (
              <AttemptLog attempts={session.attempts} machineName={session.proposal?.displayName} />
            ) : null}
          </Step>
          <Step number={3} status="todo" title="Approve the machine" />
        </JoinLayout>
      );
  }
}

// The proposal under review; keyed by the caller so each proposal starts a fresh decision.
function ReviewPhase({
  awaiting,
  client,
  joinId,
  session,
}: {
  awaiting: JoinAwaitingApproval;
  client: SiteMembershipClient;
  joinId: string;
  session: JoinSession;
}) {
  const [deciding, setDeciding] = useState(false);
  const [error, setError] = useState<string>();
  const { proposal } = awaiting;
  const name = proposal?.displayName ?? "the machine";
  const { expiresAt } = session.started ?? {};

  async function decide(decision: "approveJoin" | "rejectJoin") {
    setDeciding(true);
    setError(undefined);

    try {
      await client[decision]({ joinId, proposalId: awaiting.proposalId });
      // The watch moves the dialog on; keep the buttons off until it does.
    } catch (failure) {
      setError(ConnectError.from(failure).rawMessage);
      setDeciding(false);
    }
  }

  return (
    <JoinLayout
      footer={
        <>
          <Button
            disabled={deciding}
            onClick={() => void decide("rejectJoin")}
            variant="destructive"
          >
            Reject
          </Button>
          <Button disabled={deciding} onClick={() => void decide("approveJoin")}>
            <Check aria-hidden="true" />
            Approve {name}
          </Button>
        </>
      }
      status={
        expiresAt && session.startedAt ? (
          <CountdownPill expiresAt={expiresAt} startedAt={session.startedAt} />
        ) : undefined
      }
    >
      <Step number={1} status="done" title={`Command run on ${name}`} />
      <Step number={2} status="done" title={connectedTitle(session)} />
      <Step number={3} status="active" title={`Approve ${name}`}>
        {proposal ? <ApproveStep error={error} proposal={proposal} /> : null}
      </Step>
    </JoinLayout>
  );
}

// The dialog frame: title, an optional status next to the close button, the steps and the actions.
function JoinLayout({
  children,
  footer,
  status,
}: {
  children: ReactNode;
  footer?: ReactNode;
  status?: ReactNode;
}) {
  return (
    <>
      <DialogHeader className="flex-row items-center gap-3 border-b px-[1.6rem] py-[1.1rem] pr-16 max-sm:px-5 max-sm:pr-16 sm:pr-14">
        <DialogTitle className="mr-auto text-[17px] leading-6 font-semibold whitespace-nowrap">
          Add a machine
        </DialogTitle>
        {status}
      </DialogHeader>
      <div className="overflow-y-auto px-[1.6rem] py-6">
        <StepList>{children}</StepList>
      </div>
      {footer ? <DialogFooter className="m-0 px-[1.6rem] py-4">{footer}</DialogFooter> : null}
    </>
  );
}

function joinCommand(session: JoinSession): string {
  return `dotc node join --token ${session.started?.token ?? ""}`;
}

function connectedTitle(session: JoinSession): string {
  const { proposal } = session;
  const name = proposal?.displayName ?? "The machine";
  return proposal?.peerAddress
    ? `${name} connected from ${peerHost(proposal.peerAddress)}`
    : `${name} connected`;
}
