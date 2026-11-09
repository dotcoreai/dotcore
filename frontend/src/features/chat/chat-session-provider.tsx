import { type ReactNode, useCallback, useMemo, useState } from "react";

import type { ChatClient } from "@/api/chat-client";
import type { DecisionClient } from "@/api/decision-client";
import type { ComboboxOption } from "@/components/ui/combobox";
import type { ChatMode } from "@/features/chat/chat-opener";
import {
  ChatConversationContext,
  ChatDecisionsContext,
  type ChatRouteSession,
  ChatRouteSessionContext,
} from "@/features/chat/chat-session-context";
import { chatModeForKinds, isChattableRoute } from "@/features/chat/chattable-route";
import { createDecisionStore } from "@/features/chat/decisions/decision-store";
import { useDecisions } from "@/features/chat/decisions/use-decisions";
import { useChat } from "@/features/chat/use-chat";
import { InferenceRequestKind } from "@/gen/dotcore/catalog/v1/model_capabilities_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { getBrowserStorage, type StorageLike } from "@/lib/browser-storage";
import { requestKindsLabel, servesChat } from "@/lib/request-kinds";
import { useWatchedRoutes } from "@/resource-watch/resource-watch";

/**
 * Owns the console chat session for the chat panel and the Chat page, so the route, the
 * conversation and the decisions survive switching between them, closing the panel and navigation.
 * The chosen route is held by the caller, which also opens the chat on a route. It reads routes
 * only while `active`, that is while one of its views is shown. The route, the conversation and
 * the decisions are separate contexts, so a streamed reply or a draft keystroke doesn't re-render
 * the other views. The decision draft and saved question sets are kept in `storage`.
 */
export function ChatSessionProvider({
  active,
  chatClient,
  children,
  decisionClient,
  modes,
  onModeChange,
  onRouteChange,
  onShowAsPanel,
  route: chosenRoute,
  storage = getBrowserStorage(),
}: {
  active: boolean;
  chatClient: ChatClient;
  children: ReactNode;
  decisionClient: DecisionClient;
  /** The mode the operator picked per route serving both kinds. */
  modes: ReadonlyMap<string, ChatMode>;
  onModeChange: (route: string, mode: ChatMode) => void;
  onRouteChange: (route: string | null) => void;
  /** Leaves the Chat page with the panel open on the session's route. */
  onShowAsPanel: () => void;
  /** The route the operator picked or asked for; null lets the chat use the first available one. */
  route: string | null;
  storage?: StorageLike;
}) {
  const { loaded, loading, options, views } = useAvailableRoutes(active);
  const chosenUnavailable =
    chosenRoute !== null && loaded && !options.some((option) => option.id === chosenRoute);
  const resolvedRoute =
    chosenRoute === null ? options[0]?.id : chosenUnavailable ? undefined : chosenRoute;
  // Routes aren't read while no view is shown, so the session keeps the route it last resolved
  // rather than dropping the conversation and the answers until the list arrives again.
  const [keptRoute, setKeptRoute] = useState(resolvedRoute);
  const route = loaded ? resolvedRoute : (chosenRoute ?? keptRoute);
  if (route !== keptRoute) setKeptRoute(route);
  const pickerOptions = useMemo(
    () =>
      chosenUnavailable
        ? [
            { detail: "Unavailable", disabled: true, id: chosenRoute, label: chosenRoute },
            ...options,
          ]
        : options,
    [chosenRoute, chosenUnavailable, options],
  );
  const selectedRoute = chosenRoute ?? route;
  const kinds = views.find((view) => view.route?.name === selectedRoute)?.requestKinds ?? [];
  const modeSwitchable = servesChat(kinds) && kinds.includes(InferenceRequestKind.DECISION);
  const mode =
    modeSwitchable && selectedRoute !== undefined
      ? (modes.get(selectedRoute) ?? "chat")
      : chatModeForKinds(kinds);
  const selectMode = useCallback(
    (next: ChatMode) => {
      if (selectedRoute !== undefined) onModeChange(selectedRoute, next);
    },
    [onModeChange, selectedRoute],
  );
  const routeSession = useMemo<ChatRouteSession>(
    () => ({
      chosenRoute,
      chosenUnavailable,
      loading,
      mode,
      modeSwitchable,
      noRoute: !route && !loading && !chosenUnavailable,
      pickerOptions,
      route,
      selectMode,
      selectRoute: onRouteChange,
      showAsPanel: onShowAsPanel,
    }),
    [
      chosenRoute,
      chosenUnavailable,
      loading,
      mode,
      modeSwitchable,
      onRouteChange,
      onShowAsPanel,
      pickerOptions,
      route,
      selectMode,
    ],
  );

  const { error, messages, reset, retry, send, stop, streaming } = useChat(chatClient, route);
  const conversation = useMemo(
    () => ({ error, messages, reset, retry, send, stop, streaming }),
    [error, messages, reset, retry, send, stop, streaming],
  );
  const decisionStore = useMemo(() => createDecisionStore(storage), [storage]);
  const decisions = useDecisions(decisionClient, route, decisionStore);

  return (
    <ChatRouteSessionContext.Provider value={routeSession}>
      <ChatConversationContext.Provider value={conversation}>
        <ChatDecisionsContext.Provider value={decisions}>{children}</ChatDecisionsContext.Provider>
      </ChatConversationContext.Provider>
    </ChatRouteSessionContext.Provider>
  );
}

/**
 * Enabled, currently available routes as picker options naming their kinds, and every route's view;
 * watched only while `active`. `loaded` turns true once a read settles, so a chosen route is judged
 * against an actual list.
 */
function useAvailableRoutes(active: boolean) {
  const watched = useWatchedRoutes({ enabled: active });
  const routes = watched?.value?.routes;
  const loading = useDelayedLoading(active && routes === undefined && !watched?.failed);

  const options = useMemo<ComboboxOption[]>(
    () =>
      (routes ?? []).filter(isChattableRoute).map((view) => ({
        detail: requestKindsLabel(view.requestKinds) || undefined,
        id: view.route!.name,
        label: view.route!.name,
      })),
    [routes],
  );
  const views: readonly InferenceRouteView[] = routes ?? noRoutes;

  return { loaded: routes !== undefined || Boolean(watched?.failed), loading, options, views };
}

const noRoutes: readonly InferenceRouteView[] = [];
