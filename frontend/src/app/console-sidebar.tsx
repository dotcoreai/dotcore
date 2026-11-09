import { Collapsible } from "@base-ui/react/collapsible";
import type { LucideIcon } from "lucide-react";
import { ChevronRight, PanelLeftClose, PanelLeftOpen } from "lucide-react";
import type { FocusEvent, KeyboardEvent, MouseEvent } from "react";
import { useCallback, useEffect, useRef, useState } from "react";
import { NavLink, useLocation } from "react-router-dom";

import {
  AppsSectionIcon,
  CatalogSectionIcon,
  ChatSectionIcon,
  MachinesSectionIcon,
  ModelsSectionIcon,
  OverviewSectionIcon,
  RoutesSectionIcon,
  SettingsSectionIcon,
} from "@/components/ui/section-icons";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSub,
  SidebarMenuSubButton,
  SidebarMenuSubItem,
  SidebarRail,
  SidebarSeparator,
  useSidebar,
} from "@/components/ui/sidebar";
import { usePanelNavigationPending } from "@/panels/panel-navigation-pending";

interface NavChildItem {
  icon: LucideIcon;
  label: string;
  path: string;
}

interface NavItem {
  icon: LucideIcon;
  label: string;
  /** Absent for a section with no route yet; renders disabled instead of linked. */
  path?: string;
  /** Present only on a parent; renders a disclosure toggle instead of a link. A parent does not
   * also carry `path`: it never navigates itself. */
  children?: NavChildItem[];
}

const primaryNavItems: NavItem[] = [
  { icon: OverviewSectionIcon, label: "Overview", path: "/overview" },
  { icon: MachinesSectionIcon, label: "Machines", path: "/machines" },
  {
    icon: ModelsSectionIcon,
    label: "Models",
    children: [
      { icon: ModelsSectionIcon, label: "Models", path: "/models" },
      { icon: CatalogSectionIcon, label: "Catalog", path: "/models/catalog" },
    ],
  },
  { icon: RoutesSectionIcon, label: "Routes", path: "/routes" },
  {
    icon: AppsSectionIcon,
    label: "Apps",
    children: [
      { icon: AppsSectionIcon, label: "Apps", path: "/apps" },
      { icon: CatalogSectionIcon, label: "Catalog", path: "/apps/catalog" },
    ],
  },
];

const footerNavItem: NavItem = { icon: SettingsSectionIcon, label: "Settings" };

/**
 * Icon-collapsible left navigation with direct destinations and optional disclosure groups.
 * Settings has no route yet and renders disabled. Primary items default to the console
 * destinations; supplied items use the same linked, disabled and collapsible row treatments.
 */
