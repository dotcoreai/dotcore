import type { RefObject } from "react";

import { DeploymentListSection, DeploymentRow } from "@/components/deployment-list/deployment-list";
import { InspectorActions } from "@/components/inspector-actions/inspector-actions";
import { DeployActionIcon } from "@/components/ui/action-icons";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceTitle } from "@/components/ui/resource-marker";
import { applicationDeploymentPresentationName } from "@/domain/application-deployment-name";
import { projectApplicationDeploymentStatus } from "@/domain/application-projection";
import { placementLabel } from "@/domain/placement";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import type { Application } from "@/gen/dotcore/catalog/v1/application_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { formatCount } from "@/lib/format";

import { applicationInspectorPanelId } from "./application-inspector-panel";

export interface CatalogApplicationInspectorProps {
  application: Application;
  deployOpenerRef?: RefObject<HTMLButtonElement | null>;
  /**
   * The application's deployments, listed when `onOpenDeployment` can open them; undefined until
   * the site's deployments are read.
   */
  deployments?: readonly ApplicationDeployment[];
  /** Whether the site's deployments could not be read, so the list is unreachable. */
  deploymentsUnreachable?: boolean;
  error?: string;
  /** The site's configured execution targets, undefined until read; they name placements. */
  executionTargets?: readonly ConfiguredExecutionTarget[];
  /** Returns to the deployment inspector beneath, when the application was opened from one. */
  onBack?: () => void;
  onDeploy: () => void;
  onOpenChange: (open: boolean) => void;
  onOpenDeployment?: (applicationDeploymentId: string) => void;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
  /** Name of the deployment inspector beneath, for the Back control. */
  pending: boolean;
  suspendDismissal?: boolean;
}

export function CatalogApplicationInspector({
  application,
  deployOpenerRef,
  deployments,
  deploymentsUnreachable = false,
  error,
  executionTargets,
  onBack,
  onDeploy,
  onOpenChange,
  onOpenDeployment,
  open,
  openerRef,
  pending,
  suspendDismissal,
}: CatalogApplicationInspectorProps) {
  return (
    <InspectorShell
      body={
        <div className="inspector-sections">
          {error ? (
            <Notice role="alert" tone="destructive">
              {error}
            </Notice>
          ) : null}
          <section aria-labelledby="catalog-application-details-heading">
            <InspectorSectionHeader
              headingId="catalog-application-details-heading"
              meta={formatCount(application.graph?.services.length ?? 0, "service")}
              title="Catalog application"
            />
            {application.description ? (
              <p className="mt-2 text-[12.5px] text-muted-foreground">{application.description}</p>
            ) : null}
            <DetailList>
              <Detail label="Application ID" value={application.applicationId} />
              <Detail label="Version" value={application.version} />
            </DetailList>
          </section>
          {onOpenDeployment && (deployments || deploymentsUnreachable) ? (
            <DeploymentListSection
              count={deployments?.length ?? 0}
              headingId="catalog-application-deployments-heading"
              listLabel={`Deployments of ${application.displayName}`}
              title="Deployments"
              unreachable={deploymentsUnreachable}
            >
              {deployments?.map((deployment) => (
                <DeploymentRow
                  description={[placementLabel(deployment.placement, undefined, executionTargets)]}
                  key={deployment.applicationDeploymentId}
                  name={applicationDeploymentPresentationName(deployment)}
                  onSelect={() => onOpenDeployment(deployment.applicationDeploymentId)}
                  status={projectApplicationDeploymentStatus(deployment)}
                />
              ))}
            </DeploymentListSection>
          ) : null}
          {application.notes ? (
            <section aria-labelledby="catalog-application-notes-heading">
              <InspectorSectionHeader headingId="catalog-application-notes-heading" title="Notes" />
              <p className="mt-2 whitespace-pre-wrap text-[12.5px] text-muted-foreground [overflow-wrap:anywhere]">
                {application.notes}
              </p>
            </section>
          ) : null}
        </div>
      }
      footer={
        <InspectorActions
          primary={{
            focusRef: deployOpenerRef,
            icon: DeployActionIcon,
            label: "Deploy",
            onSelect: onDeploy,
            pending,
            pendingLabel: "Deploying…",
          }}
        />
      }
      id={applicationInspectorPanelId}
      onBack={onBack}
      onOpenChange={onOpenChange}
      open={open}
      openerRef={openerRef}
      suspendDismissal={suspendDismissal}
      title={<ResourceTitle colorKey={application.applicationId} name={application.displayName} />}
    />
  );
}
