import { create } from "@bufbuild/protobuf";
import { useEffect, useMemo, useRef, useState } from "react";

import type { ModelClient } from "@/api/model-client";
import { deploymentVramBytes } from "@/domain/model-projection";
import { profileEstimateKey } from "@/features/models/profiles/model-row";
import {
  indexProfileCompatibility,
  type ProfileCompatibility,
  profileTargetVerdict,
} from "@/features/models/profiles/profile-compatibility";
import { ListModelProfileCompatibilityRequestSchema } from "@/gen/dotcore/model/v1/service_pb";
import { timing } from "@/lib/timing";

import { type ModelProposal, modelProposalKey } from "./get-started-proposals";

/**
 * The memory each model proposal takes on its target, keyed by `proposalMemoryKey`: a running
 * deployment's measured memory, else the catalog estimate of the profile at the proposal's context
 * per request and total context. A failed estimate read is tried again with backoff; a proposal
 * without a value has none yet.
 */
export function useProposalMemory(
  modelClient: ModelClient,
  proposals: readonly ModelProposal[],
): ReadonlyMap<string, bigint> {
  // Reads follow the set of distinct reads, not the identity of the proposals array.
  const readsKey = distinctReads(proposals).map(readKey).join("\n");
  const [sizings, setSizings] = useState<ReadonlyMap<string, ProfileCompatibility>>(new Map());
  const [failures, setFailures] = useState(0);
  const sizingsRef = useRef(sizings);

  useEffect(() => {
    sizingsRef.current = sizings;
  });

  useEffect(() => {
    const load = modelClient.model.listModelProfileCompatibility;
    const missing = (readsKey ? readsKey.split("\n") : [])
      .filter((key) => !sizingsRef.current.has(key))
      .map(parseReadKey);
    if (!load || !missing.length) return;

    const controller = new AbortController();
    let retry: ReturnType<typeof setTimeout> | undefined;
    void Promise.all(
      missing.map(async (read) => {
        try {
          const response = await load(
            create(ListModelProfileCompatibilityRequestSchema, {
              modelId: read.modelId,
              scenario: {
                contextTokensPerRequest: read.perRequest,
                totalContextTokens: read.total,
              },
            }),
            { signal: controller.signal },
          );
          return [[readKey(read), indexProfileCompatibility(response)] as const];
        } catch {
          return [];
        }
      }),
    ).then((results) => {
      if (controller.signal.aborted) return;
      const read = results.flat();
      setSizings((current) => new Map([...current, ...read]));
      // A failed read runs again after the retry delay, by re-running this effect.
      if (read.length < missing.length) {
        const delays = timing().pollRetryMs;
        retry = setTimeout(
          () => setFailures((count) => count + 1),
          delays[Math.min(failures, delays.length - 1)],
        );
      }
    });

    return () => {
      controller.abort();
      clearTimeout(retry);
    };
  }, [failures, modelClient, readsKey]);

  return useMemo(
    () =>
      new Map(
        proposals.flatMap((proposal) => {
          const bytes = proposalMemory(proposal, sizings);
          return bytes === undefined ? [] : [[proposalMemoryKey(proposal), bytes] as const];
        }),
      ),
    [proposals, sizings],
  );
}

/**
 * A proposal's memory identity: its row and target with the context it is sized at, so a
 * customized launch never takes the memory of the choice it was made from.
 */
export function proposalMemoryKey(proposal: ModelProposal): string {
  const { perRequest, total } = proposalContext(proposal);
  return [modelProposalKey(proposal), perRequest, total].join("\0");
}

/** One compatibility read: a catalog model sized at one context. */
interface SizingRead {
  modelId: string;
  perRequest: number;
  total: number;
}

function distinctReads(proposals: readonly ModelProposal[]): SizingRead[] {
  const reads = new Map<string, SizingRead>();
  for (const proposal of proposals) {
    if (proposal.deploymentId) continue;
    const read = sizingRead(proposal);
    reads.set(readKey(read), read);
  }
  return [...reads.values()];
}

function sizingRead(proposal: ModelProposal): SizingRead {
  return { modelId: proposal.modelId, ...proposalContext(proposal) };
}

// A proposal without its own total context holds one request at its context per request.
function proposalContext(proposal: ModelProposal): { perRequest: number; total: number } {
  const perRequest = Number(proposal.contextTokensPerRequest);
  return {
    perRequest,
    total: Number(proposal.totalContextTokens ?? proposal.contextTokensPerRequest),
  };
}

function readKey({ modelId, perRequest, total }: SizingRead): string {
  return `${modelId}\0${perRequest}\0${total}`;
}

function parseReadKey(key: string): SizingRead {
  const [modelId, perRequest, total] = key.split("\0");
  return { modelId, perRequest: Number(perRequest), total: Number(total) };
}

function proposalMemory(
  proposal: ModelProposal,
  sizings: ReadonlyMap<string, ProfileCompatibility>,
): bigint | undefined {
  if (proposal.deploymentId) {
    return proposal.deployment ? deploymentVramBytes(proposal.deployment) : undefined;
  }

  const sizing = sizings.get(readKey(sizingRead(proposal)));
  if (!sizing) return undefined;

  const { modelId, profileId, target } = proposal;
  const hardwareClassId = profileTargetVerdict(sizing, modelId, profileId, target)?.hardwareClassId;
  return sizing.estimates
    .get(profileEstimateKey(modelId, profileId))
    ?.find((estimate) => estimate.hardwareClassId === hardwareClassId)?.acceleratorMemoryBytes;
}
