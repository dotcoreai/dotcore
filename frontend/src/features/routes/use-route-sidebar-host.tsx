import { Code, ConnectError } from "@connectrpc/connect";
import { type ReactNode, type RefObject, useCallback, useEffect, useRef, useState } from "react";
import { useLocation } from "react-router-dom";

import { type MetricsClient, metricsClient as defaultMetricsClient } from "@/api/metrics-client";
import { type RouteClient, routeClient } from "@/api/route-client";
import { usePanelDirtyGuard, usePanelNavigation } from "@/app/console-layout";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { Notice } from "@/components/ui/notice";
import { ResourceMarker, ResourceTitle } from "@/components/ui/resource-marker";
import { StatusBadge } from "@/components/ui/status-badge";
import { routeChatTarget } from "@/features/chat/chattable-route";
import {
  deploymentStatus,
  modelDeploymentPresentationName,
} from "@/features/models/model-projection";
import { type FallbackGroup, type InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import type { FieldErrors } from "@/lib/form-validation";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import {
  activePanel,
  appendPanelHref,
  appendPanelLocation,
  canonicalizePanelLocation,
  type PanelEntry,
  selectPanelLocation,
} from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";

import { newEditorGroup, type RouteEditorState } from "./route-draft";
import { RouteEditor } from "./route-editor";
import {
  RouteConnectionView,
  RouteDetails,
  RouteInspectorActions,
  type RouteMemberMention,
} from "./route-inspector";
import {
  deploymentLabel,
  deploymentOptions,
  projectRouteHealth,
  selectionForMemberCount,
} from "./route-projection";
import { useRouteViews } from "./use-route-views";

/** The route sidebar of a page, and what it offers the page around it, such as its card list. */
export interface RouteSidebarHostState {
  /** The model deployments a route names, by ID, as route members show them. */
  deploymentMentions: ReadonlyMap<string, RouteMemberMention>;
  /** The last failed action while no sidebar view shows it. */
  error?: string;
  /** Why the routes could not be loaded. */
  loadError?: string;
  /** Whether the routes are still loading once delayed loading shows. */
  loadingVisible: boolean;
  /** Opens the create screen; focus returns to the host's return focus target. */
  openCreate: () => void;
  /** The id of the sidebar the host renders, for the controls that open it. */
  panelId: string;
  /** Absent until the first payload. */
  routes?: InferenceRouteView[];
  /** Opens the route's sidebar; focus returns to `opener` when it closes. */
  select: (view: InferenceRouteView, opener: HTMLElement) => void;
  selectedRouteId?: string;
  /** The sidebar and its delete confirmation, rendered by the hosting page. */
  sidebar: ReactNode;
}

export interface RouteSidebarHostProps {
  client?: RouteClient;
  /** The range label the route's requests start at, such as the hosting page's range. */
  initialRange?: string;
  metricsClient?: MetricsClient;
  /** The element focus returns to when the sidebar closes; set it before opening the sidebar. */
  openerRef: RefObject<HTMLElement | null>;
  /** Where focus returns after a create or delete, whose opener is gone or not a route. */
  returnFocusRef?: RefObject<HTMLElement | null>;
}

/**
 * The route sidebar of whichever page hosts it: a route's details, connect screen and editor, and
 * the delete confirmation, opened from the panel stack. It owns the route mutations, the dirty
 * guard of the editor, and the streamed route lists, held while the editor is open or a mutation
 * is pending, which a page listing routes shows through the returned state. Navigation keeps the
 * hosting page, so the sidebar opens over the routes page or the overview alike.
 */
export function useRouteSidebarHost({
  client = routeClient,
  initialRange,
  metricsClient = defaultMetricsClient,
  openerRef,
  returnFocusRef,
}: RouteSidebarHostProps): RouteSidebarHostState {
  const location = useLocation();
  const panelNavigation = usePanelNavigation();
  const [editor, setEditor] = useState<RouteEditorState>();
  const [editorDirty, setEditorDirty] = useState(false);
  const [mutationPending, setMutationPending] = useState(false);
  const [togglePending, setTogglePending] = useState(false);
  const [mutationError, setMutationError] = useState<string>();
  const [fieldErrors, setFieldErrors] = useState<FieldErrors>(new Map());
  const [returnToDetail, setReturnToDetail] = useState(false);
  const [deleteRequest, setDeleteRequest] = useState<{
    id: string;
    generation: bigint;
    name: string;
  }>();
  const cancelDeleteRef = useRef<HTMLButtonElement>(null);
  const deleteButtonRef = useRef<HTMLElement>(null);
  const editorOpenRef = useRef(false);
  const deletedRouteIdRef = useRef<string | undefined>(undefined);
  const entry = activePanel(
    canonicalizePanelLocation({ pathname: location.pathname, search: location.search }).search,
  );
  const routeId = entry?.page === "routes" ? entry.resourceId : undefined;
  const isCreate = entry?.page === "routes" && entry.screen === "create";
  const isEdit = entry?.page === "routes" && entry.screen === "edit";
  // The streamed lists hold while an editor is open or a mutation is pending, so an update cannot
  // replace the route being edited or race a mutation response.
  const { acceptRoute, deployments, loadError, removeRoute, routes } = useRouteViews(
    mutationPending || togglePending || isCreate || isEdit,
  );
  const selectedRoute = routes?.find((view) => view.route?.inferenceRouteId === routeId);
  const loadingVisible = useDelayedLoading(routes === undefined);
  const isConnect = entry?.page === "routes" && entry.screen === "connect";
  const isRoute = entry?.page === "routes" && entry.screen === "route";

  useEffect(() => {
    const leavingEditor = editorOpenRef.current && !isCreate && !isEdit;
    editorOpenRef.current = isCreate || isEdit;
    queueMicrotask(() => {
      if (isCreate) {
        setEditor({
          name: "",
          groups: [newEditorGroup()],
        });
        setFieldErrors(new Map());
        setMutationError(undefined);
        setEditorDirty(false);
        return;
      }
      if (isEdit && selectedRoute) {
        setEditor(editorStateFromView(selectedRoute));
        setFieldErrors(new Map());
        setMutationError(undefined);
        setEditorDirty(false);
        return;
      }
      setEditor(undefined);
      setFieldErrors(new Map());
      setEditorDirty(false);
      // A failed save belongs to the abandoned draft, so it must not follow into the details.
      if (leavingEditor) setMutationError(undefined);
    });
  }, [isCreate, isEdit, selectedRoute]);

  usePanelDirtyGuard(editorDirty, () => {
    setEditorDirty(false);
  });
  usePanelPendingNavigation(
    mutationPending,
    "Saving changes. Inspector navigation is unavailable.",
  );

  const closeInspector = useCallback(
    (bypassPendingGuard = false) => {
      panelNavigation.close({ bypassPendingGuard });
    },
    [panelNavigation],
  );

  const popInspector = useCallback(() => {
    panelNavigation.back();
  }, [panelNavigation]);

  const select = (view: InferenceRouteView, opener: HTMLElement) => {
    const id = view.route?.inferenceRouteId;
    if (!id) return;
    openerRef.current = opener;
    setMutationError(undefined);
    panelNavigation.navigatePanel(selectPanelLocation(location, routeEntry(id)));
  };

  const openCreate = () => {
    openerRef.current = returnFocusRef?.current ?? null;
    setMutationError(undefined);
    panelNavigation.navigatePanel(selectPanelLocation(location, createEntry));
  };

  const save = async () => {
    if (!editor || mutationPending) return;
    const name = editor.name.trim();
    const fallbackGroups = editor.groups.map((group) => ({
      selection: selectionForMemberCount(group.deploymentIds.length),
      modelDeploymentIds: group.deploymentIds,
    })) as FallbackGroup[];
    setMutationError(undefined);
    setMutationPending(true);
    try {
      if (editor.route?.route) {
        const route = editor.route.route;
        const response = await client.route.replaceRoute({
          inferenceRouteId: route.inferenceRouteId,
          expectedGeneration: route.generation,
          name,
          enabled: route.enabled,
          fallbackGroups,
        });
        if (!response.route?.route) throw new Error("missing replaced route in service response");
        acceptRoute(response.route);
        setEditorDirty(false);
        setReturnToDetail(true);
      } else {
        const response = await client.route.createRoute({
          name,
          enabled: true,
          fallbackGroups,
        });
        if (!response.route?.route) {
          throw new CreateRouteProtocolError("missing created route.");
        }
        acceptCreatedRoute(response.route);
      }
    } catch (error: unknown) {
      const message =
        error instanceof CreateRouteProtocolError
          ? error.message
          : describeError(editor.route ? "Route update" : "Route creation", error);
      if (!editor.route && isOutcomeUncertain(error)) {
        try {
          const response = await client.route.listRoutes({});
          const matches = response.routes.filter((view) =>
            routeDeclarationMatches(view, name, fallbackGroups),
          );
          if (matches.length === 1) acceptCreatedRoute(matches[0]);
          else setMutationError(message);
        } catch {
          setMutationError(message);
        }
      } else {
        setMutationError(message);
      }
    } finally {
      setMutationPending(false);
    }
  };

  const acceptCreatedRoute = (view: InferenceRouteView) => {
    acceptRoute(view);
    setFieldErrors(new Map());
    setMutationError(undefined);
    setEditorDirty(false);
    openerRef.current = returnFocusRef?.current ?? null;
    // Replace the Create entry so browser Back does not return to a spent Create screen.
    panelNavigation.navigatePanel(
      selectPanelLocation(location, routeEntry(view.route!.inferenceRouteId)),
      { bypassDirtyGuard: true, bypassPendingGuard: true, replace: true },
    );
  };

  const toggleEnabled = async () => {
    const route = selectedRoute?.route;
    if (!route || mutationPending) return;
    const action = route.enabled ? "Disable" : "Enable";
    setMutationError(undefined);
    setMutationPending(true);
    setTogglePending(true);
    try {
      const response = await client.route.replaceRoute({
        inferenceRouteId: route.inferenceRouteId,
        expectedGeneration: route.generation,
        name: route.name,
        enabled: !route.enabled,
        fallbackGroups: route.fallbackGroups.map(({ selection, modelDeploymentIds }) => ({
          selection,
          modelDeploymentIds,
        })),
      });
      if (!response.route?.route) {
        throw new Error(`${action} response protocol error: missing replaced route.`);
      }
      acceptRoute(response.route);
    } catch (error: unknown) {
      setMutationError(describeError(action, error));
    } finally {
      setMutationPending(false);
      setTogglePending(false);
    }
  };

  const requestDelete = () => {
    const route = selectedRoute?.route;
    if (!route || mutationPending) return;
    setDeleteRequest({
      id: route.inferenceRouteId,
      generation: route.generation,
      name: route.name,
    });
  };

  const remove = async () => {
    if (!deleteRequest || mutationPending) return;
    setMutationError(undefined);
    setMutationPending(true);
    try {
      await client.route.deleteRoute({
        inferenceRouteId: deleteRequest.id,
        expectedGeneration: deleteRequest.generation,
      });
      openerRef.current = returnFocusRef?.current ?? null;
      deletedRouteIdRef.current = deleteRequest.id;
      removeRoute(deleteRequest.id);
      setDeleteRequest(undefined);
      closeInspector(true);
    } catch (error: unknown) {
      setMutationError(describeError("Route deletion", error));
    } finally {
      setMutationPending(false);
    }
  };

  // A deleted route is closed by the delete itself; closing again would traverse history twice.
  useEffect(() => {
    if (routes && routeId && !selectedRoute && routeId !== deletedRouteIdRef.current) {
      closeInspector();
    }
  }, [closeInspector, routeId, routes, selectedRoute]);

  // A saved edit returns to the route's details once the dirty and pending guards have released,
  // so the return is neither blocked nor asks to discard the saved draft.
  useEffect(() => {
    if (!returnToDetail || mutationPending || editorDirty) return;
    queueMicrotask(() => setReturnToDetail(false));
    popInspector();
  }, [editorDirty, mutationPending, popInspector, returnToDetail]);

  const editorOptions = deploymentOptions(
    deployments ?? [],
    editor?.groups.flatMap((group) => group.deploymentIds) ?? [],
  );
  const takenNames = new Set(
    (routes ?? [])
      .filter((view) => view.route?.inferenceRouteId !== editor?.route?.route?.inferenceRouteId)
      .map((view) => view.route?.name ?? ""),
  );
  const deploymentMentions = new Map(
    (deployments ?? []).map((deployment) => [
      deployment.modelDeploymentId,
      {
        colorKey: deployment.modelDeploymentId,
        href: appendPanelHref(location, {
          page: "models",
          resourceId: deployment.modelDeploymentId,
          screen: "deployment",
        }),
        name: deploymentLabel(deployment),
        status: deploymentStatus(deployment),
      },
    ]),
  );
  usePublishPanelTitles([
    ...(routes ?? []).flatMap((view) =>
      view.route
        ? [panelTitle("routes", "route", view.route.inferenceRouteId, view.route.name)]
        : [],
    ),
    ...(deployments ?? []).map((deployment) =>
      panelTitle(
        "models",
        "deployment",
        deployment.modelDeploymentId,
        modelDeploymentPresentationName(deployment),
      ),
    ),
  ]);
  const open = Boolean(entry?.page === "routes");
  const title = isCreate ? (
    "Create route"
  ) : selectedRoute?.route ? (
    <ResourceTitle
      colorKey={selectedRoute.route.inferenceRouteId}
      name={selectedRoute.route.name}
      screen={isConnect ? "Connect" : isEdit ? "Edit" : undefined}
    />
  ) : isConnect ? (
    "Connect"
  ) : isEdit ? (
    "Edit route"
  ) : (
    "Route details"
  );

  const sidebar = (
    <>
      <InspectorShell
        body={
          isConnect && selectedRoute ? (
            <RouteConnectionView name={selectedRoute.route!.name} />
          ) : editor ? (
            <RouteEditor
              deployments={editorOptions}
              editor={editor}
              loading={deployments === undefined}
              errors={fieldErrors}
              onChange={(nextEditor) => {
                setEditorDirty(true);
                setEditor(nextEditor);
              }}
              onClearError={(field) =>
                setFieldErrors((current) => {
                  if (!current.has(field)) return current;
                  const next = new Map(current);
                  next.delete(field);
                  return next;
                })
              }
              onErrors={setFieldErrors}
              onSave={() => void save()}
              pending={mutationPending}
              serverError={mutationError}
              takenNames={takenNames}
            />
          ) : isRoute && selectedRoute ? (
            <RouteDetails
              deploymentMentions={deploymentMentions}
              error={mutationError}
              initialRange={initialRange}
              metricsClient={metricsClient}
              view={selectedRoute}
            />
          ) : null
        }
        footer={
          isRoute && selectedRoute ? (
            <RouteInspectorActions
              chatTarget={routeChatTarget(selectedRoute)}
              deleteFocusRef={deleteButtonRef}
              enabled={selectedRoute.route!.enabled}
              onConnect={() =>
                panelNavigation.navigatePanel(
                  appendPanelLocation(
                    location,
                    connectEntry(selectedRoute.route!.inferenceRouteId),
                  ),
                )
              }
              onDelete={requestDelete}
              onEdit={() =>
                panelNavigation.navigatePanel(
                  appendPanelLocation(location, editEntry(selectedRoute.route!.inferenceRouteId)),
                )
              }
              onToggleEnabled={() => void toggleEnabled()}
              pending={mutationPending}
              togglePending={togglePending}
            />
          ) : undefined
        }
        focusKey={entry ? `${entry.screen}:${entry.resourceId ?? ""}` : undefined}
        id={inspectorPanelId}
        navigationDisabledReason={
          mutationPending ? "Saving changes. Inspector navigation is unavailable." : undefined
        }
        onBack={popInspector}
        onOpenChange={(next) => {
          if (!next) closeInspector();
        }}
        open={open}
        suspendDismissal={Boolean(deleteRequest)}
        openerRef={openerRef}
        status={
          (isRoute || isEdit) && selectedRoute ? (
            <StatusBadge status={projectRouteHealth(selectedRoute).status} />
          ) : undefined
        }
        title={title}
      />
      <Dialog
        disablePointerDismissal
        modal
        onOpenChange={(nextOpen) => {
          if (!mutationPending) setDeleteRequest(nextOpen ? deleteRequest : undefined);
        }}
        open={Boolean(deleteRequest)}
      >
        <DialogContent
          aria-describedby="delete-route-description"
          finalFocus={deleteButtonRef}
          initialFocus={cancelDeleteRef}
          showCloseButton={false}
        >
          <DialogHeader>
            <DialogTitle>Delete route?</DialogTitle>
            {mutationError ? (
              <Notice role="alert" tone="destructive">
                {mutationError}
              </Notice>
            ) : null}
            <DialogDescription id="delete-route-description">
              Delete route {deleteRequest ? <ResourceMarker colorKey={deleteRequest.id} /> : null}
              {deleteRequest?.name}? This cannot be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <DialogClose
              render={
                <Button
                  disabled={mutationPending}
                  ref={cancelDeleteRef}
                  type="button"
                  variant="outline"
                >
                  Cancel
                </Button>
              }
            />
            <Button
              disabled={mutationPending}
              onClick={() => void remove()}
              type="button"
              variant="destructive"
            >
              {mutationPending ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );

  return {
    deploymentMentions,
    error:
      mutationError && !editor && !deleteRequest && !(isRoute && selectedRoute)
        ? mutationError
        : undefined,
    loadError,
    loadingVisible,
    openCreate,
    panelId: inspectorPanelId,
    routes,
    select,
    selectedRouteId: routeId,
    sidebar,
  };
}

/** The id of the route sidebar, which rows and openers name in `aria-controls`. */
export const inspectorPanelId = "route-inspector-panel";
const rejectedMutationCodes = new Set([
  Code.InvalidArgument,
  Code.NotFound,
  Code.AlreadyExists,
  Code.PermissionDenied,
  Code.Unauthenticated,
  Code.FailedPrecondition,
  Code.ResourceExhausted,
  Code.OutOfRange,
  Code.Unimplemented,
]);
const createEntry: PanelEntry = { page: "routes", screen: "create" };

function routeEntry(resourceId: string): PanelEntry {
  return { page: "routes", resourceId, screen: "route" };
}
function connectEntry(resourceId: string): PanelEntry {
  return { page: "routes", resourceId, screen: "connect" };
}
function editEntry(resourceId: string): PanelEntry {
  return { page: "routes", resourceId, screen: "edit" };
}

function editorStateFromView(view: InferenceRouteView): RouteEditorState {
  const route = view.route!;
  return {
    route: view,
    name: route.name,
    groups: route.fallbackGroups.map((group) => newEditorGroup([...group.modelDeploymentIds])),
  };
}
function routeDeclarationMatches(
  view: InferenceRouteView,
  name: string,
  fallbackGroups: readonly FallbackGroup[],
): boolean {
  const route = view.route;
  return Boolean(
    route &&
    route.name === name &&
    route.enabled &&
    route.fallbackGroups.length === fallbackGroups.length &&
    route.fallbackGroups.every(
      (group, index) =>
        group.selection === fallbackGroups[index].selection &&
        group.modelDeploymentIds.length === fallbackGroups[index].modelDeploymentIds.length &&
        group.modelDeploymentIds.every(
          (id, memberIndex) => id === fallbackGroups[index].modelDeploymentIds[memberIndex],
        ),
    ),
  );
}

class CreateRouteProtocolError extends Error {
  constructor(detail: string) {
    super(`Route creation response protocol error: ${detail}`);
  }
}

function isOutcomeUncertain(error: unknown): boolean {
  return (
    !(error instanceof CreateRouteProtocolError) &&
    !rejectedMutationCodes.has(ConnectError.from(error).code)
  );
}

function describeError(action: string, error: unknown): string {
  const connectError = ConnectError.from(error);
  const code = Code[connectError.code] ?? "Unknown";
  const message = connectError.rawMessage || "The service returned no details";
  return rejectedMutationCodes.has(connectError.code) || action.startsWith("Loading")
    ? `${action} was rejected (${code}): ${message}`
    : `${action} outcome is uncertain (${code}): ${message}`;
}
