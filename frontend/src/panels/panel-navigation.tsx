import type { MouseEvent, ReactNode } from "react";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  type BlockerFunction,
  Link,
  type LinkProps,
  useBlocker,
  useLocation,
  useNavigate,
} from "react-router-dom";

import {
  PanelNavigationPendingContext,
  type PanelNavigationPendingRegistration,
  PanelNavigationPendingRegistrationContext,
} from "./panel-navigation-pending";
import { type PanelPresentation, PanelPresentationContext } from "./panel-presentation";
import {
  activePanel,
  clearPanels,
  createPanelHistoryState,
  isPanelHistoryState,
  type PanelEntry,
  PanelHistoryCoordinator,
  type PanelLocation,
  type PanelTraversal,
  parsePanelStack,
  planPanelClose,
  popPanelLocation,
  serializePanelStack,
  sidebarParent,
} from "./panel-stack";
import { PanelParentContext, type PanelTitleLookup } from "./panel-titles-context";
import { PanelTitlesProvider } from "./panel-titles-provider";

interface PanelDirtyRegistration {
  discard: () => void;
}

interface PanelNavigationOptions {
  bypassDirtyGuard?: boolean;
  bypassPendingGuard?: boolean;
  replace?: boolean;
}

interface PanelNavigation {
  back: () => void;
  close: (
    options?: Pick<PanelNavigationOptions, "bypassDirtyGuard" | "bypassPendingGuard">,
  ) => void;
  navigatePanel: (location: PanelLocation, options?: PanelNavigationOptions) => void;
  parent?: PanelEntry;
  presentation: PanelPresentation;
}

interface PanelLinkProps extends Omit<LinkProps, "to"> {
  to: string;
}

/**
 * The unsaved-changes question raised when a navigation would leave a dirty panel: whether it is
 * open, how many times the operator kept editing, and the two answers.
 */
interface PanelDiscardPrompt {
  cancel: () => void;
  confirm: () => void;
  open: boolean;
  refusals: number;
}

const PanelNavigationContext = createContext<PanelNavigation | undefined>(undefined);
const PanelDiscardPromptContext = createContext<PanelDiscardPrompt | undefined>(undefined);
const PanelDirtyContext = createContext<
  ((registration: PanelDirtyRegistration | undefined) => void) | undefined
>(undefined);

/**
 * Owns the panel navigation state and the panel titles that label its back targets. It asks no
 * question itself: a blocked navigation from a dirty panel opens the discard prompt, which the
 * console renders. `titleLookup` names a parent resource no page has published yet.
 */
export function PanelNavigationState({
  children,
  titleLookup,
}: {
  children: ReactNode;
  titleLookup?: PanelTitleLookup;
}) {
  return (
    <PanelTitlesProvider lookup={titleLookup}>
      <PanelNavigationStateProvider>{children}</PanelNavigationStateProvider>
    </PanelTitlesProvider>
  );
}

