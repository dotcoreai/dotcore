import {
  applicationDeploymentLifecycleState,
  applicationDeploymentReadinessState,
} from "@/domain/application-projection";
import { deploymentStatus, localModelSnapshot } from "@/domain/model-projection";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  resolvedEngineParameterValue,
} from "@/features/models/runtime/engine-parameter-values";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { ApplicationTargetOption } from "@/gen/dotcore/application/v1/service_pb";
import {
  ApplicationLifecycleState,
  ApplicationReadinessState,
} from "@/gen/dotcore/application/v1/state_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import type { ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type {
  ModelRecommendationFit,
  RecommendedModelProfile,
} from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

/** The use cases Get started offers, in the order it lists them. */
export const getStartedUseCases: readonly UseCase[] = [
  UseCase.CHAT_ASSISTANT,
  UseCase.CODING,
  UseCase.IMAGE_GENERATION,
];

/**
 * How far the site is in serving a use case: `ready` when everything proposed is deployed and
 * ready, `failed` when a deployment of the use case failed (it takes the use case over until
 * removed), `deploying` when everything proposed has a listed deployment and not all are ready,
 * and `none` otherwise (partly deployed included, so Deploy creates the rest).
 */
export type UseCaseStage = "deploying" | "failed" | "none" | "ready";

/** The proposal for one use case: a model, an app, or both, and the stage the site serves it at. */
export interface UseCaseProposal {
  useCase: UseCase;
  /** The best model row on its first target; absent when no machine can start one. */
  model?: ModelProposal;
  /** The remaining rows, each on its first target, in recommendation order. */
  otherChoices: ModelProposal[];
  /** The catalog app declared for the use case; absent when none is. */
  app?: AppProposal;
  /** The use case's failed deployments; present exactly when the stage is `failed`. */
  failed?: FailedDeployments;
  stage: UseCaseStage;
}

/** The failed deployments of a use case; at least one is set. */
export interface FailedDeployments {
  application?: ApplicationDeployment;
  model?: ModelDeployment;
}

/** One recommended model profile on one target. */
export interface ModelProposal {
  modelId: string;
  profileId: string;
  target: ExactPlacement;
  fit: ModelRecommendationFit;
  /**
   * The context per request to show and launch: the running deployment's own, else the one the
   * target starts the row at, lowered when one request at the row's doesn't fit there.
   */
  contextTokensPerRequest: bigint;
  /** The parallel requests a customized launch holds; the profile's without one. */
  parallelRequests?: bigint;
  /**
   * The total context to launch: a customized launch's, else the one the target starts the row at;
   * absent when unknown, so resolution derives it.
   */
  totalContextTokens?: bigint;
  /** The running or starting deployment on the target; its id is set before the watch lists it. */
  deploymentId?: string;
  deployment?: ModelDeployment;
}

/** A catalog app with the target it would deploy to. */
export interface AppProposal {
  application: Application;
  /** The first target; absent when no target can run the app. */
  target?: ApplicationTargetOption;
  /** The app's running or starting deployment on its first target. */
  deployment?: ApplicationDeployment;
  /**
   * Whether the app prefers an accelerator and its first target holds none of them, so it runs
   * on the CPU.
   */
  runsOnCpu: boolean;
}

export interface UseCaseReads {
  /** Recommendation rows per use case. */
  rows: ReadonlyMap<UseCase, readonly RecommendedModelProfile[]>;
  /** Targets per catalog application id, best first. */
  targets: ReadonlyMap<string, readonly ApplicationTargetOption[]>;
}

/**
 * What the proposal's primary action does: deploy what isn't running yet, or nothing when no
 * machine can start what the use case needs or everything proposed already runs.
 */
export type ProposalAction = "deploy" | "unavailable";

/** Whether a use case is served by a model; image generation is served by its app alone. */
export function usesModel(useCase: UseCase): boolean {
  return useCase !== UseCase.IMAGE_GENERATION;
}

/** The primary action for a proposal with `model` as its picked model. */
export function proposalAction(
  proposal: UseCaseProposal,
  model: ModelProposal | undefined,
): ProposalAction {
  if (usesModel(proposal.useCase) && !model) return "unavailable";

  const deployModel = Boolean(model && !model.deploymentId);
  const deployApp = Boolean(proposal.app?.target && !proposal.app.target.applicationDeploymentId);
  if (deployModel || deployApp) return "deploy";

  return "unavailable";
}

/** A model proposal's identity, stable across reads while the same row stays on the same target. */
export function modelProposalKey(model: ModelProposal): string {
  return [
    model.modelId,
    model.profileId,
    model.target.siteNodeId,
    model.target.executionTargetId,
  ].join("\0");
}

/** The catalog app declared for a use case, the first one when several declare it. */
export function applicationForUseCase(
  applications: readonly Application[],
  useCase: UseCase,
): Application | undefined {
  return applications.find((application) => application.useCases.includes(useCase));
}

