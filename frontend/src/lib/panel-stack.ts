export type PanelPage = "app-catalog" | "apps" | "machines" | "model-catalog" | "models" | "routes";

export type PanelScreen =
  | "catalog"
  | "catalog-profile"
  | "configuration"
  | "connect"
  | "create"
  | "deployment"
  | "edit"
  | "execution-target"
  | "machine"
  | "logs"
  | "route";

export interface PanelEntry {
  page: PanelPage;
  profileId?: string;
  screen: PanelScreen;
  resourceId?: string;
}

export interface PanelLocation {
  pathname: string;
  search: string;
}

export interface CanonicalPanelLocation extends PanelLocation {
  recovered: boolean;
  stack: PanelEntry[];
}

export interface PanelRegistry {
  /** Whether `entry` still names an existing resource, given the entry it sits above. */
  isAvailable?: (entry: PanelEntry, parent: PanelEntry | undefined) => boolean;
  parent?: (entry: PanelEntry) => PanelEntry | undefined;
}

export interface PanelHistoryMetadata {
  sessionId: string;
  step: number;
}

export interface PanelHistoryState {
  __dotcorePanel: PanelHistoryMetadata;
}

export interface PanelClosePlan {
  historyDelta?: number;
  location: PanelLocation;
  replace: boolean;
}

export interface PanelTraversal {
  delta: number;
  location: PanelLocation;
}

export type PanelTraversalAction =
  | { type: "render" }
  | { type: "restore"; delta: number; state: PanelHistoryState & Record<string, unknown> }
  | { type: "confirm-discard" }
  | { type: "replay"; delta: number }
  | { type: "stay" };

/** The URL parameter that carries the panel stack, one entry per value. */
export const panelParameter = "panel";

const maxPanelDepth = 8;
const panelHistoryKey = "__dotcorePanel";
const panelHistoryGuardKey = "__dotcorePanelGuard";

/**
 * Pages that render panels of other pages' kinds over their own content, such as the overview
 * opening a deployment, route or machine sidebar. A panel of another kind leaves for its own page.
 */
const hostedPanelPages: Readonly<Record<string, readonly PanelPage[]>> = {
  "/overview": ["machines", "models", "routes"],
};

const screens: Record<PanelPage, readonly PanelScreen[]> = {
  "app-catalog": ["catalog"],
  apps: ["deployment", "edit", "logs"],
  machines: ["machine", "execution-target"],
  "model-catalog": ["catalog", "catalog-profile"],
  models: ["configuration", "deployment", "edit", "logs"],
  routes: ["connect", "create", "edit", "route"],
};

/** Screens that only exist beneath a parent panel, never as the first entry of a stack. */
const childScreens = new Set<PanelScreen>([
  "catalog-profile",
  "configuration",
  "connect",
  "edit",
  "execution-target",
  "logs",
]);

/** Child screens whose parent is the deployment panel for the same resource. */
const deploymentChildScreens = new Set<PanelScreen>(["configuration", "edit", "logs"]);

/** Child screens drawn over their parent's sidebar, such as a log window, instead of replacing it. */
const overlayScreens = new Set<PanelScreen>(["logs"]);

export function parsePanelEntry(value: string): PanelEntry | undefined {
  const slash = value.indexOf("/");
  const colon = value.indexOf(":", slash + 1);
  if (slash <= 0 || (colon !== -1 && colon <= slash + 1)) return undefined;

  const page = value.slice(0, slash);
  const screen = value.slice(slash + 1, colon === -1 ? undefined : colon);
  const resource = colon === -1 ? undefined : value.slice(colon + 1);
  if (!isPanelPage(page) || !isPanelScreen(page, screen)) return undefined;
  if (screen === "catalog-profile") {
    const profileSeparator = resource?.indexOf(":") ?? -1;
    const resourceId = resource?.slice(0, profileSeparator);
    const profileId = resource?.slice(profileSeparator + 1);
    return resourceId && profileId ? { page, profileId, resourceId, screen } : undefined;
  }
  if (requiresResourceId(screen) !== Boolean(resource)) return undefined;

  return resource ? { page, resourceId: resource, screen } : { page, screen };
}

