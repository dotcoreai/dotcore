import { ArrowRightIcon } from "lucide-react";
import { type RefObject, useRef, useState } from "react";

import type { ApplicationClient } from "@/api/application-client";
import type { ModelClient } from "@/api/model-client";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { AppsSectionIcon, ModelsSectionIcon } from "@/components/ui/section-icons";
import { StatusBadge } from "@/components/ui/status-badge";
import { applicationDeploymentPresentationName } from "@/domain/application-deployment-name";
import {
  applicationLifecycleLabel,
  applicationLifecycleTone,
  projectApplicationDeploymentStatus,
} from "@/domain/application-projection";
import {
  modelDeploymentPresentationName,
  modelStatusLabel,
  modelStatusTone,
  projectModelDeploymentStatus,
} from "@/domain/model-projection";
import { ApplicationSteps } from "@/features/applications/application-inspector";
import { DeploymentLogViewer } from "@/features/deployment-logs/deployment-log-viewer";
import { DeploymentSteps } from "@/features/models/inspector/deployment-overview";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import { ApplicationLifecycleState } from "@/gen/dotcore/application/v1/state_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import type { LifecycleTone, StatusProjection } from "@/lib/status-projection";

import { Item } from "./proposal-card";

/**
 * The deployments Get started started, each with its status and step bar, and a link to its logs.
 * A deployment not listed yet shows its name as queued. The page leaves this step on its own once
 * every deployment is ready; when one can't get there, Back returns to the proposal.
 */
export function DeployingStep({
  applicationClient,
  applicationDeployment,
  application,
  modelClient,
  modelDeployment,
  model,
  notice,
  onBack,
}: {
  applicationClient: ApplicationClient;
  applicationDeployment?: ApplicationDeployment;
  /** The app this step follows; absent when no app deploys. */
  application?: FollowedDeployment;
  modelClient: ModelClient;
  modelDeployment?: ModelDeployment;
  /** The model this step follows; absent when no model deploys. */
  model?: FollowedDeployment;
  /** What stopped the deployments, such as one removed elsewhere. */
  notice?: string;
  /** Returns to the proposal; absent while the deployments can still become ready. */
  onBack?: () => void;
}) {
  const [viewing, setViewing] = useState<LogTarget>();
  const openerRef = useRef<HTMLElement | null>(null);
  const openLogs = (target: LogTarget, opener: HTMLElement) => {
    openerRef.current = opener;
    setViewing(target);
  };

  return (
    <Card aria-label="Deployment progress" className="gap-0 px-5 py-4.5" role="status">
      {modelDeployment ? (
        <Item icon={ModelsSectionIcon} kind="Model">
          <DeploymentHead
            colorKey={modelDeployment.modelDeploymentId}
            logs={Boolean(modelClient.logs)}
            name={modelDeploymentPresentationName(modelDeployment)}
            onOpenLogs={(opener) =>
              openLogs(
                {
                  id: modelDeployment.modelDeploymentId,
                  kind: "model",
                  name: modelDeploymentPresentationName(modelDeployment),
                },
                opener,
              )
            }
            status={projectModelDeploymentStatus(modelDeployment)}
          />
          {modelDeployment.state?.steps?.steps.length ? (
            <DeploymentSteps
              className="mt-0"
              progress={modelDeployment.state.progress}
              steps={modelDeployment.state.steps}
            />
          ) : null}
        </Item>
      ) : model ? (
        <Item icon={ModelsSectionIcon} kind="Model">
          <DeploymentHead
            colorKey={model.id ?? model.name}
            name={model.name}
            status={queuedStatus(modelStatusLabel(QUEUED_MODEL), modelStatusTone(QUEUED_MODEL))}
          />
        </Item>
      ) : null}
      {applicationDeployment ? (
        <Item icon={AppsSectionIcon} kind="App">
          <DeploymentHead
            colorKey={applicationDeployment.applicationDeploymentId}
            logs={Boolean(applicationClient.logs)}
            name={applicationDeploymentPresentationName(applicationDeployment)}
            onOpenLogs={(opener) =>
              openLogs(
                {
                  id: applicationDeployment.applicationDeploymentId,
                  kind: "application",
                  name: applicationDeploymentPresentationName(applicationDeployment),
                },
                opener,
              )
            }
            status={projectApplicationDeploymentStatus(applicationDeployment)}
          />
          {applicationDeployment.state?.steps?.steps.length &&
          !applicationDeployment.state.observationStale ? (
            <ApplicationSteps className="mt-0" steps={applicationDeployment.state.steps} />
          ) : null}
        </Item>
      ) : application ? (
        <Item icon={AppsSectionIcon} kind="App">
          <DeploymentHead
            colorKey={application.id ?? application.name}
            name={application.name}
            status={queuedStatus(
              applicationLifecycleLabel(QUEUED_APPLICATION),
              applicationLifecycleTone(QUEUED_APPLICATION),
            )}
          />
        </Item>
      ) : null}
      {notice ? (
        <Notice className="mb-4" tone="warning">
          {notice}
        </Notice>
      ) : null}
      {onBack ? (
        <div className="mt-0.5 flex justify-end gap-2 border-t border-border pt-4">
          <Button onClick={onBack} size="console" type="button" variant="outline">
            Back
          </Button>
        </div>
      ) : null}
      <LogViewer
        applicationClient={applicationClient}
        modelClient={modelClient}
        onClose={() => setViewing(undefined)}
        openerRef={openerRef}
        target={viewing}
      />
    </Card>
  );
}