function PanelNavigationStateProvider({ children }: { children: ReactNode }) {
  const location = useLocation();
  const navigate = useNavigate();
  const sessionId = useRef(createSessionId());
  const closing = useRef<PanelLocation | undefined>(undefined);
  // The location a panel Back left, whose page view parameters the entry it returns to takes over.
  const carrying = useRef<PanelLocation | undefined>(undefined);
  const historyCoordinator = useRef(new PanelHistoryCoordinator());
  const pendingTraversal = useRef<PanelTraversal | undefined>(undefined);
  const replayingTraversal = useRef<PanelTraversal | undefined>(undefined);
  const bypassDirtyGuard = useRef(false);
  const dirtyRef = useRef<PanelDirtyRegistration | undefined>(undefined);
  const pendingNavigationRef = useRef(new Map<symbol, string>());
  const [dirty, setDirtyState] = useState<PanelDirtyRegistration>();
  const [pendingNavigationReason, setPendingNavigationReason] = useState<string>();
  const [discardOpen, setDiscardOpen] = useState(false);
  const [discardRefusals, setDiscardRefusals] = useState(0);
  const setDirty = useCallback((registration: PanelDirtyRegistration | undefined) => {
    dirtyRef.current = registration;
    setDirtyState(registration);
  }, []);
  const pendingRegistration = useMemo<PanelNavigationPendingRegistration>(
    () => ({
      register(token, reason) {
        if (reason) pendingNavigationRef.current.set(token, reason);
        else pendingNavigationRef.current.delete(token);
        setPendingNavigationReason(pendingNavigationRef.current.values().next().value);
      },
    }),
    [],
  );
  const blocker = useBlocker(
    useCallback<BlockerFunction>(({ currentLocation, nextLocation }) => {
      if (bypassDirtyGuard.current) {
        bypassDirtyGuard.current = false;
        return false;
      }
      return Boolean(dirtyRef.current) && !isViewOnlyChange(currentLocation, nextLocation);
    }, []),
  );

  useEffect(() => {
    const base = closing.current;
    if (!base) return;
    closing.current = undefined;
    if (location.search === base.search && location.pathname === base.pathname) return;
    void navigate(base, { replace: true });
  }, [location.pathname, location.search, navigate]);

  useEffect(() => {
    const left = carrying.current;
    if (!left || (left.pathname === location.pathname && left.search === location.search)) return;
    carrying.current = undefined;
    if (left.pathname !== location.pathname) return;

    // Page view parameters, such as table filters, live outside the panel history, so a panel Back
    // keeps the current ones on the same page.
    const search = serializePanelStack(left.search, parsePanelStack(location.search));
    if (search !== location.search) {
      void navigate(
        { pathname: location.pathname, search },
        { replace: true, state: location.state as unknown },
      );
    }
  }, [location, navigate]);

  const navigatePanel = useCallback(
    (next: PanelLocation, options: PanelNavigationOptions = {}) => {
      if (pendingNavigationRef.current.size > 0 && !options.bypassPendingGuard) return;
      bypassDirtyGuard.current = Boolean(options.bypassDirtyGuard);
      const currentPanel = activePanel(location.search);
      const nextPanel = activePanel(next.search);
      const currentState: unknown = location.state;
      const metadata = isPanelHistoryState(currentState) ? currentState.__dotcorePanel : undefined;
      const continuing =
        currentPanel &&
        metadata?.sessionId === sessionId.current &&
        metadata.step > 0 &&
        Boolean(nextPanel);
      const step = continuing ? metadata.step + (options.replace ? 0 : 1) : 1;
      void navigate(next, {
        replace: options.replace,
        state: createPanelHistoryState(sessionId.current, step),
      });
      if (options.bypassDirtyGuard) queueMicrotask(() => (bypassDirtyGuard.current = false));
    },
    [location.search, location.state, navigate],
  );

  const back = useCallback(() => {
    if (pendingNavigationRef.current.size > 0) return;
    const state: unknown = location.state;
    if (
      isPanelHistoryState(state) &&
      state.__dotcorePanel.sessionId === sessionId.current &&
      state.__dotcorePanel.step > 1
    ) {
      carrying.current = { pathname: location.pathname, search: location.search };
      void navigate(-1);
      return;
    }
    void navigate(popPanelLocation(location.pathname, location.search), { replace: true });
  }, [location.pathname, location.search, location.state, navigate]);

  const close = useCallback(
    (options: Pick<PanelNavigationOptions, "bypassDirtyGuard" | "bypassPendingGuard"> = {}) => {
      if (pendingNavigationRef.current.size > 0 && !options.bypassPendingGuard) return;
      bypassDirtyGuard.current = Boolean(options.bypassDirtyGuard);
      const state: unknown = location.state;
      const metadata = isPanelHistoryState(state) ? state.__dotcorePanel : undefined;
      const plan = planPanelClose(
        location,
        state,
        metadata?.sessionId === sessionId.current ? metadata.step : 0,
      );
      if (plan.historyDelta) {
        closing.current = plan.location;
        void navigate(plan.historyDelta);
      } else {
        void navigate(plan.location, { replace: true });
      }
      if (options.bypassDirtyGuard) queueMicrotask(() => (bypassDirtyGuard.current = false));
    },
    [location, navigate],
  );

  useEffect(() => {
    const replaying = replayingTraversal.current;
    if (
      !replaying ||
      replaying.location.pathname !== location.pathname ||
      replaying.location.search !== location.search
    )
      return;
    historyCoordinator.current.traverse(replaying, Boolean(dirtyRef.current));
    replayingTraversal.current = undefined;
  }, [location.pathname, location.search]);

  useEffect(() => {
    if (blocker.state !== "blocked" || pendingTraversal.current) return;
    const traversal = blockedPanelTraversal(location, blocker.location);
    const restoration = historyCoordinator.current.traverse(traversal, Boolean(dirty));
    if (restoration.type !== "restore") return;

    pendingTraversal.current = traversal;
    if (historyCoordinator.current.restored(restoration.state).type === "confirm-discard") {
      setDiscardOpen(true);
    }
  }, [blocker, dirty, location]);

  const cancelDiscard = useCallback(() => {
    if (!pendingTraversal.current) return;
    closing.current = undefined;
    carrying.current = undefined;
    historyCoordinator.current.resolveDiscard(false);
    pendingTraversal.current = undefined;
    setDiscardOpen(false);
    setDiscardRefusals((count) => count + 1);
    if (blocker.state === "blocked") blocker.reset();
  }, [blocker]);

  const confirmDiscard = useCallback(() => {
    const traversal = pendingTraversal.current;
    if (!traversal) return;
    const replay = historyCoordinator.current.resolveDiscard(true);
    if (replay.type !== "replay") return;

    // React Router restores blocked POP locations before exposing the blocker and performs the
    // non-cancelable traversal again on proceed. The location effect consumes the coordinator's
    // replay suppression only after that exact traversal completes.
    replayingTraversal.current = traversal;
    pendingTraversal.current = undefined;
    setDiscardOpen(false);
    dirty?.discard();
    if (blocker.state === "blocked") blocker.proceed();
  }, [blocker, dirty]);

  const parent = useMemo(() => sidebarParent(location.search), [location.search]);
  const presentation = usePanelPresentationForLocation(location);
  const value = useMemo(
    () => ({ back, close, navigatePanel, parent, presentation }),
    [back, close, navigatePanel, parent, presentation],
  );
  const discardPrompt = useMemo(
    () => ({
      cancel: cancelDiscard,
      confirm: confirmDiscard,
      open: discardOpen,
      refusals: discardRefusals,
    }),
    [cancelDiscard, confirmDiscard, discardOpen, discardRefusals],
  );
  return (
    <PanelNavigationPendingRegistrationContext.Provider value={pendingRegistration}>
      <PanelNavigationPendingContext.Provider value={Boolean(pendingNavigationReason)}>
        <PanelPresentationContext.Provider value={presentation}>
          <PanelDirtyContext.Provider value={setDirty}>
            <PanelDiscardPromptContext.Provider value={discardPrompt}>
              <PanelNavigationContext.Provider value={value}>
                <PanelParentContext.Provider value={parent}>{children}</PanelParentContext.Provider>
              </PanelNavigationContext.Provider>
            </PanelDiscardPromptContext.Provider>
          </PanelDirtyContext.Provider>
        </PanelPresentationContext.Provider>
      </PanelNavigationPendingContext.Provider>
    </PanelNavigationPendingRegistrationContext.Provider>
  );
}

