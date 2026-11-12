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

/** The proposal for one use case: a model, an app, or both, and whether the site already serves it. */
export interface UseCaseProposal {
  useCase: UseCase;
  /** The best model row on its first target; absent when no machine can start one. */
  model?: ModelProposal;
  /** The remaining rows, each on its first target, in recommendation order. */
  otherChoices: ModelProposal[];
  /** The catalog app declared for the use case; absent when none is. */
  app?: AppProposal;
  /** Whether everything proposed is already deployed and ready. */
  ready: boolean;
}

/** One recommended model profile on one target. */
export interface ModelProposal {
  modelId: string;
  profileId: string;
  target: ExactPlacement;
  fit: ModelRecommendationFit;
  /** The context per request to show: the running deployment's own, else the row's. */
  contextTokensPerRequest: bigint;
  /** The total context a customized launch holds; one request's context per request without one. */
  totalContextTokens?: bigint;
  startingTotalContextTokens?: bigint;
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
 * What the proposal's primary action does: deploy what isn't running yet, continue when everything
 * proposed already runs, or nothing when no machine can start what the use case needs.
 */
export type ProposalAction = "continue" | "deploy" | "unavailable";

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

  return model || proposal.app?.target ? "continue" : "unavailable";
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

    return {
      app,
      model: first,
      otherChoices: rest,
      ready: isServed(useCase, first, app),
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
    contextTokensPerRequest: running ?? row.contextTokensPerRequest,
    deployment,
    deploymentId,
    fit: target.fit,
    modelId: row.modelId,
    profileId: row.profileId,
    startingTotalContextTokens: target.startingTotalContextTokens,
    target: target.target,
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

function isServed(
  useCase: UseCase,
  model: ModelProposal | undefined,
  app: AppProposal | undefined,
): boolean {
  const appServed = app?.target ? isApplicationReady(app.deployment) : undefined;
  if (!usesModel(useCase)) return appServed === true;

  const modelReady = model?.deployment
    ? deploymentStatus(model.deployment) === ModelDeploymentStatus.READY
    : false;

  return modelReady && appServed !== false;
}

/** Whether an application deployment is up: its lifecycle and readiness are both ready. */
export function isApplicationReady(deployment: ApplicationDeployment | undefined): boolean {
  return (
    deployment !== undefined &&
    applicationDeploymentLifecycleState(deployment) === ApplicationLifecycleState.READY &&
    applicationDeploymentReadinessState(deployment) === ApplicationReadinessState.READY
  );
}