/** The deployments a use case follows, known by name before their creates answer. */
export interface FollowedDeployments {
  application?: FollowedDeployment;
  model?: FollowedDeployment;
}

/** One followed deployment: the name it shows until the watch lists it, and its id once created. */
export interface FollowedDeployment {
  id?: string;
  name: string;
}

const QUEUED_MODEL = ModelDeploymentStatus.QUEUED;
const QUEUED_APPLICATION = ApplicationLifecycleState.QUEUED;

// A deployment whose create hasn't answered or that the watch hasn't listed yet waits in line.
function queuedStatus(label: string, tone: LifecycleTone): StatusProjection {
  return { inProgress: true, label, tone };
}

interface LogTarget {
  id: string;
  kind: "application" | "model";
  name: string;
}

function DeploymentHead({
  colorKey,
  logs,
  name,
  onOpenLogs,
  status,
}: {
  colorKey: string;
  logs?: boolean;
  name: string;
  onOpenLogs?: (opener: HTMLElement) => void;
  status: Parameters<typeof StatusBadge>[0]["status"];
}) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <span className="inline-flex items-center text-[15px] font-semibold">
        <ResourceMarker colorKey={colorKey} />
        {name}
      </span>
      <StatusBadge status={status} />
      {logs && onOpenLogs ? (
        <Button
          className="ml-auto"
          onClick={(event) => onOpenLogs(event.currentTarget)}
          size="sm"
          type="button"
          variant="link"
        >
          Logs
          <ArrowRightIcon aria-hidden="true" className="size-3.5" />
        </Button>
      ) : null}
    </div>
  );
}

function LogViewer({
  applicationClient,
  modelClient,
  onClose,
  openerRef,
  target,
}: {
  applicationClient: ApplicationClient;
  modelClient: ModelClient;
  onClose: () => void;
  openerRef: RefObject<HTMLElement | null>;
  target?: LogTarget;
}) {
  const operations = target?.kind === "model" ? modelClient.logs : applicationClient.logs;
  if (!target || !operations) return null;

  return (
    <DeploymentLogViewer
      colorKey={target.id}
      deploymentId={target.id}
      name={target.name}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      open
      openerRef={openerRef}
      operations={operations}
    />
  );
}
