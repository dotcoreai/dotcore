import type { RefObject } from "react";

import { Detail } from "@/components/ui/detail-list";
import { Notice } from "@/components/ui/notice";
import { StatusBadge } from "@/components/ui/status-badge";
import { parameterDisplayValue, type ParameterViewValue } from "@/domain/parameter-view";
import { projectRouteHealth } from "@/domain/route-projection";
import { DeploymentSummaryStep } from "@/features/deployments/deployment-summary-step";
import type { StoragePlacement } from "@/gen/dotcore/catalog/v1/storage_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";

import { formatGpuDevices } from "../application-gpu-devices";
import type { ApplicationParameterField } from "../application-parameters";
import {
  type StorageChoiceField,
  storageChoiceKey,
  storagePlacementLabel,
} from "../application-storage";

interface ApplicationDeploySummaryStepProps {
  /** The parameters the operator changed from their catalog defaults, with their values. */
  changedParameters: readonly { field: ApplicationParameterField; value: ParameterViewValue }[];
  /** The storage bindings the operator moved off their catalog default. */
  changedStorage: readonly { field: StorageChoiceField; placement: StoragePlacement }[];
  disabled: boolean;
  name: string;
  nameError?: string;
  nameRef: RefObject<HTMLInputElement | null>;
  onNameBlur: () => void;
  onNameChange: (name: string) => void;
  /** The route link, for an application that takes a route. */
  route?: {
    /** Why the link blocks deploying, such as a removed route. */
    error?: string;
    selected?: InferenceRouteView;
    selectedId: string;
  };
  target?: ConfiguredExecutionTarget;
  /** Why the chosen target blocks deploying, and where to choose another. */
  targetNotice?: string;
  /** The chosen target's name, or what stands in for it. */
  targetLabel: string;
}

/**
 * The Deploy step of the application deploy dialog: what blocks deploying, the deployment name,
 * and a summary of the route link, the target and every setting changed from its default.
 */
export function ApplicationDeploySummaryStep({
  changedParameters,
  changedStorage,
  disabled,
  name,
  nameError,
  nameRef,
  onNameBlur,
  onNameChange,
  route,
  target,
  targetLabel,
  targetNotice,
}: ApplicationDeploySummaryStepProps) {
  return (
    <DeploymentSummaryStep
      disabled={disabled}
      name={name}
      nameError={nameError}
      nameInputId="application-deployment-name"
      nameRef={nameRef}
      notices={
        <>
          {targetNotice ? (
            <Notice aria-label="Deploy execution target status" role="alert" tone="destructive">
              {targetNotice}
            </Notice>
          ) : null}
          {route?.error ? (
            <Notice aria-label="Deploy route status" role="alert" tone="destructive">
              {route.error} Return to Inference route to change the link.
            </Notice>
          ) : null}
        </>
      }
      onNameBlur={onNameBlur}
      onNameChange={onNameChange}
    >
      {route ? (
        <Detail
          label="Inference route"
          value={
            route.selected?.route ? (
              <span className="inline-flex flex-wrap items-center justify-end gap-2">
                {route.selected.route.name}
                <StatusBadge status={projectRouteHealth(route.selected).status} />
              </span>
            ) : route.selectedId ? (
              "Removed route"
            ) : (
              "None"
            )
          }
        />
      ) : null}
      <Detail label="Target" value={targetLabel} />
      {target ? (
        <Detail label="GPU devices" value={formatGpuDevices(target.acceleratorDeviceIds)} />
      ) : null}
      {changedParameters.map(({ field, value }) => (
        <Detail
          key={field.view.id}
          label={field.view.label}
          value={parameterDisplayValue(field.view, value)}
        />
      ))}
      {changedStorage.map(({ field, placement }) => (
        <Detail
          key={storageChoiceKey(field)}
          label={field.displayName}
          value={storagePlacementLabel(placement)}
        />
      ))}
    </DeploymentSummaryStep>
  );
}