export function ConsoleSidebar({
  chatCurrent = false,
  chatOpen = false,
  items = primaryNavItems,
  onChatToggle,
}: {
  /** Whether the Chat page is the current page, which marks the Chat item current. */
  chatCurrent?: boolean;
  chatOpen?: boolean;
  items?: NavItem[];
  /** Present when the console offers chat; renders its toggle above Settings. */
  onChatToggle?: () => void;
} = {}) {
  const { pathname } = useLocation();
  const { state } = useSidebar();
  const { hovering, onPointerEnter, onPointerLeave, suppressHover } = useHoverPeek();
  const { focusWithin, onBlur, onFocus, onKeyDown, onPointerDown, suppressFocus } =
    useFocusWithinPeek();
  const [renderedState, setRenderedState] = useState(state);
  // Collapse, the rail handle, and the keyboard shortcut all collapse with the pointer or focus
  // already inside the sidebar, so that hover or focus must not re-open the new rail as a peek.
  if (state !== renderedState) {
    setRenderedState(state);
    if (state === "collapsed") {
      suppressHover();
      suppressFocus();
    }
  }
  // Only the collapsed rail peeks: an expanded sidebar already shows everything hover or focus
  // would reveal, and this keeps either from doing anything once the user has expanded it.
  const peek = state === "collapsed" && (hovering || focusWithin);

  return (
    <Sidebar
      className="top-(--header-height) h-[calc(100dvh-var(--header-height))]"
      collapsible="icon"
      onBlur={onBlur}
      onFocus={onFocus}
      onKeyDown={onKeyDown}
      onPointerDown={onPointerDown}
      onPointerEnter={onPointerEnter}
      onPointerLeave={onPointerLeave}
      peek={peek}
      side="left"
    >
      <SidebarContent>
        <SidebarGroup>
          <SidebarMenu>
            {items.map((item) => (
              <NavMenuItem item={item} key={item.label} pathname={pathname} />
            ))}
          </SidebarMenu>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <SidebarMenu>
          {onChatToggle ? (
            <ChatMenuItem current={chatCurrent} onToggle={onChatToggle} open={chatOpen} />
          ) : null}
          <NavMenuItem item={footerNavItem} pathname={pathname} />
        </SidebarMenu>
        <SidebarSeparator />
        <SidebarMenu>
          <SidebarToggleMenuItem />
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}

function NavMenuItem({ item, pathname }: { item: NavItem; pathname: string }) {
  const { icon: Icon, label, path, children } = item;
  const closeMobileNavigation = useCloseMobileNavigation();

  if (children) {
    return <NavParentMenuItem icon={Icon} items={children} label={label} pathname={pathname} />;
  }

  return (
    <SidebarMenuItem>
      {path ? (
        <SidebarMenuButton
          isActive={pathname === path}
          render={
            <NavLink data-panel-navigation="" end onClick={closeMobileNavigation} to={path} />
          }
          tooltip={label}
        >
          <Icon />
          <span>{label}</span>
        </SidebarMenuButton>
      ) : (
        // `disabled` is consumed by TooltipTrigger as "suppress the tooltip," not forwarded to
        // the DOM (see @base-ui/react/tooltip's TooltipTrigger), so a real disabled/non-focusable
        // state needs aria-disabled and tabIndex explicitly. sidebarMenuButtonVariants already
        // styles aria-disabled the same as disabled.
        <SidebarMenuButton aria-disabled="true" tabIndex={-1} tooltip={label}>
          <Icon />
          <span>{label}</span>
        </SidebarMenuButton>
      )}
    </SidebarMenuItem>
  );
}

/**
 * Parent row: a pure disclosure toggle over a `SidebarMenuSub` of children, never a link itself.
 * Expansion is derived from the location rather than stored, and re-derived only when the location
 * changes, so a manual collapse survives unrelated re-renders.
 */
function NavParentMenuItem({
  icon: Icon,
  items,
  label,
  pathname,
}: {
  icon: LucideIcon;
  items: NavChildItem[];
  label: string;
  pathname: string;
}) {
  const { state } = useSidebar();
  const closeMobileNavigation = useCloseMobileNavigation();
  const branchActive = items.some((child) => isBranchActive(pathname, child.path));
  const [open, setOpen] = useDerivedOpen(pathname, branchActive);
  // The parent highlights only while its children are hidden: the branch is closed, or the whole
  // sidebar is collapsed to the icon rail, where children never render regardless of `open`. While
  // the branch is open and the rail is expanded, the active child carries the signal alone.
  const parentActive = branchActive && (!open || state === "collapsed");

  return (
    <Collapsible.Root onOpenChange={setOpen} open={open} render={<SidebarMenuItem />}>
      <Collapsible.Trigger
        render={
          <SidebarMenuButton isActive={parentActive} tooltip={label}>
            <Icon />
            <span>{label}</span>
            {/* `data-panel-open` lands on the trigger button itself, not on this descendant svg, so
              the rotation has to key off the button's own named group rather than a same-element
              variant. */}
            <ChevronRight className="ml-auto transition-transform group-data-panel-open/menu-button:rotate-90" />
          </SidebarMenuButton>
        }
      />
      <Collapsible.Panel>
        <SidebarMenuSub>
          {items.map((child) => (
            <SidebarMenuSubItem key={child.path}>
              <SidebarMenuSubButton
                isActive={pathname === child.path}
                render={
                  <NavLink
                    data-panel-navigation=""
                    end
                    onClick={closeMobileNavigation}
                    to={child.path}
                  />
                }
              >
                <child.icon />
                <span>{child.label}</span>
              </SidebarMenuSubButton>
            </SidebarMenuSubItem>
          ))}
        </SidebarMenuSub>
      </Collapsible.Panel>
    </Collapsible.Root>
  );
}

/**
 * Toggle for the chat panel, marked current on the Chat page; on mobile it also dismisses the drawer
 * so the chat is visible.
 */
function ChatMenuItem({
  current,
  onToggle,
  open,
}: {
  current: boolean;
  onToggle: () => void;
  open: boolean;
}) {
  const { isMobile, setOpenMobile } = useSidebar();

  return (
    <SidebarMenuItem>
      <SidebarMenuButton
        aria-current={current ? "page" : undefined}
        aria-pressed={open}
        isActive={open || current}
        onClick={() => {
          onToggle();
          if (isMobile) setOpenMobile(false);
        }}
        tooltip="Chat"
      >
        <ChatSectionIcon />
        <span>Chat</span>
      </SidebarMenuButton>
    </SidebarMenuItem>
  );
}

/**
 * Explicit, always-visible affordance for the sidebar's expanded/collapsed state, pairing with the
 * ambient `SidebarRail` drag handle. States its outcome rather than its name: label and icon flip
 * with `useSidebar()` state instead of reading "Toggle sidebar" at both states.
 */
function SidebarToggleMenuItem() {
  const { isMobile, state, toggleSidebar } = useSidebar();
  const expanded = state === "expanded";
  // `state` only tracks the desktop `open` value, not the drawer's `openMobile`. On mobile this
  // item is only ever rendered inside an already-open drawer, so it is always a close affordance:
  // "Collapse"/"Expand" are rail language and the off-canvas drawer dismisses rather than collapses.
  const label = isMobile ? "Close" : expanded ? "Collapse" : "Expand";
  const Icon = isMobile || expanded ? PanelLeftClose : PanelLeftOpen;

  return (
    <SidebarMenuItem>
      <SidebarMenuButton onClick={toggleSidebar} tooltip={label}>
        <Icon />
        <span>{label}</span>
      </SidebarMenuButton>
    </SidebarMenuItem>
  );
}

/** Segment-aware: a location is inside `base`'s branch only at `base` itself or beneath it, so a
 * sibling like `/models-archive` is never read as inside a `/models` branch. */
function isBranchActive(pathname: string, base: string): boolean {
  return pathname === base || pathname.startsWith(`${base}/`);
}

/**
 * Expansion derived from the location: open on first paint when the branch is already active, and
 * re-opened when navigation enters the branch. Re-derives only when `pathname` changes, not on
 * every render, so a manual collapse survives re-renders that are not navigations.
 */
function useCloseMobileNavigation() {
  const navigationPending = usePanelNavigationPending();
  const { isMobile, setOpenMobile } = useSidebar();

  return useCallback(
    (event: MouseEvent<HTMLAnchorElement>) => {
      if (navigationPending) {
        event.preventDefault();
        return;
      }
      if (
        !isMobile ||
        event.defaultPrevented ||
        event.button !== 0 ||
        event.metaKey ||
        event.ctrlKey ||
        event.shiftKey ||
        event.altKey
      ) {
        return;
      }

      setOpenMobile(false);
    },
    [isMobile, navigationPending, setOpenMobile],
  );
}

function useDerivedOpen(pathname: string, branchActive: boolean) {
  const [open, setOpen] = useState(branchActive);
  const [renderedPathname, setRenderedPathname] = useState(pathname);

  if (pathname !== renderedPathname) {
    setRenderedPathname(pathname);
    if (branchActive) {
      setOpen(true);
    }
  }

  return [open, setOpen] as const;
}

// A graze across the rail on the way to somewhere else should not open it, and a deliberate hover
// away should not close it mid-transit toward a revealed sub-item — hence the asymmetric delays.
// Exported so tests exercise the real values instead of a duplicated guess.
export const PEEK_ENTER_DELAY_MS = 150;
export const PEEK_LEAVE_DELAY_MS = 300;

/**
 * Debounced pointer-hover state for the peek. Local to the caller and cleared on unmount, so it
 * never persists anywhere and never reaches the stored collapsed preference. `suppressHover` hides
 * the current hover until the pointer enters the sidebar again.
 */
function useHoverPeek() {
  const [hovering, setHovering] = useState(false);
  const [suppressed, setSuppressed] = useState(false);
  const timeoutRef = useRef<ReturnType<typeof setTimeout>>(undefined);

  useEffect(() => () => clearTimeout(timeoutRef.current), []);

  const onPointerEnter = useCallback(() => {
    clearTimeout(timeoutRef.current);
    // A new entry after suppression starts from not hovering, so it still waits the enter delay.
    if (suppressed) {
      setSuppressed(false);
      setHovering(false);
    }
    timeoutRef.current = setTimeout(() => setHovering(true), PEEK_ENTER_DELAY_MS);
  }, [suppressed]);

  const onPointerLeave = useCallback(() => {
    clearTimeout(timeoutRef.current);
    timeoutRef.current = setTimeout(() => setHovering(false), PEEK_LEAVE_DELAY_MS);
  }, []);

  const suppressHover = useCallback(() => setSuppressed(true), []);

  return { hovering: hovering && !suppressed, onPointerEnter, onPointerLeave, suppressHover };
}

/**
 * Immediate focus-within state for the peek. Deliberately undelayed, unlike hover: every keyboard
 * focus move is a deliberate action, so there is no graze to debounce on entry, and a delay on
 * exit would only lag behind where focus is visibly already sitting. Tab moving between two
 * elements that are both still inside the rail is not a leave — checking the blur's
 * `relatedTarget` against the container mirrors what the `:focus-within` pseudo-class does
 * natively, so tabbing through revealed rows never flickers the peek shut and reopen.
 *
 * Focus-visible identifies keyboard entry. Pointer intent explicitly releases that entry even when
 * the browser retains its focus-visible indication; focus produced by the same pointer gesture
 * cannot reopen it. Keyboard input or leaving and re-entering restores keyboard entry.
 *
 * `suppressFocus` hides the current focus until focus leaves the sidebar and enters it again.
 */
function useFocusWithinPeek() {
  const [focusWithin, setFocusWithin] = useState(false);
  const [suppressed, setSuppressed] = useState(false);
  const pointerIntent = useRef(false);

  const onFocus = useCallback((event: FocusEvent<HTMLDivElement>) => {
    setFocusWithin(!pointerIntent.current && event.target.matches(":focus-visible"));
  }, []);

  const onBlur = useCallback((event: FocusEvent<HTMLDivElement>) => {
    const next = event.relatedTarget;
    if (next instanceof Node && event.currentTarget.contains(next)) {
      return;
    }
    pointerIntent.current = false;
    setFocusWithin(false);
    setSuppressed(false);
  }, []);

  const onPointerDown = useCallback(() => {
    pointerIntent.current = true;
    setFocusWithin(false);
  }, []);

  const onKeyDown = useCallback((event: KeyboardEvent<HTMLDivElement>) => {
    pointerIntent.current = false;
    setFocusWithin(event.target instanceof HTMLElement && event.target.matches(":focus-visible"));
  }, []);

  const suppressFocus = useCallback(() => setSuppressed(true), []);

  return {
    focusWithin: focusWithin && !suppressed,
    onBlur,
    onFocus,
    onKeyDown,
    onPointerDown,
    suppressFocus,
  };
}
