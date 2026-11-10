import type { RefObject } from "react";

import { DeploymentListSection, DeploymentRow } from "@/components/deployment-list/deployment-list";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { DeployActionIcon } from "@/components/ui/action-icons";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceTitle } from "@/components/ui/resource-marker";
import {
  localModelSnapshot,
  modelDeploymentPresentationName,
  projectModelDeploymentStatus,
} from "@/domain/model-projection";
import { placementLabel } from "@/domain/placement";
import { requestKindsLabel } from "@/domain/request-kinds";
import type { ModelRow } from "@/features/models/profiles/model-row";
import {
  type CatalogProfileCompatibility,
  profileDeployBlockedReason,
} from "@/features/models/profiles/profile-availability";
import type { CatalogChips } from "@/features/models/profiles/profile-table-seed";
import { type Model } from "@/gen/dotcore/catalog/v1/model_pb";
import { type ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";
import { type ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

import { modelInspectorPanelId } from "./model-inspector-panel";
import { ModelIntelligenceSection } from "./model-intelligence-section";
import { ModelLicenseDetail } from "./model-license-detail";
import { CatalogProfiles, CatalogProfileSections } from "./profile-sections";

/**
 * The catalog model panel: the model's details, its deployments and its profiles on the overview,
 * and one profile's sections on the profile screen, with Deploy as the primary action.
 */
export function CatalogModelInspector({
  catalogChips,
  compatibility,
  deployOpenerRef,
  deployments = [],
  error,
  executionTargets,
  memoryPending,
  model,
  onBack,
  onDeploy,
  onOpenChange,
  onOpenDeployment,
  onSelectProfile,
  open,
  openerRef,
  pending,
  profileId,
  row,
  screen = "overview",
  suspendDismissal,
}: CatalogModelInspectorProps) {
  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const profileRow = row.profiles.find((candidate) => candidate.profileId === profileId);
  const deployBlocked =
    screen === "profile-detail" && profileId
      ? profileDeployBlockedReason(compatibility?.compatibility, model.modelId, profileId)
      : undefined;

  return (
    <InspectorShell
      body={
        <div className="inspector-sections">
          {error ? (
            <Notice role="alert" tone="destructive">
              {error}
            </Notice>
          ) : null}
          {screen === "overview" ? (
            <CatalogModelOverview
              catalogChips={catalogChips}
              compatibility={compatibility}
              deployments={deployments}
              executionTargets={executionTargets}
              memoryPending={memoryPending}
              model={model}
              onOpenDeployment={onOpenDeployment}
              onSelectProfile={onSelectProfile}
              row={row}
            />
          ) : null}
          {screen === "profile-detail" && profile && profileRow ? (
            <CatalogProfileSections
              compatibility={compatibility}
              memoryPending={memoryPending}
              model={model}
              profile={profile}
              row={profileRow}
            />
          ) : null}
        </div>
      }
      focusKey={`${screen}:${profileId ?? model.modelId}`}
      footer={
        <InspectorActions
          primary={{
            disabled: pending || Boolean(deployBlocked),
            disabledReason: deployBlocked,
            focusRef: deployOpenerRef,
            icon: DeployActionIcon,
            label: "Deploy",
            onSelect: onDeploy,
            pending,
            pendingLabel: "Deploying…",
          }}
        />
      }
      id={modelInspectorPanelId}
      onBack={onBack}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      suspendDismissal={suspendDismissal}
      title={
        <ResourceTitle
          colorKey={model.modelId}
          name={model.displayName}
          screen={screen === "profile-detail" ? profileId : undefined}
        />
      }
    />
  );
}

export type CatalogModelInspectorScreen = "overview" | "profile-detail";

interface CatalogModelInspectorProps {
  /** The catalog's chips the profile table starts from; the catalog's default chips without them. */
  catalogChips?: CatalogChips;
  /** The model's profile compatibility on the site; absent when the site can't report it. */
  compatibility?: CatalogProfileCompatibility;
  deployOpenerRef?: RefObject<HTMLButtonElement | null>;
  /** The model's deployments, listed on the overview when `onOpenDeployment` can open them. */
  deployments?: readonly ModelDeployment[];
  error?: string;
  /** The site's configured execution targets, undefined until read; they name placements. */
  executionTargets?: readonly ConfiguredExecutionTarget[];
  /** Whether profile memory estimates are still loading. */
  memoryPending: boolean;
  model: Model;
  onBack?: () => void;
  onDeploy: () => void;
  onOpenChange: (open: boolean) => void;
  onOpenDeployment?: (modelDeploymentId: string) => void;
  onSelectProfile?: (profileId: string) => void;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
  pending: boolean;
  profileId?: string;
  /** The model's table projection, the source of profile names, engines, and memory values. */
  row: ModelRow;
  screen?: CatalogModelInspectorScreen;
  suspendDismissal?: boolean;
}

function CatalogModelOverview({
  catalogChips,
  compatibility,
  deployments,
  executionTargets,
  memoryPending,
  model,
  onOpenDeployment,
  onSelectProfile,
  row,
}: {
  catalogChips?: CatalogChips;
  compatibility?: CatalogProfileCompatibility;
  deployments: readonly ModelDeployment[];
  executionTargets?: readonly ConfiguredExecutionTarget[];
  memoryPending: boolean;
  model: Model;
  onOpenDeployment?: (modelDeploymentId: string) => void;
  onSelectProfile?: (profileId: string) => void;
  row: ModelRow;
}) {
  return (
    <>
      <section aria-labelledby="catalog-model-details-heading">
        <InspectorSectionHeader headingId="catalog-model-details-heading" title="Catalog model" />
        <DetailList>
          <Detail label="Version" value={model.version} />
          <Detail label="Model ID" value={model.modelId} />
          <Detail label="Request kinds" value={requestKindsLabel(row.requestKinds) || "None"} />
          <ModelLicenseDetail model={model} />
        </DetailList>
      </section>
      {onOpenDeployment ? (
        <DeploymentListSection
          count={deployments.length}
          headingId="catalog-model-deployments-heading"
          listLabel={`Deployments of ${model.displayName}`}
          title="Deployments"
        >
          {deployments.map((deployment) => (
            <DeploymentRow
              description={[
                localModelSnapshot(deployment.snapshot)?.profileId,
                placementLabel(deployment.intent?.placement, undefined, executionTargets),
              ]}
              key={deployment.modelDeploymentId}
              name={modelDeploymentPresentationName(deployment)}
              onSelect={() => onOpenDeployment(deployment.modelDeploymentId)}
              status={projectModelDeploymentStatus(deployment)}
            />
          ))}
        </DeploymentListSection>
      ) : null}
      <ModelIntelligenceSection model={model} />
      <CatalogProfiles
        catalogChips={catalogChips}
        compatibility={compatibility}
        memoryPending={memoryPending}
        onSelectProfile={onSelectProfile}
        row={row}
      />
    </>
  );
}
