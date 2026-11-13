import { create } from "@bufbuild/protobuf";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { ListApplicationTargetsRequestSchema } from "@/gen/dotcore/application/v1/service_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { ListModelRecommendationsRequestSchema } from "@/gen/dotcore/model/v1/service_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";
import { createPollTimer } from "@/lib/poll-timer";
import { timing } from "@/lib/timing";
import {
  useWatchedApplicationDeployments,
  useWatchedMachines,
  useWatchedModelDeployments,
} from "@/resource-watch/resource-watch";
import { useSessionRead } from "@/resource-watch/use-session-read";

import {
  applicationForUseCase,
  buildProposals,
  getStartedUseCases,
  type UseCaseProposal,
  type UseCaseReads,
} from "./get-started-proposals";

/** What Get started shows: the site's machines and one proposal per use case. */
export interface GetStartedData {
  /** Set while a read keeps failing; the last data stays in place. */
  error?: string;
  /** Whether the first reads are still pending. */
  loading: boolean;
  machines: readonly Machine[];
  /** One proposal per use case, in the order Get started lists them. */
  useCases: readonly UseCaseProposal[];
}

const readError =
  "Recommendations are unavailable. Check that the backend is running; the last result stays shown.";

const sourceError =
  "Site data is unavailable. Check that the backend is running; the last result stays shown.";

/**
 * Loads Get started's data: the model recommendations for every use case that can start now, the
 * targets of each use case's catalog app, and the live machines and deployments. It reads again
 * shortly after a machine or deployment changes, retries a failed read with backoff, and pauses
 * while the page is hidden. Deployment status comes from the console's resource watch, so the
 * Apps section's polling provider is not needed.
 */
export function useGetStartedData({
  applicationClient,
  modelClient,
}: {
  applicationClient: ApplicationClient;
  modelClient: ModelClient;
}): GetStartedData {
  const readApplications = useCallback(
    (signal: AbortSignal) =>
      applicationClient.catalog
        .listApplications({}, { signal })
        .then((response) => response.applications),
    [applicationClient],
  );
  const catalog = useSessionRead(readApplications);
  const machines = useWatchedMachines();
  const modelDeployments = useWatchedModelDeployments();
  const applicationDeployments = useWatchedApplicationDeployments();
  const applications = catalog.value;

  const applicationIds = useMemo(
    () =>
      applications
        ? getStartedUseCases.flatMap((useCase) => {
            const application = applicationForUseCase(applications, useCase);
            return application ? [application.applicationId] : [];
          })
        : undefined,
    [applications],
  );
  const watchesLoaded = Boolean(
    machines?.value && modelDeployments?.value && applicationDeployments?.value,
  );
  const changeKey = useMemo(
    () =>
      watchesLoaded
        ? changeSignature(
            machines?.value?.machines ?? [],
            modelDeployments?.value?.deployments ?? [],
            applicationDeployments?.value?.deployments ?? [],
          )
        : undefined,
    [applicationDeployments?.value, machines?.value, modelDeployments?.value, watchesLoaded],
  );
  const { error, reads } = useRecommendationReads(
    modelClient,
    applicationClient,
    applicationIds,
    changeKey,
  );

  const proposals = useMemo(
    () =>
      reads && applications
        ? buildProposals(
            reads,
            applications,
            modelDeployments?.value?.deployments ?? [],
            applicationDeployments?.value?.deployments ?? [],
          )
        : [],
    [applicationDeployments?.value, applications, modelDeployments?.value, reads],
  );

  // The catalog and the watches keep their last values through a failure, so it only adds a notice.
  const sourceFailed =
    catalog.failed ||
    Boolean(machines?.failed || modelDeployments?.failed || applicationDeployments?.failed);

  return {
    error: error ?? (sourceFailed ? sourceError : undefined),
    loading: !reads || !applications || !machines?.value,
    machines: machines?.value?.machines ?? [],
    useCases: proposals,
  };
}

// Reads the recommendations and app targets once the watches have data, then again, after a short
// wait, each time the change key moves. A failed read keeps the last reads and retries with backoff.
function useRecommendationReads(
  modelClient: ModelClient,
  applicationClient: ApplicationClient,
  applicationIds: readonly string[] | undefined,
  changeKey: string | undefined,
): { error?: string; reads?: UseCaseReads } {
  const [state, setState] = useState<{ error?: string; reads?: UseCaseReads }>({});
  const idsKey = applicationIds?.join("\n");
  const readOnce = useRef(false);
  const enabled = changeKey !== undefined && idsKey !== undefined;

  useEffect(() => {
    if (!enabled) return;

    let disposed = false;
    let failureCount = 0;
    let controller: AbortController | undefined;
    const ids = idsKey ? idsKey.split("\n") : [];
    const timer = createPollTimer(() => void run(), {
      onHidden: () => controller?.abort(),
      onVisible: () => void run(),
    });

    const run = async () => {
      if (disposed || document.visibilityState === "hidden") return;

      controller?.abort();
      const current = new AbortController();
      controller = current;
      try {
        const reads = await readAll(modelClient, applicationClient, ids, current.signal);
        if (disposed || current.signal.aborted) return;
        failureCount = 0;
        readOnce.current = true;
        setState({ reads });
      } catch {
        if (disposed || current.signal.aborted) return;
        failureCount += 1;
        setState((previous) => ({ ...previous, error: readError }));
        const delays = timing().pollRetryMs;
        timer.arm(delays[Math.min(failureCount - 1, delays.length - 1)]);
      }
    };

    // The first read waits on nothing; later ones wait out a burst of changes.
    timer.arm(readOnce.current ? timing().recommendationReadMs : 0);

    return () => {
      disposed = true;
      controller?.abort();
      timer.dispose();
    };
  }, [applicationClient, changeKey, enabled, idsKey, modelClient]);

  return state;
}

async function readAll(
  modelClient: ModelClient,
  applicationClient: ApplicationClient,
  applicationIds: readonly string[],
  signal: AbortSignal,
): Promise<UseCaseReads> {
  const [recommendations, targets] = await Promise.all([
    modelClient.model.listModelRecommendations(
      create(ListModelRecommendationsRequestSchema, {
        canStartNow: true,
        useCases: [...getStartedUseCases],
      }),
      { signal },
    ),
    Promise.all(
      applicationIds.map(async (applicationId) => {
        const response = await applicationClient.application.listApplicationTargets(
          create(ListApplicationTargetsRequestSchema, { applicationId }),
          { signal },
        );
        return [applicationId, response.targets] as const;
      }),
    ),
  ]);

  return {
    rows: new Map(recommendations.useCases.map((entry) => [entry.useCase, entry.rows])),
    targets: new Map(targets),
  };
}

// What the recommendations depend on: each machine's freshness and each deployment's presence and
// status. Other watch updates leave it unchanged and read nothing.
function changeSignature(
  machines: readonly Machine[],
  modelDeployments: readonly ModelDeployment[],
  applicationDeployments: readonly ApplicationDeployment[],
): string {
  return JSON.stringify([
    machines.map((m) => [m.machineId, m.observationFreshness?.state]),
    modelDeployments.map((d) => [d.modelDeploymentId, d.desiredState, d.state?.status]),
    applicationDeployments.map((d) => [
      d.applicationDeploymentId,
      d.desiredState,
      d.state?.lifecycleState,
    ]),
  ]);
}
