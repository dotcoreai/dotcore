import { Badge } from "@/components/ui/badge";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { Notice } from "@/components/ui/notice";
import { workloadKindLabel } from "@/domain/execution-target";
import {
  type JoinProposal,
  type JoinProposedTarget,
  JoinTargetRuntime,
} from "@/gen/dotcore/site/v1/membership_pb";

import { FingerprintPatternView, GroupedFingerprint } from "./fingerprint-pattern";
import { peerHost } from "./join-format";

/**
 * What the operator checks before allowing a machine to join: the fingerprint to confirm against
 * the machine's own screen, then where it is, what it runs and what changes for a machine that
 * joins again.
 */
export function ApproveStep({ error, proposal }: { error?: string; proposal: JoinProposal }) {
  const { kind } = proposal;

  return (
    <>
      <div className="flex items-center gap-4 rounded-xl border border-dashed border-input p-4 max-sm:flex-col max-sm:items-start">
        {proposal.fingerprintPattern ? (
          <FingerprintPatternView pattern={proposal.fingerprintPattern} />
        ) : null}
        <div className="flex min-w-0 flex-col gap-2">
          <b className="text-[12.5px] font-semibold">Confirm {proposal.displayName} fingerprint</b>
          <GroupedFingerprint fingerprint={proposal.keyFingerprint} />
        </div>
      </div>
      <DetailList className="mt-0 grid-cols-[7.5rem_minmax(0,1fr)] gap-x-4 gap-y-2.5 max-sm:grid-cols-1 max-sm:gap-y-0.5 max-sm:[&_dd]:mb-2 [&_dd]:text-left [&_dd]:font-normal [&_dt]:text-[13px] [&_dd]:text-[13px]">
        <Detail label="Address" value={`${proposal.host}:${proposal.privatePort}`} />
        {kind.case === "newNode" ? (
          <Detail label="Runs" value={<Targets targets={kind.value.targets} />} />
        ) : null}
        {kind.case === "rejoin" ? (
          <>
            <Detail label="Joining" value="Existing machine again" />
            <Detail label="Previous address" value={kind.value.previousHost} />
            {kind.value.managerHost !== kind.value.previousManagerHost ? (
              <Detail
                label="Manager address"
                value={`${kind.value.previousManagerHost || "declared"} to ${kind.value.managerHost || "declared"}`}
              />
            ) : null}
            <Detail label="Deployments kept" value={kind.value.deployments} />
          </>
        ) : null}
      </DetailList>
      {proposal.hostDiffersFromPeer ? (
        <Notice tone="warning">
          Connected from {peerHost(proposal.peerAddress)}, which is not the address it gave.
        </Notice>
      ) : null}
      {error ? <Notice tone="destructive">{error}</Notice> : null}
    </>
  );
}

function Targets({ targets }: { targets: JoinProposedTarget[] }) {
  if (targets.length === 0) return "No workloads";

  return (
    <ul className="flex flex-wrap gap-1.5">
      {targets.map((target, index) => (
        <li key={index}>
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

function targetLabel(target: JoinProposedTarget): string {
  return [
    workloadKindLabel(target.workloadKind),
    runtimeLabel(target.runtime),
    target.platformLabel,
    ...target.acceleratorDeviceIds,
  ]
    .filter(Boolean)
    .join(" · ");
}

function runtimeLabel(runtime: JoinTargetRuntime): string {
  switch (runtime) {
    case JoinTargetRuntime.SIMULATED:
      return "Simulated";
    case JoinTargetRuntime.NVIDIA:
      return "NVIDIA";
    case JoinTargetRuntime.APPLE:
      return "Apple GPU";
    case JoinTargetRuntime.DOCKER:
      return "Docker";
    default:
      return "";
  }
}
