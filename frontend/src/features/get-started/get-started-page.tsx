import { X } from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router-dom";

import { useClients } from "@/api/clients";
import { Button } from "@/components/ui/button";
import { Notice } from "@/components/ui/notice";
import { PageActions, PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import { GetStartedSectionIcon } from "@/components/ui/section-icons";
import { AddMachineDialog } from "@/features/machines/add-machine-dialog";
import { useSiteMemory } from "@/features/memory/use-site-memory";
import type { LocalDeploymentValues } from "@/features/models/deploy/model-deploy-launch";
import { useSharedModelsPolling } from "@/features/models/polling/models-polling-context";
import { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { useGetStartedDismissed } from "@/hooks/use-get-started-dismissed";
import { useWatchedModelDeployments } from "@/resource-watch/resource-watch";

import { CustomizeModelDialog } from "./customize-model-dialog";
import { FollowedStep } from "./followed-step";
import { launchedProposal, proposalLaunch } from "./get-started-launch";
import { type ModelProposal, modelProposalKey } from "./get-started-proposals";
import { MachinesCard } from "./machines-card";
import { ProposalCard } from "./proposal-card";
import { UseCaseCards } from "./use-case-cards";
import { useGetStartedData } from "./use-get-started-data";
import { type StartedDeployments, useGetStartedDeploy } from "./use-get-started-deploy";
import { useProposalMemory } from "./use-proposal-memory";

/**
 * The Get started page: a guided path from the site's machines to a running model and app. The
 * operator picks a use case, reviews its proposal and deploys it. Dismissing the page hides its
 * sidebar item until the profile menu reopens it.
 */
export function GetStartedPage() {
  const navigate = useNavigate();
  const [, setDismissed] = useGetStartedDismissed();
  const { applicationClient, modelClient, siteClient } = useClients();
  const data = useGetStartedData({ applicationClient, modelClient });
  const deployer = useGetStartedDeploy({ applicationClient, modelClient });
  const { snapshot } = useSharedModelsPolling();
  const watchedDeployments = useWatchedModelDeployments();
  const siteMemory = useSiteMemory(
    watchedDeployments?.value?.deployments,
    watchedDeployments?.failed ?? false,
  );
  const [useCase, setUseCase] = useState(UseCase.CHAT_ASSISTANT);
  const [pickedKeys, setPickedKeys] = useState<ReadonlyMap<UseCase, string>>(new Map());
  const [customizations, setCustomizations] = useState<ReadonlyMap<UseCase, Customization>>(
    new Map(),
  );
  const [following, setFollowing] = useState<ReadonlyMap<UseCase, StartedDeployments>>(new Map());
  const [customizing, setCustomizing] = useState(false);
  const [addingMachine, setAddingMachine] = useState(false);

  const listed = data.useCases.find((candidate) => candidate.useCase === useCase);
  // A model deployment this page created for the use case stays its model, so a retry deploys
  // only what is missing; going back from its progress releases it.
  const created = deployer.createdModels.get(useCase);
  const createdModel = created && {
    ...created,
    deployment: watchedDeployments?.value?.deployments.find(
      (deployment) => deployment.modelDeploymentId === created.deploymentId,
    ),
  };
  const proposal =
    listed && createdModel ? { ...listed, model: createdModel, otherChoices: [] } : listed;
  const choices = proposal?.model ? [proposal.model, ...proposal.otherChoices] : [];
  // The pick holds while its row stays proposed; otherwise the best row is the proposal.
  const choice =
    choices.find((candidate) => modelProposalKey(candidate) === pickedKeys.get(useCase)) ??
    choices[0];
  const catalogModel = snapshot?.availableModels.find((model) => model.modelId === choice?.modelId);
  // A customization holds while its choice stays picked; a running model has nothing to launch.
  const customization = customizations.get(useCase);
  const customLaunch =
    choice && !choice.deploymentId && customization?.choiceKey === modelProposalKey(choice)
      ? customization.launch
      : undefined;
  const launch =
    choice && !choice.deploymentId
      ? (customLaunch ?? proposalLaunch(choice, catalogModel?.displayName || choice.modelId))
      : undefined;
  const picked = choice && customLaunch ? launchedProposal(choice, customLaunch) : choice;
  const memory = useProposalMemory(
    modelClient,
    customLaunch && picked ? [...choices, picked] : choices,
  );
  const failure = deployer.failures.get(useCase);
  const followed = following.get(useCase);
  const loadingVisible = useDelayedLoading(data.loading);

  const pick = (model: ModelProposal) =>
    setPickedKeys((current) => new Map(current).set(useCase, modelProposalKey(model)));
  const apply = (applied: LocalDeploymentValues) => {
    if (choice) {
      const key = modelProposalKey(choice);
      setCustomizations((current) =>
        new Map(current).set(useCase, { choiceKey: key, launch: applied }),
      );
    }
    setCustomizing(false);
  };
  const follow = (deployments: StartedDeployments | undefined) => {
    if (deployments) setFollowing((current) => new Map(current).set(useCase, deployments));
  };
  // Leaves a failed or removed deployment's progress for the proposal, which can deploy afresh.
  const back = () => {
    setFollowing((current) => {
      const next = new Map(current);
      next.delete(useCase);
      return next;
    });
    deployer.forget(useCase);
  };
  const deploy = async () => {
    if (!proposal) return;
    follow(
      await deployer.deploy({
        app: proposal.app,
        model: picked && launch ? { launch, proposal: picked } : undefined,
        runningModelDeploymentId: choice?.deploymentId,
        useCase,
      }),
    );
  };
  // Continue has nothing to deploy: what the use case needs already runs.
  const proceed = () =>
    follow({
      applicationDeploymentId: proposal?.app?.target?.applicationDeploymentId,
      modelDeploymentId: choice?.deploymentId,
    });

  return (
    <div className="flex w-full flex-col gap-6">
      <PageHeader>
        <PageHeaderHeading icon={GetStartedSectionIcon}>Get started</PageHeaderHeading>
        <PageActions>
          <Button
            onClick={() => {
              setDismissed(true);
              void navigate("/overview");
            }}
            size="console"
            type="button"
            variant="outline"
          >
            <X aria-hidden="true" data-icon="inline-start" />
            Dismiss
          </Button>
        </PageActions>
      </PageHeader>
      {data.error ? <Notice tone="destructive">{data.error}</Notice> : null}
      {data.loading && loadingVisible && !data.error ? (
        <p className="text-sm text-muted-foreground" role="status">
          Loading recommendations…
        </p>
      ) : null}
      <UseCaseCards onSelect={setUseCase} proposals={data.useCases} selected={useCase} />
      <div className="grid items-start gap-4 lg:grid-cols-[minmax(0,1.5fr)_minmax(260px,1fr)]">
        {followed ? (
          <FollowedStep
            applicationClient={applicationClient}
            deployments={followed}
            machines={data.machines}
            modelClient={modelClient}
            onBack={back}
            useCase={useCase}
          />
        ) : proposal ? (
          <ProposalCard
            choiceKey={choice && modelProposalKey(choice)}
            error={failure?.message}
            machines={data.machines}
            memory={memory}
            modelDeployments={watchedDeployments?.value?.deployments ?? []}
            models={snapshot?.availableModels ?? []}
            onAddMachine={() => setAddingMachine(true)}
            onContinue={proceed}
            onCustomize={catalogModel ? () => setCustomizing(true) : undefined}
            onDeploy={() => void deploy()}
            onPick={pick}
            pending={deployer.pending !== undefined}
            picked={picked}
            proposal={proposal}
          />
        ) : (
          <div />
        )}
        <MachinesCard
          machines={data.machines}
          memory={siteMemory.kind === "loading" ? undefined : siteMemory.memory}
          onAddMachine={() => setAddingMachine(true)}
        />
      </div>
      {customizing && launch && catalogModel ? (
        <CustomizeModelDialog
          deployments={watchedDeployments?.value?.deployments ?? []}
          launch={launch}
          model={catalogModel}
          modelClient={modelClient}
          onApply={apply}
          onClose={() => setCustomizing(false)}
          routeConflict={failure?.routeConflict}
          siteClient={siteClient}
        />
      ) : null}
      <AddMachineDialog onOpenChange={setAddingMachine} open={addingMachine} />
    </div>
  );
}

/** The operator's launch for one use case, kept while the choice it was made from stays picked. */
interface Customization {
  choiceKey: string;
  launch: LocalDeploymentValues;
}