/** Builds each use case's proposal from the reads and the live deployments. */
export function buildProposals(
  reads: UseCaseReads,
  applications: readonly Application[],
  modelDeployments: readonly ModelDeployment[],
  applicationDeployments: readonly ApplicationDeployment[],
): UseCaseProposal[] {
  return getStartedUseCases.map((useCase) => {
    const [first, ...rest] = (reads.rows.get(useCase) ?? []).flatMap((row) => {
      const proposal = modelProposal(row, modelDeployments);
      return proposal ? [proposal] : [];
    });
    const application = applicationForUseCase(applications, useCase);
    const app = application
      ? appProposal(
          application,
          reads.targets.get(application.applicationId),
          applicationDeployments,
        )
      : undefined;

    const failed = failedDeployments(useCase, reads.rows.get(useCase) ?? [], app, modelDeployments);

    return {
      app,
      failed,
      model: first,
      otherChoices: rest,
      stage: useCaseStage(useCase, first, app, failed),
      useCase,
    };
  });
}

function modelProposal(
  row: RecommendedModelProfile,
  deployments: readonly ModelDeployment[],
): ModelProposal | undefined {
  const target = row.targets[0];
  if (!target?.target) return undefined;

  const deploymentId = target.modelDeploymentId;
  const deployment = deploymentId
    ? deployments.find((candidate) => candidate.modelDeploymentId === deploymentId)
    : undefined;
  const running = resolvedEngineParameterValue(
    localModelSnapshot(deployment?.snapshot)?.engineParameterValues,
    CONTEXT_PER_REQUEST_PARAMETER_ID,
  );

  return {
    contextTokensPerRequest:
      running ?? target.startingContextTokensPerRequest ?? row.contextTokensPerRequest,
    deployment,
    deploymentId,
    fit: target.fit,
    modelId: row.modelId,
    profileId: row.profileId,
    target: target.target,
    totalContextTokens: target.startingTotalContextTokens,
  };
}

function appProposal(
  application: Application,
  targets: readonly ApplicationTargetOption[] | undefined,
  deployments: readonly ApplicationDeployment[],
): AppProposal {
  const target = targets?.[0];
  const deploymentId = target?.applicationDeploymentId;

  return {
    application,
    deployment: deploymentId
      ? deployments.find((candidate) => candidate.applicationDeploymentId === deploymentId)
      : undefined,
    runsOnCpu: Boolean(
      target && application.preferredAccelerators.length > 0 && !target.matchesPreference,
    ),
    target,
  };
}

function useCaseStage(
  useCase: UseCase,
  model: ModelProposal | undefined,
  app: AppProposal | undefined,
  failed: FailedDeployments | undefined,
): UseCaseStage {
  const appServed = app?.target ? isApplicationReady(app.deployment) : undefined;
  const modelReady = model?.deployment
    ? deploymentStatus(model.deployment) === ModelDeploymentStatus.READY
    : false;
  if (usesModel(useCase) ? modelReady && appServed !== false : appServed === true) return "ready";

  if (failed) return "failed";

  // Every proposed piece needs a deployment the watch lists, so a stale read can't open a step
  // whose deployments are gone.
  const modelPiece = usesModel(useCase) ? [model?.deployment] : [];
  const appPiece = app?.target ? [app.deployment] : [];
  const pieces = [...modelPiece, ...appPiece];
  if (!pieces.length || pieces.some((deployment) => !deployment)) return "none";

  return "deploying";
}

// A failed model deployment is found among every row and target of the use case: it no longer
// counts as running, so another row may be the best one while it still holds its route name.
function failedDeployments(
  useCase: UseCase,
  rows: readonly RecommendedModelProfile[],
  app: AppProposal | undefined,
  modelDeployments: readonly ModelDeployment[],
): FailedDeployments | undefined {
  const model = usesModel(useCase)
    ? modelDeployments.find(
        (deployment) =>
          isModelDeploymentFailed(deployment) &&
          rows.some((row) => deploymentServesRow(deployment, row)),
      )
    : undefined;
  const application =
    app?.deployment && isApplicationDeploymentFailed(app.deployment) ? app.deployment : undefined;

  return model || application ? { application, model } : undefined;
}

function deploymentServesRow(deployment: ModelDeployment, row: RecommendedModelProfile): boolean {
  const local = localModelSnapshot(deployment.snapshot);
  const placement = deployment.intent?.placement;
  if (local?.modelId !== row.modelId || local.profileId !== row.profileId || !placement) {
    return false;
  }

  return row.targets.some(
    (target) =>
      target.target?.siteNodeId === placement.siteNodeId &&
      target.target.executionTargetId === placement.executionTargetId,
  );
}

/** Whether a model deployment has failed. */
export function isModelDeploymentFailed(deployment: ModelDeployment): boolean {
  return deploymentStatus(deployment) === ModelDeploymentStatus.FAILED;
}

/** Whether an application deployment has failed. */
export function isApplicationDeploymentFailed(deployment: ApplicationDeployment): boolean {
  return applicationDeploymentLifecycleState(deployment) === ApplicationLifecycleState.FAILED;
}

/** Whether an application deployment is up: its lifecycle and readiness are both ready. */
export function isApplicationReady(deployment: ApplicationDeployment | undefined): boolean {
  return (
    deployment !== undefined &&
    applicationDeploymentLifecycleState(deployment) === ApplicationLifecycleState.READY &&
    applicationDeploymentReadinessState(deployment) === ApplicationReadinessState.READY
  );
}
