import { Code, ExternalLink } from "lucide-react";
import { useState } from "react";

import { consoleInferenceOrigin } from "@/api/inference-origin";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { CodeCopyControl } from "@/components/ui/copy-control";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { AppsSectionIcon, ModelsSectionIcon } from "@/components/ui/section-icons";
import { SegmentedControl } from "@/components/ui/segmented-control";
import { StatusBadge } from "@/components/ui/status-badge";
import { applicationDeploymentPresentationName } from "@/domain/application-deployment-name";
import {
  applicationDeploymentEndpoint,
  projectApplicationDeploymentStatus,
} from "@/domain/application-projection";
import {
  deploymentSiteNodeId,
  localModelSnapshot,
  modelDeploymentPresentationName,
  projectModelDeploymentStatus,
} from "@/domain/model-projection";
import { placementMachine, placementMachineName } from "@/domain/placement";
import { deploymentChatTarget } from "@/features/chat/chattable-route";
import { useChatAction } from "@/features/chat/use-chat-action";
import {
  buildModelConnectionExamples,
  type ModelConnectionExamples,
} from "@/features/inference-routes/model-connection";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import { Item } from "./proposal-card";

/**
 * What the operator does with what Get started deployed: chat on the model, open the app, and for
 * coding the commands that start a coding agent on the model's route.
 */
export function ReadyStep({
  applicationDeployment,
  machines,
  modelDeployment,
  routes,
  useCase,
}: {
  applicationDeployment?: ApplicationDeployment;
  machines: readonly Machine[];
  modelDeployment?: ModelDeployment;
  routes: readonly InferenceRouteView[];
  useCase: UseCase;
}) {
  const chatTarget = modelDeployment
    ? deploymentChatTarget(
        routes,
        modelDeployment.modelDeploymentId,
        localModelSnapshot(modelDeployment.snapshot)?.requestKinds ?? [],
      )
    : undefined;
  const route = chatTarget?.route;
  const examples = route
    ? buildModelConnectionExamples(consoleInferenceOrigin(), route)
    : undefined;

  return (
    <div className="grid gap-4">
      <Card aria-label="Ready" className="gap-0 px-5 py-4.5" role="region">
        {modelDeployment ? (
          <ReadyModel
            chatTarget={chatTarget}
            deployment={modelDeployment}
            machines={machines}
            route={route}
          />
        ) : null}
        {applicationDeployment ? <ReadyApp deployment={applicationDeployment} /> : null}
      </Card>
      {useCase === UseCase.CODING && examples ? <CodingAgent examples={examples} /> : null}
    </div>
  );
}

function ReadyModel({
  chatTarget,
  deployment,
  machines,
  route,
}: {
  chatTarget: ReturnType<typeof deploymentChatTarget> | undefined;
  deployment: ModelDeployment;
  machines: readonly Machine[];
  route: string | undefined;
}) {
  const chat = useChatAction(chatTarget);
  const siteNodeId = deploymentSiteNodeId(deployment);

  return (
    <Item icon={ModelsSectionIcon} kind="Model">
      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex items-center text-[15px] font-semibold">
          <ResourceMarker colorKey={deployment.modelDeploymentId} />
          {modelDeploymentPresentationName(deployment)}
        </span>
        <StatusBadge status={projectModelDeploymentStatus(deployment)} />
        {chat ? (
          <Button
            className="ml-auto"
            disabled={chat.disabled}
            onClick={chat.onSelect}
            size="sm"
            title={chat.disabledReason}
            type="button"
            variant="outline"
          >
            {chat.icon ? <chat.icon aria-hidden="true" data-icon="inline-start" /> : null}
            {chat.label}
          </Button>
        ) : null}
      </div>
      <DetailList>
        {route ? <Detail label="Route" value={<code>{route}</code>} /> : null}
        {siteNodeId ? (
          <Detail
            label="Machine"
            value={
              <>
                <ResourceMarker colorKey={siteNodeId} />
                {placementMachineName(siteNodeId, placementMachine(machines, siteNodeId))}
              </>
            }
          />
        ) : null}
      </DetailList>
    </Item>
  );
}

function ReadyApp({ deployment }: { deployment: ApplicationDeployment }) {
  const endpoint = applicationDeploymentEndpoint(deployment);
  const name = applicationDeploymentPresentationName(deployment);

  return (
    <Item icon={AppsSectionIcon} kind="App">
      <div className="flex flex-wrap items-center gap-2">
        <span className="inline-flex items-center text-[15px] font-semibold">
          <ResourceMarker colorKey={deployment.applicationDeploymentId} />
          {name}
        </span>
        <StatusBadge status={projectApplicationDeploymentStatus(deployment)} />
        {endpoint ? (
          <a
            aria-label={`Open ${name}`}
            className={buttonVariants({ className: "ml-auto", size: "sm", variant: "outline" })}
            href={endpoint}
            rel="noopener noreferrer"
            target="_blank"
          >
            <ExternalLink aria-hidden="true" data-icon="inline-start" />
            Open
          </a>
        ) : null}
      </div>
      {endpoint ? (
        <DetailList>
          <Detail label="Address" value={<code>{endpoint}</code>} />
        </DetailList>
      ) : null}
    </Item>
  );
}

/** The command that starts a coding agent on the model's route, for the agent the operator picks. */
function CodingAgent({ examples }: { examples: ModelConnectionExamples }) {
  const [agent, setAgent] = useState<CodingAgentId>("opencode");
  const { command, label } =
    codingAgents.find((option) => option.value === agent) ?? codingAgents[0];

  return (
    <Card className="gap-0 px-5 py-4.5">
      <Item icon={Code} kind="Coding agent">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <SegmentedControl
            aria-label="Coding agent"
            onValueChange={setAgent}
            options={codingAgents}
            value={agent}
          />
          <CodeCopyControl label={`${label} command`} value={command(examples)} />
        </div>
        <code className="overflow-x-auto rounded-md bg-muted px-3 py-2 text-xs whitespace-pre">
          {command(examples)}
        </code>
      </Item>
    </Card>
  );
}

type CodingAgentId = "codex" | "opencode" | "pi";

/** The coding agents the ready step offers, each with its command among the connection examples. */
const codingAgents: readonly {
  command: (examples: ModelConnectionExamples) => string;
  label: string;
  value: CodingAgentId;
}[] = [
  { command: (examples) => examples.opencodeCommand, label: "OpenCode", value: "opencode" },
  { command: (examples) => examples.piCommand, label: "Pi", value: "pi" },
  { command: (examples) => examples.codexCommand, label: "Codex", value: "codex" },
];