export function serializePanelEntry(entry: PanelEntry): string {
  const resource = entry.resourceId
    ? `:${entry.resourceId}${entry.profileId ? `:${entry.profileId}` : ""}`
    : "";
  return `${entry.page}/${entry.screen}${resource}`;
}

/** Whether two entries name the same panel: the same page, screen, resource and profile. */
export function isSamePanelEntry(left: PanelEntry, right: PanelEntry): boolean {
  return (
    left.page === right.page &&
    left.profileId === right.profileId &&
    left.screen === right.screen &&
    left.resourceId === right.resourceId
  );
}

export function parsePanelStack(search: string | URLSearchParams): PanelEntry[] {
  const params = toParams(search);
  const stack: PanelEntry[] = [];
  for (const value of params.getAll(panelParameter)) {
    const entry = parsePanelEntry(value);
    if (!entry || !isParseablePanelAppend(stack, entry)) break;
    stack.push(entry);
  }
  return stack;
}

export function hasInvalidPanelEntries(search: string | URLSearchParams): boolean {
  return toParams(search).getAll(panelParameter).length !== parsePanelStack(search).length;
}

export function serializePanelStack(
  search: string | URLSearchParams,
  stack: readonly PanelEntry[],
): string {
  const params = toParams(search);
  params.delete(panelParameter);
  for (const entry of stack) params.append(panelParameter, serializePanelEntry(entry));
  return stringifySearch(params);
}

export function activePanel(search: string | URLSearchParams): PanelEntry | undefined {
  return parsePanelStack(search).at(-1);
}

/**
 * The entry the sidebar's back button returns to: the one beneath the entry the sidebar shows. A
 * screen drawn over its parent's sidebar leaves the sidebar showing that parent, so the back target
 * stays the parent's own.
 */
export function sidebarParent(search: string | URLSearchParams): PanelEntry | undefined {
  const stack = parsePanelStack(search);
  let shown = stack.length - 1;
  while (shown > 0 && overlayScreens.has(stack[shown].screen)) shown -= 1;

  return shown > 0 ? stack[shown - 1] : undefined;
}

export function appendPanel(search: string | URLSearchParams, entry: PanelEntry): string {
  const stack = parsePanelStack(search);
  if (!isValidPanelAppend(stack, entry)) return serializePanelStack(search, stack);
  return serializePanelStack(search, [...stack, entry]);
}

/**
 * Location pushing `entry` above the stack at `location`, on the page that renders it: the current
 * page when it hosts the entry's kind, otherwise the entry's own page. Crossing to another page
 * drops the source page's non-panel parameters.
 */
export function appendPanelLocation(location: PanelLocation, entry: PanelEntry): PanelLocation {
  return hostedLocation(location, appendPanel(location.search, entry));
}

/** Link destination of `appendPanelLocation`. */
export function appendPanelHref(location: PanelLocation, entry: PanelEntry): string {
  const target = appendPanelLocation(location, entry);
  return `${target.pathname}${target.search}`;
}

export function replaceActivePanel(search: string | URLSearchParams, entry: PanelEntry): string {
  const stack = parsePanelStack(search);
  if (stack.length) stack[stack.length - 1] = entry;
  else stack.push(entry);
  return serializePanelStack(search, normalizePanelStack(stack));
}

export function selectPanel(search: string | URLSearchParams, entry: PanelEntry): string {
  const stack = parsePanelStack(search);
  const active = stack.at(-1);
  if (active?.page === entry.page && isRootPanel(entry)) {
    // Selecting a root replaces the whole trailing run of its page, so a same-page child pushed on
    // top of another root, such as a catalog model above a deployment, cannot swallow the selection.
    while (stack.length && stack.at(-1)!.page === entry.page) stack.pop();
    stack.push(entry);
  } else if (isRootPanel(entry)) {
    stack.push(entry);
  } else if (isValidPanelAppend(stack, entry)) {
    stack.push(entry);
  }
  return serializePanelStack(search, normalizePanelStack(stack));
}

