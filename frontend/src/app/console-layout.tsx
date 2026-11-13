import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Outlet, useLocation, useMatch, useNavigate } from "react-router-dom";

import { useClients } from "@/api/clients";
import { ConsoleHeader } from "@/app/console-header";
import { ConsoleSidebar } from "@/app/console-sidebar";
import { createPanelTitleLookup } from "@/app/panel-title-lookup";
import { useSidebarPreference } from "@/app/sidebar-preference-context";
import { SidebarPreferenceProvider } from "@/app/sidebar-preference-provider";
import { narrowInspectorMaxWidth } from "@/components/ui/inspector-breakpoint";
import { PanelNavigationProvider } from "@/components/ui/panel-navigation-provider";
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar";
import { type ChatMode, ChatOpenerContext } from "@/features/chat/chat-opener";
import { ChatPanel } from "@/features/chat/chat-panel";
import { ChatSessionProvider } from "@/features/chat/chat-session-provider";
import { GetStartedHiddenProvider } from "@/features/get-started/get-started-hidden";
import { ModelsPollingProvider } from "@/features/models/polling/models-polling-provider";
import { WatchPauseProvider } from "@/hooks/watch-pause-provider";
import type { StorageLike } from "@/lib/browser-storage";
import { usePanelNavigation } from "@/panels/panel-navigation";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { ResourceColorProvider } from "@/theme/resource-color-provider";

export interface ConsoleLayoutProps {
  eventTarget?: EventTarget;
  storage?: StorageLike;
}

/**
 * Layout route rendering the persistent console shell around routed page content. It mounts the
 * console-wide resource watch and models polling, which every page reads.
 */
export function ConsoleLayout({ eventTarget, storage }: ConsoleLayoutProps) {
  const { modelClient, resourceWatchClient } = useClients();

  return (
    <SidebarPreferenceProvider eventTarget={eventTarget} storage={storage}>
      <ResourceWatchProvider client={resourceWatchClient}>
        <ResourceColorProvider>
          <GetStartedHiddenProvider>
            <ModelsPollingProvider client={modelClient}>
              <WatchPauseProvider>
                <ConsoleLayoutShell storage={storage} />
              </WatchPauseProvider>
            </ModelsPollingProvider>
          </GetStartedHiddenProvider>
        </ResourceColorProvider>
      </ResourceWatchProvider>
    </SidebarPreferenceProvider>
  );
}

function ConsoleLayoutShell({ storage }: { storage?: StorageLike }) {
  const { applicationClient, routeClient } = useClients();
  const { collapsed, setCollapsed } = useSidebarPreference();
  // Names a panel's parent resource that no page has published yet.
  const panelTitleLookup = useMemo(
    () => createPanelTitleLookup(routeClient, applicationClient),
    [applicationClient, routeClient],
  );

  return (
    <SidebarProvider
      className="h-full flex-col"
      onOpenChange={(open) => setCollapsed(!open)}
      open={!collapsed}
    >
      <PanelNavigationProvider titleLookup={panelTitleLookup}>
        <ConsoleChrome storage={storage} />
      </PanelNavigationProvider>
    </SidebarProvider>
  );
}

/**
 * The console header, sidebar, routed page and the chat session with its two views: the floating
 * panel and the Chat page. The panel is hidden on the Chat page and stays closed after leaving it,
 * unless the operator asks to show the chat as a panel.
 */
function ConsoleChrome({ storage }: { storage?: StorageLike }) {
  const { chatClient, decisionClient } = useClients();
  const location = useLocation();
  const navigate = useNavigate();
  const onChatPage = useMatch("/chat") !== null;
  const [chatOpen, setChatOpen] = useState(false);
  const [chatRoute, setChatRoute] = useState<string | null>(null);
  const [chatModes, setChatModes] = useState<ReadonlyMap<string, ChatMode>>(new Map());
  const openPanelOnLeave = useRef(false);
  if (onChatPage && chatOpen) setChatOpen(false);

  const setChatMode = useCallback(
    (routeName: string, mode: ChatMode) =>
      setChatModes((modes) => new Map(modes).set(routeName, mode)),
    [],
  );
  const openChat = useCallback(
    (routeName?: string, mode?: ChatMode) => {
      if (routeName !== undefined) setChatRoute(routeName);
      if (routeName !== undefined && mode) setChatMode(routeName, mode);
      setChatOpen(true);
    },
    [setChatMode],
  );
  const opener = useChatOpenerRules(openChat);

  const showAsPanel = useCallback(() => {
    openPanelOnLeave.current = true;
    // The first history entry has no console page before it to return to.
    if (location.key === "default") void navigate("/routes", { replace: true });
    else void navigate(-1);
  }, [location.key, navigate]);

  // Opens the panel once the page has left, so the opener's inspector rules see the page returned to.
  useEffect(() => {
    if (onChatPage || !openPanelOnLeave.current) return;
    openPanelOnLeave.current = false;
    opener();
  }, [onChatPage, opener]);

  return (
    <ChatSessionProvider
      active={chatOpen || onChatPage}
      chatClient={chatClient}
      decisionClient={decisionClient}
      modes={chatModes}
      onModeChange={setChatMode}
      onRouteChange={setChatRoute}
      onShowAsPanel={showAsPanel}
      route={chatRoute}
      storage={storage}
    >
      <ChatOpenerContext.Provider value={opener}>
        <ConsoleHeader />
        <div className="flex min-h-0 flex-1">
          <ConsoleSidebar
            chatCurrent={onChatPage}
            chatOpen={chatOpen}
            onChatToggle={onChatPage ? showAsPanel : () => setChatOpen((open) => !open)}
          />
          <SidebarInset className="min-h-0 overflow-y-auto">
            <div className="mx-auto w-full max-w-[1160px] p-6.5 max-md:p-3.5">
              <Outlet />
            </div>
          </SidebarInset>
        </div>
        <ChatPanel onOpenChange={setChatOpen} open={chatOpen} />
      </ChatOpenerContext.Provider>
    </ChatSessionProvider>
  );
}

/**
 * Opens the console chat from an inspector or the Chat page, on a route and mode or on the
 * session's ones.
 * The narrow inspector is a full-screen modal that would hide the chat, so it closes first; a wide
 * inspector stays open beside the chat where both fit, and closes where the chat would overlap it.
 */
function useChatOpenerRules(openChat: (routeName?: string, mode?: ChatMode) => void) {
  const { close } = usePanelNavigation();
  const closeRef = useRef(close);
  const [overlapChecks, setOverlapChecks] = useState(0);
  const opener = useCallback(
    (routeName?: string, mode?: ChatMode) => {
      if (window.innerWidth <= narrowInspectorMaxWidth) close();
      else setOverlapChecks((count) => count + 1);
      openChat(routeName, mode);
    },
    [close, openChat],
  );

  useLayoutEffect(() => {
    closeRef.current = close;
  }, [close]);

  // Runs once the opened chat is laid out and before it paints: a wide inspector the chat would
  // cover closes, so the chat is never hidden beneath it.
  useLayoutEffect(() => {
    if (overlapChecks === 0) return;
    const chat = document.querySelector("[data-inspector-companion]")?.getBoundingClientRect();
    const inspector = document.querySelector("[data-inspector-panel]")?.getBoundingClientRect();
    if (chat && inspector && chat.width > 0 && chat.right > inspector.left) closeRef.current();
  }, [overlapChecks]);

  return opener;
}