export function PanelLink({ children, onClick, to, ...props }: PanelLinkProps) {
  const navigation = useContext(PanelNavigationContext);
  if (!navigation) {
    return (
      <a {...props} href={to}>
        {children}
      </a>
    );
  }

  return (
    <Link
      {...props}
      data-panel-navigation=""
      onClick={(event) => {
        onClick?.(event);
        if (!shouldNavigatePanel(event)) return;
        event.preventDefault();
        const target = new URL(to, window.location.origin);
        navigation.navigatePanel({ pathname: target.pathname, search: target.search });
      }}
      to={to}
    >
      {children}
    </Link>
  );
}

// eslint-disable-next-line react-refresh/only-export-components
export function usePanelDirtyGuard(dirty: boolean, discard: () => void): void {
  const register = useContext(PanelDirtyContext);
  useEffect(() => {
    if (!register || !dirty) return;
    const registration = { discard };
    register(registration);
    return () => register(undefined);
  }, [dirty, discard, register]);

  useEffect(() => {
    if (!dirty) return;
    const beforeUnload = (event: BeforeUnloadEvent) => event.preventDefault();
    window.addEventListener("beforeunload", beforeUnload);
    return () => window.removeEventListener("beforeunload", beforeUnload);
  }, [dirty]);
}

/** The open discard prompt's state and answers, or nothing outside a panel navigation. */
// eslint-disable-next-line react-refresh/only-export-components
export function usePanelDiscardPrompt(): PanelDiscardPrompt | undefined {
  return useContext(PanelDiscardPromptContext);
}