/**
 * Location selecting `entry` in the stack at `location`, as `selectPanel` does, on the page that
 * renders it: the current page when it hosts the entry's kind, otherwise the entry's own page.
 */
export function selectPanelLocation(location: PanelLocation, entry: PanelEntry): PanelLocation {
  return hostedLocation(location, selectPanel(location.search, entry));
}

/**
 * Whether `next` holds more panels than `current`. Only a deeper stack takes a new history entry;
 * switching the open panel replaces its entry, so history Back keeps returning to the parent panel.
 */
export function deepensPanelStack(
  current: string | URLSearchParams,
  next: string | URLSearchParams,
): boolean {
  return parsePanelStack(next).length > parsePanelStack(current).length;
}

export function popPanel(search: string | URLSearchParams): string {
  const stack = parsePanelStack(search);
  stack.pop();
  return serializePanelStack(search, stack);
}

export function clearPanels(search: string | URLSearchParams): string {
  return serializePanelStack(search, []);
}

/**
 * Base location after closing every panel. It is the page hosting the active panel, except when the
 * stack started on a page whose section contains the active page, such as a catalog model opened
 * from a deployment on `/models`: closing then returns to the root panel's page. Parameters other
 * than panels stay while the pathname is unchanged and are stripped when it changes.
 */
export function closePanelLocation(location: PanelLocation): PanelLocation {
  const stack = parsePanelStack(location.search);
  const root = stack[0]?.page;
  const active = stack.at(-1)?.page;
  const page = root && active && isPageInSection(active, root) ? root : active;
  const pathname = page ? panelHostPath(location.pathname, page) : location.pathname;
  return {
    pathname,
    search: searchForPathname(location.pathname, pathname, clearPanels(location.search)),
  };
}

export function planPanelClose(
  location: PanelLocation,
  state: unknown,
  contiguousSessionSteps: number,
): PanelClosePlan {
  const baseLocation = closePanelLocation(location);
  if (!isPanelHistoryState(state) || state.__dotcorePanel.step > contiguousSessionSteps) {
    return { location: baseLocation, replace: true };
  }
  return { historyDelta: -state.__dotcorePanel.step, location: baseLocation, replace: false };
}

/**
 * The path rendering a panel of `page`'s kind while the console shows `pathname`: the current page
 * when it hosts that kind, such as the overview, otherwise the kind's own page.
 */
export function panelHostPath(pathname: string, page: PanelPage): string {
  return hostedPanelPages[pathname]?.includes(page) ? pathname : panelPagePath(page);
}

export function panelPagePath(page: PanelPage): string {
  return {
    "app-catalog": "/apps/catalog",
    apps: "/apps",
    machines: "/machines",
    "model-catalog": "/models/catalog",
    models: "/models",
    routes: "/routes",
  }[page];
}

export function popPanelLocation(
  pathname: string,
  search: string | URLSearchParams,
): PanelLocation {
  const nextSearch = popPanel(search);
  const parent = activePanel(nextSearch);
  const nextPathname = parent ? panelHostPath(pathname, parent.page) : pathname;
  return {
    pathname: nextPathname,
    search: searchForPathname(pathname, nextPathname, nextSearch),
  };
}

export function canonicalizePanelLocation(
  location: PanelLocation,
  registry: PanelRegistry = {},
): CanonicalPanelLocation {
  const parsed = parsePanelStack(location.search);
  const rawEntries = toParams(location.search).getAll(panelParameter);
  const stack = normalizeAvailableStack(parsed, registry);
  const pathname = stack.length
    ? panelHostPath(location.pathname, stack.at(-1)!.page)
    : location.pathname;
  const search = searchForPathname(
    location.pathname,
    pathname,
    serializePanelStack(location.search, stack),
  );
  return {
    pathname,
    recovered:
      rawEntries.length !== stack.length ||
      location.pathname !== pathname ||
      search !== location.search,
    search,
    stack,
  };
}

export function createPanelHistoryState(
  sessionId: string,
  step: number,
  state: Record<string, unknown> = {},
): PanelHistoryState & Record<string, unknown> {
  return { ...state, [panelHistoryKey]: { sessionId, step } };
}

