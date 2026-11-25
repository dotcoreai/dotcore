import { Check } from "lucide-react";

import { ResourceMarker } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { projectApplicationDeploymentStatus } from "@/domain/application-projection";
import {
  modelDeploymentPresentationName,
  modelStatusLabel,
  modelStatusTone,
  projectModelDeploymentStatus,
} from "@/domain/model-projection";
import type { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import {
  useWatchedApplicationDeployments,
  useWatchedModelDeployments,
} from "@/resource-watch/resource-watch";

import type { FollowedDeployments } from "./deploying-step";
import {
  isApplicationDeploymentFailed,
  isModelDeploymentFailed,
  type UseCaseProposal,
  type UseCaseStage,
} from "./get-started-proposals";
import { useCaseCopy } from "./use-case-copy";

/**
 * The use cases Get started offers, one pressable card each. A use case the site serves, or that
 * this page is deploying, says what runs it in place of its description. A followed use case takes
 * its status from the deployments the page follows.
 */
export function UseCaseCards({
  following,
  onSelect,
  proposals,
  selected,
}: {
  /** The deployments this page follows per use case; their use case counts as deploying. */
  following: ReadonlyMap<UseCase, FollowedDeployments>;
  onSelect: (useCase: UseCase) => void;
  proposals: readonly UseCaseProposal[];
  selected: UseCase;
}) {
  const modelDeployments = useWatchedModelDeployments()?.value?.deployments;
  const applicationDeployments = useWatchedApplicationDeployments()?.value?.deployments;
  // A followed use case takes its status from the deployments the page follows; one the watch
  // doesn't list yet, or lists while it starts, is deploying.
  const stageOf = (proposal: UseCaseProposal): UseCaseStage | "failed" => {
    if (proposal.stage === "failed") return "failed";

    const followed = following.get(proposal.useCase);
    if (!followed) return proposal.stage;

    const model = modelDeployments?.find(
      (deployment) => deployment.modelDeploymentId === followed.model?.id,
    );
    const application = applicationDeployments?.find(
      (deployment) => deployment.applicationDeploymentId === followed.application?.id,
    );
    if (
      (model && isModelDeploymentFailed(model)) ||
      (application && isApplicationDeploymentFailed(application))
    ) {
      return "failed";
    }
    if (proposal.stage === "ready") return "ready";

    const starting =
      (followed.model && (!model || projectModelDeploymentStatus(model).inProgress)) ||
      (followed.application &&
        (!application || projectApplicationDeploymentStatus(application).inProgress));

    return starting ? "deploying" : proposal.stage;
  };

  return (
    <div
      aria-label="Use case"
      className="grid grid-cols-[repeat(auto-fit,minmax(190px,1fr))] gap-2.5"
      role="group"
    >
      {proposals.map((proposal) => {
        const { useCase } = proposal;
        const { description, icon: Icon, label } = useCaseCopy[useCase];
        const followed = following.get(useCase);
        const stage = stageOf(proposal);

        return (
          <button
            aria-pressed={useCase === selected}
            className="grid gap-0.75 rounded-xl border-[1.5px] border-border bg-card px-3.5 py-3 text-left outline-none hover:border-foreground/25 focus-visible:ring-3 focus-visible:ring-ring/50 aria-pressed:border-primary aria-pressed:bg-info"
            key={useCase}
            onClick={() => onSelect(useCase)}
            type="button"
          >
            <span className="flex items-center gap-2 text-sm font-semibold">
              <Icon aria-hidden="true" className="size-4" />
              {label}
            </span>
            {stage !== "none" ? (
              <span className="flex flex-wrap items-center gap-x-1.5 gap-y-1 text-[12.5px] text-muted-foreground">
                {stage === "ready" ? (
                  <Check
                    aria-label="Ready"
                    className="size-3.5 text-success-foreground"
                    role="img"
                  />
                ) : stage === "failed" ? (
                  <StatusBadge
                    status={{
                      label: modelStatusLabel(ModelDeploymentStatus.FAILED),
                      tone: modelStatusTone(ModelDeploymentStatus.FAILED),
                    }}
                  />
                ) : (
                  <StatusBadge status={{ inProgress: true, label: "Deploying", tone: "info" }} />
                )}
                {runningNames(proposal, followed).map((item, index) => (
                  <span className="inline-flex items-center" key={item.key}>
                    {index > 0 ? <span className="mr-1.5">·</span> : null}
                    <ResourceMarker colorKey={item.key} />
                    {item.name}
                  </span>
                ))}
              </span>
            ) : (
              <span className="text-[12.5px] text-muted-foreground">{description}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}

/** What runs a use case, model first: the followed names when this page follows it. */
function runningNames(proposal: UseCaseProposal, followed: FollowedDeployments | undefined) {
  const { app, failed, model } = proposal;
  const items: { key: string; name: string }[] = [];

  const modelName =
    followed?.model ??
    (model?.deploymentId
      ? {
          id: model.deploymentId,
          name: model.deployment
            ? modelDeploymentPresentationName(model.deployment)
            : model.modelId,
        }
      : failed?.model && {
          id: failed.model.modelDeploymentId,
          name: modelDeploymentPresentationName(failed.model),
        });
  if (modelName) items.push({ key: modelName.id ?? modelName.name, name: modelName.name });

  const appName =
    followed?.application ??
    (app?.target?.applicationDeploymentId
      ? {
          id: app.target.applicationDeploymentId,
          name: app.application.displayName || app.application.applicationId,
        }
      : undefined);
  if (appName) items.push({ key: appName.id ?? appName.name, name: appName.name });

  return items;
}