// eslint-disable-next-line react-refresh/only-export-components
export function usePanelNavigation(): PanelNavigation {
  const provided = useContext(PanelNavigationContext);
  const location = useLocation();
  const navigate = useNavigate();
  const fallback = useMemo<PanelNavigation>(
    () => ({
      back: () => void navigate(popPanelLocation(location.pathname, location.search)),
      close: () =>
        void navigate(
          { pathname: location.pathname, search: clearPanels(location.search) },
          { replace: true },
        ),
      navigatePanel: (next, options) => void navigate(next, options),
      parent: sidebarParent(location.search),
      presentation: parsePanelStack(location.search).length ? "restore" : "idle",
    }),
    [location.pathname, location.search, navigate],
  );
  return provided ?? fallback;
}

function usePanelPresentationForLocation(
  location: ReturnType<typeof useLocation>,
): PanelPresentation {
  const size = parsePanelStack(location.search).length;
  const [state, setState] = useState<{
    key: string;
    presentation: PanelPresentation;
    size: number;
  }>(() => ({
    key: location.key,
    presentation: size ? "restore" : "idle",
    size,
  }));

  useLayoutEffect(() => {
    if (state.key === location.key) return;
    // Keep a location's presentation stable before the browser can paint its route-owned shell.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setState({
      key: location.key,
      presentation: state.size === 0 ? (size ? "enter" : "idle") : size ? "continue" : "idle",
      size,
    });
  }, [location.key, size, state]);

  return state.presentation;
}

function shouldNavigatePanel(event: MouseEvent<HTMLAnchorElement>): boolean {
  return (
    !event.defaultPrevented &&
    event.button === 0 &&
    !event.metaKey &&
    !event.ctrlKey &&
    !event.shiftKey &&
    !event.altKey &&
    !event.currentTarget.hasAttribute("download") &&
    (!event.currentTarget.target || event.currentTarget.target === "_self")
  );
}

// A same-page change of only the page view parameters, such as a table filter, discards no edit.
function isViewOnlyChange(
  current: { pathname: string; search: string },
  next: { pathname: string; search: string },
): boolean {
  return (
    current.pathname === next.pathname &&
    current.search !== next.search &&
    serializePanelStack("", parsePanelStack(current.search)) ===
      serializePanelStack("", parsePanelStack(next.search))
  );
}

function blockedPanelTraversal(
  current: { pathname: string; search: string; state: unknown },
  next: { pathname: string; search: string; state: unknown },
): PanelTraversal {
  const currentStep = isPanelHistoryState(current.state) ? current.state.__dotcorePanel.step : 0;
  const nextStep = isPanelHistoryState(next.state) ? next.state.__dotcorePanel.step : 0;
  const stepDelta = nextStep - currentStep;
  return {
    delta: stepDelta === 0 ? 1 : stepDelta,
    location: { pathname: next.pathname, search: next.search },
  };
}

function createSessionId() {
  const bytes = new Uint8Array(16);
  const browserCrypto = globalThis.crypto;
  if (browserCrypto?.getRandomValues) {
    browserCrypto.getRandomValues(bytes);
  } else {
    fallbackSessionSequence += 1;
    const seed = `${Date.now()}-${globalThis.performance?.now() ?? 0}-${fallbackSessionSequence}-${Math.random()}`;
    for (let index = 0; index < bytes.length; index += 1) {
      bytes[index] = seed.charCodeAt(index % seed.length) ^ Math.floor(Math.random() * 256);
    }
  }
  return `panel-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}

let fallbackSessionSequence = 0;