export function isPanelHistoryState(state: unknown): state is PanelHistoryState {
  if (!state || typeof state !== "object" || !(panelHistoryKey in state)) return false;
  const metadata = (state as PanelHistoryState)[panelHistoryKey];
  return (
    typeof metadata?.sessionId === "string" &&
    metadata.sessionId.length > 0 &&
    Number.isSafeInteger(metadata.step) &&
    metadata.step >= 0
  );
}

/** Coordinates non-cancelable browser history traversal with a dirty-form discard guard. */
export class PanelHistoryCoordinator {
  private pending?: { token: string; traversal: PanelTraversal };
  private replaying?: PanelTraversal;
  private nextToken = 0;

  traverse(traversal: PanelTraversal, dirty: boolean): PanelTraversalAction {
    if (!Number.isSafeInteger(traversal.delta) || traversal.delta === 0) {
      throw new Error("Panel traversal delta must be a non-zero integer.");
    }
    if (this.replaying && sameTraversal(this.replaying, traversal)) {
      this.replaying = undefined;
      return { type: "render" };
    }
    if (!dirty) return { type: "render" };

    const token = `panel-guard-${this.nextToken++}`;
    this.pending = { token, traversal };
    return {
      delta: -traversal.delta,
      state: createGuardedPanelHistoryState(token),
      type: "restore",
    };
  }

  restored(state: unknown): PanelTraversalAction {
    if (!this.pending || !hasPanelHistoryGuard(state, this.pending.token)) return { type: "stay" };
    return { type: "confirm-discard" };
  }

  resolveDiscard(discard: boolean): PanelTraversalAction {
    if (!this.pending) return { type: "stay" };
    const pending = this.pending;
    this.pending = undefined;
    if (!discard) return { type: "stay" };
    this.replaying = pending.traversal;
    return { delta: pending.traversal.delta, type: "replay" };
  }
}

function createGuardedPanelHistoryState(
  token: string,
): PanelHistoryState & Record<string, unknown> {
  return createPanelHistoryState("guard", 0, { [panelHistoryGuardKey]: token });
}

function hasPanelHistoryGuard(state: unknown, token: string): boolean {
  return Boolean(
    state &&
    typeof state === "object" &&
    panelHistoryGuardKey in state &&
    (state as Record<string, unknown>)[panelHistoryGuardKey] === token,
  );
}

function sameTraversal(left: PanelTraversal, right: PanelTraversal): boolean {
  return (
    left.delta === right.delta &&
    left.location.pathname === right.location.pathname &&
    left.location.search === right.location.search
  );
}

/** `search` on the page hosting its active panel, keeping page parameters only on the same page. */
function hostedLocation(location: PanelLocation, search: string): PanelLocation {
  const target = canonicalizePanelLocation({ pathname: location.pathname, search });
  return { pathname: target.pathname, search: target.search };
}

/**
 * The search a location carries on `pathname`. Parameters other than panels belong to the page they
 * were written on, so a pathname change keeps only the panel stack and the panel helpers need no
 * list of page-specific keys.
 */
function searchForPathname(fromPathname: string, pathname: string, search: string): string {
  if (fromPathname === pathname) return search;
  return serializePanelStack("", parsePanelStack(search));
}

function normalizeAvailableStack(stack: PanelEntry[], registry: PanelRegistry): PanelEntry[] {
  const available: PanelEntry[] = [];
  for (const entry of stack) {
    if (registry.isAvailable && !registry.isAvailable(entry, available.at(-1))) break;
    if (isValidPanelAppend(available, entry)) {
      available.push(entry);
      continue;
    }

    const parent = registry.parent?.(entry) ?? impliedParent(entry);
    if (
      !parent ||
      (registry.isAvailable && !registry.isAvailable(parent, available.at(-1))) ||
      !isValidPanelAppend(available, parent) ||
      !isValidPanelAppend([...available, parent], entry)
    ) {
      break;
    }
    available.push(parent, entry);
  }
  return available;
}

function normalizePanelStack(stack: PanelEntry[]): PanelEntry[] {
  const normalized: PanelEntry[] = [];
  for (const entry of stack) {
    if (!isValidPanelAppend(normalized, entry)) break;
    normalized.push(entry);
  }
  return normalized;
}

