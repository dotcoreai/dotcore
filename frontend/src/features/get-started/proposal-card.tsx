import type { LucideIcon } from "lucide-react";
import { Plus } from "lucide-react";
import { type ReactNode, useState } from "react";
import { Link } from "react-router-dom";

import { DeployActionIcon } from "@/components/ui/action-icons";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker } from "@/components/ui/resource-marker";
import {
  AppsSectionIcon,
  ModelsSectionIcon,
  PreferencesSectionIcon,
} from "@/components/ui/section-icons";
import { StatusBadge } from "@/components/ui/status-badge";
import {
  deploymentSiteNodeId,
  modelDeploymentPresentationName,
  projectModelDeploymentStatus,
} from "@/domain/model-projection";
import { placementMachine, placementMachineName } from "@/domain/placement";
import { engineKindKey, engineKindLabel } from "@/features/models/profiles/model-row";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ModelDeployment,
  ModelDeploymentDesiredState,
} from "@/gen/dotcore/model/v1/deployment_pb";
import { type Machine, NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";
import { formatBytes, formatCompactCount } from "@/lib/format";

import {
  type AppProposal,
  type ModelProposal,
  modelProposalKey,
  proposalAction,
  type UseCaseProposal,
  usesModel,
} from "./get-started-proposals";
import { useCaseCopy } from "./use-case-copy";
import { proposalMemoryKey } from "./use-proposal-memory";

/**
 * The proposal for one use case: its model on a machine, with the other choices one click away,
 * and its app, then the action that deploys what isn't running yet. When no machine has room for
 * a model, it says why and points to the models and to adding a machine.
 */
export function ProposalCard({
  choiceKey,
  error,
  machines,
  memory,
  modelDeployments,
  models,
  onAddMachine,
  onContinue,
  onCustomize,
  onDeploy,
  onPick,
  pending,
  picked,
  proposal,
}: {
  /** The key of the choice the picked model starts from, before its customization. */
  choiceKey: string | undefined;
  /** Why the last Deploy failed. */
  error?: string;
  machines: readonly Machine[];
  /** Memory per model proposal, keyed by `proposalMemoryKey`. */
  memory: ReadonlyMap<string, bigint>;
  modelDeployments: readonly ModelDeployment[];
  /** The catalog models, which name the proposals' models, profiles and engines. */
  models: readonly Model[];
  onAddMachine: () => void;
  onContinue: () => void;
  /** Opens Customize on the picked model; absent while the catalog that names it loads. */
  onCustomize?: (model: ModelProposal) => void;
  onDeploy: () => void;
  onPick: (model: ModelProposal) => void;
  /** Whether a Deploy is running, for this use case or another; one runs at a time. */
  pending: boolean;
  /** The model the operator picked among the proposal's choices, with its customization. */
  picked: ModelProposal | undefined;
  proposal: UseCaseProposal;
}) {
  const action = proposalAction(proposal, picked);
  const choices = proposal.model ? [proposal.model, ...proposal.otherChoices] : [];
  const machineName = (siteNodeId: string) =>
    placementMachineName(siteNodeId, placementMachine(machines, siteNodeId));

  return (
    <Card className="gap-0 px-5 py-4.5">
      {picked ? (
        <ModelItem
          choiceKey={choiceKey}
          choices={choices}
          key={proposal.useCase}
          machineName={machineName}
          memory={memory}
          models={models}
          onCustomize={onCustomize}
          onPick={onPick}
          picked={picked}
        />
      ) : usesModel(proposal.useCase) ? (
        <NoRoomItem
          machines={machines}
          modelDeployments={modelDeployments}
          onAddMachine={onAddMachine}
          useCaseLabel={useCaseCopy[proposal.useCase].label}
        />
      ) : null}
      {proposal.app ? <AppItem app={proposal.app} machineName={machineName} /> : null}
      {error ? (
        <Notice className="mb-4" role="alert" tone="destructive">
          {error}
        </Notice>
      ) : null}
      <div className="mt-0.5 flex justify-end gap-2 border-t border-border pt-4">
        {action === "continue" ? (
          <Button onClick={onContinue} size="console" type="button">
            Continue
          </Button>
        ) : (
          <Button
            disabled={action === "unavailable" || pending}
            onClick={onDeploy}
            size="console"
            type="button"
          >
            <DeployActionIcon aria-hidden="true" data-icon="inline-start" />
            {pending ? "Deploying…" : "Deploy"}
          </Button>
        )}
      </div>
    </Card>
  );
}

function ModelItem({
  choiceKey,
  choices,
  machineName,
  memory,
  models,
  onCustomize,
  onPick,
  picked,
}: {
  choiceKey: string | undefined;
  choices: readonly ModelProposal[];
  machineName: (siteNodeId: string) => string;
  memory: ReadonlyMap<string, bigint>;
  models: readonly Model[];
  onCustomize?: (model: ModelProposal) => void;
  onPick: (model: ModelProposal) => void;
  picked: ModelProposal;
}) {
  const [showChoices, setShowChoices] = useState(false);
  const names = modelNames(models, picked);
  const memoryText = (model: ModelProposal) => {
    const bytes = memory.get(proposalMemoryKey(model));
    return bytes === undefined ? "—" : formatBytes(bytes);
  };

  return (
    <Item icon={ModelsSectionIcon} kind="Model">
      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex items-center text-[15px] font-semibold">
          <ResourceMarker colorKey={markerKey(picked)} />
          {names.model}
        </span>
        <Tag>{names.profile}</Tag>
        {names.engine ? <Tag>{names.engine}</Tag> : null}
        {picked.deployment ? (
          <StatusBadge status={projectModelDeploymentStatus(picked.deployment)} />
        ) : null}
        {picked.deploymentId ? null : (
          <span className="ml-auto">
            <Button
              disabled={!onCustomize}
              onClick={() => onCustomize?.(picked)}
              size="sm"
              type="button"
              variant="outline"
            >
              <PreferencesSectionIcon aria-hidden="true" data-icon="inline-start" />
              Customize
            </Button>
          </span>
        )}
      </div>
      <DetailList>
        <Detail
          label="Machine"
          value={
            <>
              <ResourceMarker colorKey={picked.target.siteNodeId} />
              {machineName(picked.target.siteNodeId)}
            </>
          }
        />
        <Detail label="Memory" value={memoryText(picked)} />
        <Detail
          label="Context per request"
          value={formatCompactCount(Number(picked.contextTokensPerRequest))}
        />
      </DetailList>
      {choices.length > 1 ? (
        <Button
          aria-expanded={showChoices}
          className="h-auto justify-self-start p-0"
          onClick={() => setShowChoices((shown) => !shown)}
          size="sm"
          type="button"
          variant="link"
        >
          {showChoices ? "Hide other choices" : `Other choices (${choices.length - 1})`}
        </Button>
      ) : null}
      {showChoices && choices.length > 1 ? (
        <div aria-label="Model choices" className="grid gap-1.5" role="group">
          {choices.map((choice) => {
            const choiceNames = modelNames(models, choice);
            const subtitle = [choiceNames.profile, choiceNames.engine].filter(Boolean).join(" · ");
            return (
              <button
                aria-pressed={modelProposalKey(choice) === choiceKey}
                className="group/choice flex w-full items-center gap-2.5 rounded-lg border-[1.5px] border-border bg-card px-2.5 py-2 text-left outline-none hover:border-foreground/25 focus-visible:ring-3 focus-visible:ring-ring/50 aria-pressed:border-primary"
                key={modelProposalKey(choice)}
                onClick={() => onPick(choice)}
                type="button"
              >
                <span
                  aria-hidden="true"
                  className="size-3.5 flex-none rounded-full border-[1.5px] border-foreground/30 group-aria-pressed/choice:border-4 group-aria-pressed/choice:border-primary"
                />
                <span className="grid min-w-0 flex-1">
                  <strong className="text-sm font-semibold">
                    <ResourceMarker colorKey={markerKey(choice)} />
                    {choiceNames.model}
                  </strong>
                  <span className="text-xs text-muted-foreground">
                    {subtitle} · <ResourceMarker colorKey={choice.target.siteNodeId} />
                    {machineName(choice.target.siteNodeId)}
                  </span>
                </span>
                <span className="flex items-center gap-2 text-xs text-muted-foreground">
                  {memoryText(choice)}
                  {choice.deployment ? (
                    <StatusBadge status={projectModelDeploymentStatus(choice.deployment)} />
                  ) : null}
                </span>
              </button>
            );
          })}
        </div>
      ) : null}
    </Item>
  );
}

// Nothing the recommendations list starts now on a machine that reports its free memory.
function NoRoomItem({
  machines,
  modelDeployments,
  onAddMachine,
  useCaseLabel,
}: {
  machines: readonly Machine[];
  modelDeployments: readonly ModelDeployment[];
  onAddMachine: () => void;
  useCaseLabel: string;
}) {
  return (
    <Item icon={ModelsSectionIcon} kind="Model">
      <Notice tone="warning">
        No machine has room for a {useCaseLabel.toLowerCase()} model.
        <span className="block">{noRoomReason(machines, modelDeployments)}</span>
      </Notice>
      <div className="flex flex-wrap items-center gap-2">
        <Link className={buttonVariants({ size: "sm", variant: "outline" })} to="/models">
          <ModelsSectionIcon aria-hidden="true" data-icon="inline-start" />
          Models
        </Link>
        <Button onClick={onAddMachine} size="sm" type="button" variant="outline">
          <Plus aria-hidden="true" data-icon="inline-start" />
          Add a machine
        </Button>
      </div>
    </Item>
  );
}

function AppItem({
  app,
  machineName,
}: {
  app: AppProposal;
  machineName: (siteNodeId: string) => string;
}) {
  const { application, runsOnCpu, target } = app;
  const name = application.displayName || application.applicationId;

  return (
    <Item icon={AppsSectionIcon} kind="App">
      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex items-center text-[15px] font-semibold">
          <ResourceMarker colorKey={application.applicationId} />
          {name}
        </span>
        {application.version ? <Tag>{application.version}</Tag> : null}
      </div>
      {!target ? (
        <Notice tone="warning">
          No machine can run apps.
          <span className="block">Install Docker on a machine to run {name}.</span>
        </Notice>
      ) : runsOnCpu ? (
        <Notice tone="warning">
          No {acceleratorNames(application.preferredAccelerators)} machine can run {name}.
          <span className="block">
            It runs on the CPU on {machineName(target.target?.siteNodeId ?? "")}, slowly.
          </span>
        </Notice>
      ) : null}
    </Item>
  );
}

/** One item of the card, led by the kind of thing it proposes. */
export function Item({
  children,
  icon: Icon,
  kind,
}: {
  children: ReactNode;
  icon: LucideIcon;
  kind: string;
}) {
  return (
    <section
      aria-label={kind}
      className="grid gap-2.5 border-t border-border py-4 first:border-t-0 first:pt-0"
    >
      <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <Icon aria-hidden="true" className="size-3.5" />
        {kind}
      </span>
      {children}
    </section>
  );
}

function Tag({ children }: { children: ReactNode }) {
  return (
    <Badge className="rounded-md font-normal" variant="outline">
      {children}
    </Badge>
  );
}

// A running proposal is its deployment, so it takes the deployment's marker.
function markerKey(model: ModelProposal): string {
  return model.deploymentId ?? model.modelId;
}

/**
 * The catalog names of a proposal's model, profile and engine; ids stand in for missing ones, and
 * the engine is left out when the profile already carries its name.
 */
function modelNames(
  models: readonly Model[],
  proposal: ModelProposal,
): { engine?: string; model: string; profile: string } {
  const model = models.find((candidate) => candidate.modelId === proposal.modelId);
  const profile = model?.profiles.find((candidate) => candidate.profileId === proposal.profileId);
  const configuration = model?.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const engineKind = configuration?.engineConfiguration?.engineKind;
  const engineKey = engineKind === undefined ? undefined : engineKindKey(engineKind);
  const engine = engineKey && engineKindLabel(engineKey);
  const profileName = profile?.displayName || proposal.profileId;

  return {
    // A profile named after its engine says it once.
    engine: engine === profileName ? undefined : engine,
    model: model?.displayName || proposal.modelId,
    profile: profileName,
  };
}

/**
 * Why no machine has room: the models that hold the machines' memory, else the machines whose free
 * memory is unknown, else the machines' size.
 */
function noRoomReason(
  machines: readonly Machine[],
  modelDeployments: readonly ModelDeployment[],
): string {
  const busy = machines.flatMap((machine) => {
    const running = modelDeployments.filter(
      (deployment) =>
        deployment.desiredState !== ModelDeploymentDesiredState.TEARDOWN &&
        deploymentSiteNodeId(deployment) === machine.machineId,
    );
    if (!running.length) return [];

    const names = joinNames(running.map(modelDeploymentPresentationName));
    return [`${names} ${running.length === 1 ? "uses" : "use"} most of ${machineLabel(machine)}.`];
  });
  if (busy.length) return busy.join(" ");

  const silent = machines.filter(
    (machine) => machine.observationFreshness?.state !== NodeObservationState.CONNECTED,
  );
  if (silent.length) return `Free memory on ${joinNames(silent.map(machineLabel))} is unknown.`;

  return "The recommended models need more memory than the machines have.";
}

function machineLabel(machine: Machine): string {
  return placementMachineName(machine.machineId, machine);
}

function joinNames(names: readonly string[]): string {
  return names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names.at(-1)}` : names[0];
}

function acceleratorNames(vendors: readonly string[]): string {
  return vendors.map((vendor) => vendorNames[vendor] ?? vendor).join(" or ");
}

const vendorNames: Record<string, string> = { apple: "Apple", nvidia: "NVIDIA" };
