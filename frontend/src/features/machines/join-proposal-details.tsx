import { Detail, DetailList } from "@/components/ui/detail-list";
import { workloadKindLabel } from "@/domain/execution-target";
import {
  type JoinProposal,
  type JoinProposedTarget,
  JoinTargetRuntime,
} from "@/gen/dotcore/site/v1/join_pb";

/** The facts an operator checks before allowing a machine to join: who it is and what changes. */
export function JoinProposalDetails({ proposal }: { proposal: JoinProposal }) {
  const { kind } = proposal;

  return (
    <DetailList>
      <Detail label="Name" value={proposal.displayName} />
      <Detail label="Address" value={`${proposal.host}:${proposal.privatePort}`} />
      {proposal.hostDiffersFromPeer ? (
        <Detail label="Connected from" value={proposal.peerAddress} />
      ) : null}
      {kind.case === "newNode" ? (
        <Detail
          label="Targets"
          value={
            kind.value.targets.length === 0 ? (
              "None"
            ) : (
              <ul className="flex flex-col gap-1">
                {kind.value.targets.map((target, index) => (
                  <li key={index}>{targetLabel(target)}</li>
                ))}
              </ul>
            )
          }
        />
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
      <Detail
        label="Key fingerprint"
        value={<code className="font-mono break-all">{proposal.keyFingerprint}</code>}
      />
    </DetailList>
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