function isParseablePanelAppend(stack: readonly PanelEntry[], entry: PanelEntry): boolean {
  if (!stack.length)
    return isPanelEntry(entry) && (isRootPanel(entry) || Boolean(impliedParent(entry)));
  return isValidPanelAppend(stack, entry);
}

function isValidPanelAppend(stack: readonly PanelEntry[], entry: PanelEntry): boolean {
  if (stack.length >= maxPanelDepth || !isPanelEntry(entry)) return false;
  const parent = stack.at(-1);
  if (!parent) return isRootPanel(entry);
  if (
    isSamePanelEntry(parent, entry) ||
    stack.some((ancestor) => isSamePanelEntry(ancestor, entry))
  )
    return false;
  return isRootPanel(entry) ? parent.page !== entry.page : isChildOf(parent, entry);
}

function isRootPanel(entry: PanelEntry): boolean {
  return !childScreens.has(entry.screen);
}

function impliedParent(entry: PanelEntry): PanelEntry | undefined {
  if (entry.page === "routes" && (entry.screen === "connect" || entry.screen === "edit")) {
    return { page: "routes", resourceId: entry.resourceId, screen: "route" };
  }
  if (
    deploymentChildScreens.has(entry.screen) &&
    (entry.page === "apps" || entry.page === "models")
  ) {
    return { page: entry.page, resourceId: entry.resourceId, screen: "deployment" };
  }
  if (entry.page === "model-catalog" && entry.screen === "catalog-profile") {
    return { page: "model-catalog", resourceId: entry.resourceId, screen: "catalog" };
  }
  return undefined;
}

function isChildOf(parent: PanelEntry, entry: PanelEntry): boolean {
  if (entry.page === "routes" && (entry.screen === "connect" || entry.screen === "edit")) {
    return (
      parent.page === "routes" &&
      parent.screen === "route" &&
      parent.resourceId === entry.resourceId
    );
  }
  if (
    deploymentChildScreens.has(entry.screen) &&
    (entry.page === "apps" || entry.page === "models")
  ) {
    return (
      parent.page === entry.page &&
      parent.screen === "deployment" &&
      parent.resourceId === entry.resourceId
    );
  }
  // The URL does not say which machine owns a target, so any machine parent parses; the Machines
  // page's registry rejects a target its parent machine does not own once targets are read.
  if (entry.page === "machines" && entry.screen === "execution-target") {
    return parent.page === "machines" && parent.screen === "machine";
  }
  return (
    entry.page === "model-catalog" &&
    entry.screen === "catalog-profile" &&
    parent.page === "model-catalog" &&
    parent.resourceId === entry.resourceId &&
    parent.screen === "catalog"
  );
}

/** Whether `page` is `section` itself or a page nested beneath its path, such as `/models/catalog`. */
function isPageInSection(page: PanelPage, section: PanelPage): boolean {
  const sectionPath = panelPagePath(section);
  const path = panelPagePath(page);
  return path === sectionPath || path.startsWith(`${sectionPath}/`);
}

function isPanelEntry(entry: PanelEntry): boolean {
  return (
    isPanelPage(entry.page) &&
    isPanelScreen(entry.page, entry.screen) &&
    requiresResourceId(entry.screen) === Boolean(entry.resourceId) &&
    (entry.screen === "catalog-profile" ? Boolean(entry.profileId) : !entry.profileId)
  );
}

function requiresResourceId(screen: PanelScreen): boolean {
  return screen !== "create";
}

function isPanelPage(value: string): value is PanelPage {
  return value in screens;
}

function isPanelScreen(page: PanelPage, value: string): value is PanelScreen {
  return screens[page].includes(value as PanelScreen);
}

function toParams(search: string | URLSearchParams): URLSearchParams {
  return typeof search === "string" ? new URLSearchParams(search) : new URLSearchParams(search);
}

function stringifySearch(params: URLSearchParams): string {
  const serialized = params.toString();
  return serialized ? `?${serialized}` : "";
}
