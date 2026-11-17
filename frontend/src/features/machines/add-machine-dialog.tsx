import { timestampDate } from "@bufbuild/protobuf/wkt";
import { ConnectError } from "@connectrpc/connect";
import { LoaderCircle } from "lucide-react";
import { type ReactNode, useState } from "react";

import { useClients } from "@/api/clients";
import type { SiteJoinClient } from "@/api/site-join-client";
import { Button } from "@/components/ui/button";
import { CodeCopyControl } from "@/components/ui/copy-control";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Notice } from "@/components/ui/notice";
import type { JoinAwaitingApproval } from "@/gen/dotcore/site/v1/join_pb";

import { JoinProposalDetails } from "./join-proposal-details";
import { type JoinSession, useJoinSession } from "./use-join-session";

/**
 * Adds a machine to the running site: starts a join, shows the command to run on the new machine,
 * and lets the operator review and allow the machine that answers. Closing it ends a join that has
 * not finished.
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
        className="gap-0 p-0 sm:max-w-md"
        // Centres the close button on the title row and aligns its icon with the header gutter.
        closeButtonClassName="top-3.5 right-3.5 max-sm:top-1.5"
      >
        <DialogHeader className="border-b px-5 py-4 pr-16 sm:pr-12">
          <DialogTitle className="text-[17px] leading-6 font-semibold">Add a machine</DialogTitle>
        </DialogHeader>
        <JoinFlow onDone={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}

// Mounted only while the dialog is open, so each opening starts its own join.
function JoinFlow({ onDone }: { onDone: () => void }) {
  const { siteJoinClient } = useClients();
  const { restart, session } = useJoinSession(siteJoinClient);
  const { error, proposalName, started, state } = session;

  if (error) {
    return (
      <JoinMessage
        action={<Button onClick={restart}>Try again</Button>}
        notice={<Notice tone="destructive">{error}</Notice>}
      />
    );
  }

  switch (state?.state.case) {
    case "awaitingApproval":
      return (
        <ProposalReview
          // A new proposal starts a fresh decision.
          key={state.state.value.proposalId}
          awaiting={state.state.value}
          joinId={started?.joinId ?? ""}
          client={siteJoinClient}
        />
      );
    case "committed":
      return (
        <JoinMessage
          action={<Button onClick={onDone}>Done</Button>}
          notice={
            <Notice>
              {proposalName ? `${proposalName} was added.` : "The machine was added."}
            </Notice>
          }
        />
      );
    case "expired":
      return (
        <JoinMessage
          action={<Button onClick={restart}>New command</Button>}
          notice={<Notice tone="warning">The command expired.</Notice>}
        />
      );
    case "cancelled":
      return (
        <JoinMessage
          action={<Button onClick={restart}>Start again</Button>}
          notice={<Notice tone="warning">The join was cancelled.</Notice>}
        />
      );
    case "failed":
      return (
        <JoinMessage
          action={<Button onClick={restart}>Start again</Button>}
          notice={<Notice tone="destructive">{state.state.value.reason}</Notice>}
        />
      );
    default:
      return <WaitingForMachine session={session} />;
  }
}

function WaitingForMachine({ session }: { session: JoinSession }) {
  const { started, state } = session;
  const command = started ? `dotc node join --token ${started.token}` : undefined;
  const lastAttempt =
    state?.state.case === "waiting" ? state.state.value.lastAttempt?.outcome : undefined;

  return (
    <div className="flex flex-col gap-3 px-5 py-4 text-sm">
      {command ? (
        <>
          <div>
            Run on the new machine:
            <div className="mt-2 flex items-start justify-between gap-2 rounded-md border border-border bg-muted/50 py-1.5 pr-1.5 pl-3">
              <code className="min-w-0 py-1 font-mono break-all">{command}</code>
              <CodeCopyControl label="join command" value={command} />
            </div>
            {started?.expiresAt ? (
              <p className="mt-2 text-xs text-muted-foreground">
                Expires at {formatTime(timestampDate(started.expiresAt))}
              </p>
            ) : null}
          </div>
          {lastAttempt?.case === "rejected" ? (
            <Notice tone="warning">The last attempt was rejected.</Notice>
          ) : null}
          {lastAttempt?.case === "failed" ? (
            <Notice tone="warning">Last attempt failed: {lastAttempt.value}</Notice>
          ) : null}
        </>
      ) : null}
      <p className="flex items-center gap-2 text-muted-foreground" role="status">
        <LoaderCircle
          aria-hidden="true"
          className="size-4 animate-spin motion-reduce:animate-none"
        />
        {command ? "Waiting for the machine" : "Starting"}
      </p>
    </div>
  );
}

function ProposalReview({
  awaiting,
  client,
  joinId,
}: {
  awaiting: JoinAwaitingApproval;
  client: SiteJoinClient;
  joinId: string;
}) {
  const [deciding, setDeciding] = useState(false);
  const [error, setError] = useState<string>();

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
    <>
      <div className="px-5 pt-1 pb-4 text-sm">
        {awaiting.proposal ? <JoinProposalDetails proposal={awaiting.proposal} /> : null}
        {error ? (
          <Notice className="mt-3" tone="destructive">
            {error}
          </Notice>
        ) : null}
      </div>
      <DialogFooter className="m-0 px-5 py-3">
        <Button disabled={deciding} onClick={() => void decide("rejectJoin")} variant="outline">
          Reject
        </Button>
        <Button disabled={deciding} onClick={() => void decide("approveJoin")}>
          Approve
        </Button>
      </DialogFooter>
    </>
  );
}

function JoinMessage({ action, notice }: { action: ReactNode; notice: ReactNode }) {
  return (
    <>
      <div className="px-5 py-4 text-sm">{notice}</div>
      <DialogFooter className="m-0 px-5 py-3">{action}</DialogFooter>
    </>
  );
}

function formatTime(date: Date): string {
  return date.toLocaleTimeString([], { hour: "2-digit", hourCycle: "h23", minute: "2-digit" });
}
