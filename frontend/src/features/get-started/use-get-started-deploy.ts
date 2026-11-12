import { create } from "@bufbuild/protobuf";
import { useRef, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import { describeMutationError as describeApplicationError } from "@/features/applications/application-mutation-errors";
import type { LocalDeploymentValues } from "@/features/models/deploy/model-deploy-launch";
import {
  type RouteNameConflict,
  routeNameConflictMessage,
} from "@/features/models/deployment-exposure";
import {
  describeMutationError,
  routeConflictFromError,
} from "@/features/models/model-mutation-errors";
import type { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import {
  CreateLocalModelDeploymentRequestSchema,
  LocalModelDeploymentConfigurationSchema,
} from "@/gen/dotcore/model/v1/service_pb";

import type { AppProposal, ModelProposal } from "./get-started-proposals";

/** What one Deploy click starts: the model's launch unless it already runs, and the app. */
export interface DeployPlan {
  app?: AppProposal;
  /** The model to launch, as the card shows it; absent when the use case needs none or it runs. */
  model?: { launch: LocalDeploymentValues; proposal: ModelProposal };
  /** The proposed model's running deployment, which the use case reuses. */
  runningModelDeploymentId?: string;
  useCase: UseCase;
}

/** The deployments a use case's Deploy started or reused, which the deploying step follows. */
export interface StartedDeployments {
  applicationDeploymentId?: string;
  modelDeploymentId?: string;
}

/** A failed Deploy, shown on its use case's proposal. */
export interface DeployFailure {
  message: string;
  /** The route name the backend rejected as taken, which Customize then shows on the name. */
  routeConflict?: RouteNameConflict;
}

/**
 * Get started's one-click deploy: it creates the model deployment, then the app's on its target,
 * skipping what already runs, and answers with the deployments to follow. One deploy runs at a
 * time across use cases. A failure stops before the app and stays on the use case until the next
 * attempt. A model deployment it created stays the use case's model, so a retry never creates a
 * second one, until `forget` releases it.
 */
export function useGetStartedDeploy({
  applicationClient,
  modelClient,
}: {
  applicationClient: ApplicationClient;
  modelClient: ModelClient;
}) {
  const [failures, setFailures] = useState<ReadonlyMap<UseCase, DeployFailure>>(new Map());
  const [pending, setPending] = useState<UseCase>();
  const [createdModels, setCreatedModels] = useState<ReadonlyMap<UseCase, ModelProposal>>(
    new Map(),
  );
  const createdModelsRef = useRef(createdModels);
  const pendingRef = useRef(false);

  const setCreated = (useCase: UseCase, model: ModelProposal | undefined) => {
    const next = new Map(createdModelsRef.current);
    if (model) next.set(useCase, model);
    else next.delete(useCase);
    createdModelsRef.current = next;
    setCreatedModels(next);
  };
  const fail = (useCase: UseCase, failure: DeployFailure | undefined) =>
    setFailures((current) => {
      const next = new Map(current);
      if (failure) next.set(useCase, failure);
      else next.delete(useCase);
      return next;
    });

  const deploy = async ({
    app,
    model,
    runningModelDeploymentId,
    useCase,
  }: DeployPlan): Promise<StartedDeployments | undefined> => {
    if (pendingRef.current) return undefined;
    pendingRef.current = true;
    setPending(useCase);
    fail(useCase, undefined);
    try {
      let modelDeploymentId =
        createdModelsRef.current.get(useCase)?.deploymentId ?? runningModelDeploymentId;
      if (model && !modelDeploymentId) {
        try {
          modelDeploymentId = await createModel(modelClient, model.proposal.modelId, model.launch);
        } catch (error) {
          fail(useCase, modelFailure(error, model.launch));
          return undefined;
        }
        setCreated(useCase, { ...model.proposal, deploymentId: modelDeploymentId });
      }

      let applicationDeploymentId = app?.target?.applicationDeploymentId;
      if (app?.target?.target && !applicationDeploymentId) {
        try {
          applicationDeploymentId = await createApplication(applicationClient, app);
        } catch (error) {
          fail(useCase, { message: describeApplicationError("Create", error) });
          return undefined;
        }
      }
      return { applicationDeploymentId, modelDeploymentId };
    } finally {
      pendingRef.current = false;
      setPending(undefined);
    }
  };

  /** Releases the use case's created model and failure, so its next Deploy starts afresh. */
  const forget = (useCase: UseCase) => {
    setCreated(useCase, undefined);
    fail(useCase, undefined);
  };

  return { createdModels, deploy, failures, forget, pending };
}

async function createModel(
  client: ModelClient,
  modelId: string,
  { configuration, exposure, name, placement, profileId }: LocalDeploymentValues,
): Promise<string> {
  if (!client.model.createLocalModelDeployment) {
    throw new Error(
      "Deploy is unavailable because the model service does not support explicit local deployments.",
    );
  }

  const response = await client.model.createLocalModelDeployment({
    ...create(CreateLocalModelDeploymentRequestSchema, {
      configuration: create(LocalModelDeploymentConfigurationSchema, configuration),
      modelId,
      placement,
      profileId,
    }),
    exposure,
    modelDeploymentName: name,
  });
  const id = response.deployment?.modelDeploymentId;
  if (!id) throw new Error("Deploy response protocol error: missing accepted deployment ID.");
  return id;
}

async function createApplication(client: ApplicationClient, app: AppProposal): Promise<string> {
  const { application, target } = app;
  const response = await client.application.createApplicationDeployment({
    applicationDeploymentName: application.displayName,
    applicationId: application.applicationId,
    parameterValues: [],
    placement: target?.target,
  });
  const id = response.deployment?.applicationDeploymentId;
  if (!id) throw new Error("Create response protocol error: missing accepted deployment ID.");
  return id;
}

// A taken route names the fix: renaming the deployment in Customize, which the route follows.
function modelFailure(error: unknown, launch: LocalDeploymentValues): DeployFailure {
  const routeConflict = routeConflictFromError(error);
  if (!routeConflict) return { message: describeMutationError("Deploy", error) };

  const routeName =
    launch.exposure.kind.case === "direct" ? launch.exposure.kind.value.routeName : "";
  const taken =
    routeNameConflictMessage(routeConflict, routeName) ??
    "An inference route with this name already exists.";
  return { message: `${taken} Change the model's name in Customize.`, routeConflict };
}
