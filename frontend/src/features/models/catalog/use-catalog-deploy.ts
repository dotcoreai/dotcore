import { create } from "@bufbuild/protobuf";
import { ConnectError } from "@connectrpc/connect";
import { type RefObject, useEffect, useRef, useState } from "react";

import type { ModelClient } from "@/api/model-client";
import type { ModelDeployDialogProps } from "@/features/models/deploy/model-deploy-dialog";
import { type RouteNameConflict } from "@/features/models/deployment-exposure";
import {
  describeMutationError,
  routeConflictFromError,
} from "@/features/models/model-mutation-errors";
import type { ModelsPolling } from "@/features/models/polling/models-polling-context";
import {
  defaultMemoryBasis,
  type MemoryBasis,
} from "@/features/models/profiles/profile-compatibility";
import type { CatalogChips } from "@/features/models/profiles/profile-table-seed";
import type { SizingScenario } from "@/features/models/profiles/sizing-scenario";
import {
  toUpstreamConfiguration,
  type UpstreamFormValues,
} from "@/features/models/upstream/upstream-form";
import { type Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { UpstreamModelTemplate } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type CreateLocalModelDeploymentRequest,
  CreateLocalModelDeploymentRequestSchema,
  EstimateLocalModelResourcesRequestSchema,
  type LocalModelDeploymentAdvisory,
  LocalModelDeploymentConfigurationSchema,
  type ModelDeploymentExposure,
} from "@/gen/dotcore/model/v1/service_pb";
import { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import type { usePanelNavigation } from "@/panels/panel-navigation";
import { panelPagePath, serializePanelStack } from "@/panels/panel-stack";

import { type CatalogSelection, isSameSelection } from "./use-catalog-selection";

/**
 * The Catalog page's deploys: a catalog model through the deploy dialog and an upstream model
 * through its connect dialog. It tracks each model's pending deploy and the errors to show, and
 * opens an accepted deployment's panel on the Models page.
 */
export function useCatalogDeploy({
  applyDeploymentOverlay,
  client,
  panelNavigation,
  selectionRef,
}: CatalogDeployOptions) {
  const [deployTemplate, setDeployTemplate] = useState<UpstreamModelTemplate>();
  const [deployModel, setDeployModel] = useState<Model>();
  const [deployProfileId, setDeployProfileId] = useState<string>();
  // The catalog's chips the deploy dialog's profile table starts from, as the model panel took them.
  const [deployChips, setDeployChips] = useState<CatalogChips>();
  const [deployScenario, setDeployScenario] = useState<SizingScenario>();
  const [deployBasis, setDeployBasis] = useState<MemoryBasis>(defaultMemoryBasis);
  const [upstreamDeployPending, setUpstreamDeployPending] = useState(false);
  const [upstreamError, setUpstreamError] = useState<string>();
  const [routeConflict, setRouteConflict] = useState<RouteNameConflict>();
  const [deployError, setDeployError] = useState<DeployError>();
  const [pendingActions, setPendingActions] = useState<ReadonlySet<string>>(() => new Set());
  const pendingActionsRef = useRef<Set<string>>(new Set());
  const deployTemplateRef = useRef<UpstreamModelTemplate | undefined>(undefined);
  const upstreamDeployPendingRef = useRef(false);
  const mountedRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const updatePending = (key: string, pending: boolean) => {
    if (pending) {
      pendingActionsRef.current.add(key);
    } else {
      pendingActionsRef.current.delete(key);
    }

    setPendingActions(new Set(pendingActionsRef.current));
  };

  /**
   * Opens an accepted deployment's panel on the Models page as a new history entry, so Back returns
   * to the catalog inspector the deploy started from.
   */
  const openAcceptedDeployment = (modelDeploymentId: string) => {
    // A deploy accepted after the operator left the page keeps them where they went.
    if (!mountedRef.current) return;

    panelNavigation.navigatePanel(
      {
        pathname: panelPagePath("models"),
        // The pathname changes, so the Catalog view parameters stay behind.
        search: serializePanelStack("", [
          { page: "models", resourceId: modelDeploymentId, screen: "deployment" },
        ]),
      },
      { bypassPendingGuard: true },
    );
  };

  const load = async (
    model: Model,
    profileId: string,
    configuration: CreateLocalModelDeploymentRequest["configuration"],
    placement: CreateLocalModelDeploymentRequest["placement"],
    exposure: ModelDeploymentExposure,
    name: string,
  ) => {
    const initiatingSelection = selectionRef.current;
    if (!initiatingSelection) return;
    const pendingKey = catalogPendingKey(model.modelId);
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    setDeployError(undefined);
    setRouteConflict(undefined);
    updatePending(pendingKey, true);
    try {
      if (!client.model.createLocalModelDeployment) {
        throw new Error(
          "Deploy is unavailable because the model service does not support explicit local deployments.",
        );
      }
      const response = await client.model.createLocalModelDeployment({
        ...create(CreateLocalModelDeploymentRequestSchema, {
          modelId: model.modelId,
          profileId,
          placement,
          configuration,
        }),
        exposure,
        modelDeploymentName: name,
      });
      if (!isSameSelection(selectionRef.current, initiatingSelection)) {
        return;
      }

      const deployment = response.deployment;
      if (!deployment?.modelDeploymentId) {
        setDeployError({
          message: "Deploy response protocol error: missing accepted deployment ID.",
          selection: initiatingSelection,
        });
        return;
      }

      setDeployModel(undefined);
      setDeployProfileId(undefined);
      applyDeploymentOverlay(deployment);
      openAcceptedDeployment(deployment.modelDeploymentId);
    } catch (mutationError) {
      const conflict = routeConflictFromError(mutationError);
      if (conflict) {
        setRouteConflict(conflict);
      } else if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setDeployError({
          message: describeMutationError("Deploy", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const deployUpstream = async (template: UpstreamModelTemplate, values: UpstreamFormValues) => {
    if (upstreamDeployPendingRef.current) return;
    upstreamDeployPendingRef.current = true;
    setUpstreamDeployPending(true);
    setUpstreamError(undefined);
    setRouteConflict(undefined);
    try {
      const response = await client.model.createModelDeployment({
        modelDeploymentName: values.name?.trim(),
        source: {
          case: "upstream",
          value: {
            upstreamModelTemplateId: template.upstreamModelTemplateId,
            desiredConfiguration: toUpstreamConfiguration(values),
          },
        },
        exposure:
          values.exposure === "direct"
            ? { kind: { case: "direct", value: { routeName: values.routeName?.trim() ?? "" } } }
            : { kind: { case: "hidden", value: {} } },
      });
      const deployment = response.deployment;
      if (!deployment?.modelDeploymentId.trim()) {
        throw new Error(
          "Deploy response protocol error: service returned no deployment ID. Keep the dialog open and retry.",
        );
      }
      if (deployTemplateRef.current?.upstreamModelTemplateId !== template.upstreamModelTemplateId) {
        return;
      }
      deployTemplateRef.current = undefined;
      setDeployTemplate(undefined);
      applyDeploymentOverlay(deployment);
      openAcceptedDeployment(deployment.modelDeploymentId);
    } catch (error) {
      const conflict = routeConflictFromError(error);
      if (conflict) {
        setRouteConflict(conflict);
      } else {
        setUpstreamError(ConnectError.from(error).rawMessage || "Deploy was rejected.");
      }
    } finally {
      upstreamDeployPendingRef.current = false;
      setUpstreamDeployPending(false);
    }
  };

  // The dialog starts from the chips the model panel copied when it opened, not the catalog's
  // current ones.
  const startDeploy = (
    model: Model,
    profileId: string | undefined,
    chips: CatalogChips,
    deployScenario: SizingScenario | undefined,
    basis: MemoryBasis,
  ) => {
    setDeployProfileId(profileId);
    setDeployChips(chips);
    setDeployScenario(deployScenario);
    setDeployBasis(basis);
    setDeployModel(model);
  };

  // A pending deploy keeps the dialog open.
  const closeDeployDialog = (model: Model) => {
    if (!pendingActionsRef.current.has(catalogPendingKey(model.modelId))) {
      setDeployModel(undefined);
      setDeployProfileId(undefined);
      setRouteConflict(undefined);
    }
  };

  const selectUpstreamTemplate = (template: UpstreamModelTemplate) => {
    setUpstreamError(undefined);
    setRouteConflict(undefined);
    deployTemplateRef.current = template;
    setDeployTemplate(template);
  };

  // A pending upstream deploy keeps the dialog open.
  const closeUpstreamDialog = () => {
    if (!upstreamDeployPendingRef.current) {
      deployTemplateRef.current = undefined;
      setDeployTemplate(undefined);
      setUpstreamError(undefined);
      setRouteConflict(undefined);
    }
  };

  const deployPending = (modelId: string) => pendingActions.has(catalogPendingKey(modelId));

  return {
    closeDeployDialog,
    closeUpstreamDialog,
    deployBasis,
    deployChips,
    deployError,
    deployModel,
    deployPending,
    deployProfileId,
    deployScenario,
    deployTemplate,
    deployUpstream,
    load,
    routeConflict,
    selectUpstreamTemplate,
    setDeployError,
    setDeployModel,
    setDeployProfileId,
    setUpstreamError,
    startDeploy,
    upstreamDeployPending,
    upstreamError,
  };
}

export interface CatalogDeployOptions {
  applyDeploymentOverlay: ModelsPolling["applyDeploymentOverlay"];
  client: ModelClient;
  panelNavigation: ReturnType<typeof usePanelNavigation>;
  /** The latest selection; a deploy that resolves after it changed leaves the page alone. */
  selectionRef: RefObject<CatalogSelection | undefined>;
}

/** A deploy failure, shown on the selection it was started from. */
export interface DeployError {
  message: string;
  selection: CatalogSelection;
}

/**
 * The deploy dialog's reads for one catalog model: the resource estimate, the command preview and
 * the background memory preview of a local deployment.
 */
export function modelDeployRequests(
  client: ModelClient,
  model: Model,
): Required<Pick<ModelDeployDialogProps, "onEstimate" | "onPreview" | "onPreviewMemory">> {
  return {
    onEstimate: async ({ configuration, placement, profileId }) => {
      if (!client.model.estimateLocalModelResources)
        throw new Error("The model service does not support local resource estimates.");
      const response = await client.model.estimateLocalModelResources(
        create(EstimateLocalModelResourcesRequestSchema, {
          modelId: model.modelId,
          profileId,
          configuration: create(LocalModelDeploymentConfigurationSchema, configuration),
          placement,
        }),
      );
      // Without a placement, a profile supporting several hardware classes gets one
      // estimate per class, and only a target tells which one applies.
      const [estimate, ...others] = response.hardwareClassEstimates;
      if (others.length) throw new Error("it depends on the selected target's hardware");
      return {
        launchRejection: estimate?.launchRejection,
        placementPreview: response.placementPreview,
      };
    },
    onPreview: async (values) =>
      (await previewLocalDeployment(client, model.modelId, values)).snapshot,
    onPreviewMemory: async (values) => {
      const { advisories, snapshot } = await previewLocalDeployment(client, model.modelId, values);
      const minimum = snapshot.hardwareSupport?.hardwareRequirements?.minimumMemoryBytesPerDevice;
      return {
        acceleratorMemoryArchitecture:
          snapshot.selectedTarget?.acceleratorMemoryArchitecture ??
          AcceleratorMemoryArchitecture.UNSPECIFIED,
        advisories,
        engineRelease: snapshot.engineArtifact && {
          engineReleaseId: snapshot.engineArtifact.engineReleaseId,
          version: snapshot.engineArtifact.engineVersion,
        },
        memoryPoolTotalBytes: snapshot.selectedTarget?.acceleratorMemoryPoolTotalBytes,
        minimumMemoryBytesPerDevice: minimum || undefined,
      };
    },
  };
}

type LocalDeploymentPreviewValues = Pick<
  CreateLocalModelDeploymentRequest,
  "placement" | "profileId"
> & {
  configuration: Parameters<typeof create<typeof LocalModelDeploymentConfigurationSchema>>[1];
};

/**
 * Resolves a local deployment without creating it, returning the resolved snapshot and the
 * non-blocking advisories the model service reports for it.
 */
async function previewLocalDeployment(
  client: ModelClient,
  modelId: string,
  { configuration, placement, profileId }: LocalDeploymentPreviewValues,
): Promise<{
  advisories: readonly LocalModelDeploymentAdvisory[];
  snapshot: ResolvedLocalModelDeploymentSnapshot;
}> {
  if (!client.model.previewLocalModelDeployment)
    throw new Error(
      "Preview is unavailable because the model service does not support local deployment previews.",
    );
  const response = await client.model.previewLocalModelDeployment(
    create(CreateLocalModelDeploymentRequestSchema, {
      modelId,
      profileId,
      placement,
      configuration: create(LocalModelDeploymentConfigurationSchema, configuration),
    }),
  );
  if (!response.resolvedSnapshot)
    throw new Error("Preview response protocol error: missing resolved deployment snapshot.");
  return { advisories: response.advisories, snapshot: response.resolvedSnapshot };
}

function catalogPendingKey(modelId: string): string {
  return `catalog:${modelId}`;
}
