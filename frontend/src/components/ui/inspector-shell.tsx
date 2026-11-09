import type { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { ChevronLeftIcon, XIcon } from "lucide-react";
import type { CSSProperties, ReactNode, RefObject } from "react";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { narrowInspectorMaxWidth } from "@/components/ui/inspector-breakpoint";
import {
  InspectorFloatingLayerContext,
  type InspectorFloatingLayerRegistry,
  useInspectorFloatingLayerRegistration,
} from "@/components/ui/inspector-floating-layer";
import { cn } from "@/lib/utils";
import { usePanelPresentation } from "@/panels/panel-presentation";
import { usePanelParent } from "@/panels/panel-titles-context";

/**
 * Whether a modal over the inspector suspends its dismissal, and how many dismissals that modal
 * turned down, such as an operator keeping an editor open instead of discarding it.
 */
const InspectorDismissalSuspensionContext = createContext({ active: false, refusals: 0 });

type InspectorDismissReason = "close-press" | "escape-key" | "outside-press";

interface InspectorOpenChangeDetails {
  reason: InspectorDismissReason;
}

interface InspectorShellProps {
  body: ReactNode;
  disablePointerDismissal?: boolean;
  floatingLayer?: ReactNode;
  footer?: ReactNode;
  focusKey?: string | number;
  id: string;
  navigationDisabledReason?: string;
  onBack?: () => void;
  onDismissGuard?: () => void;
  onOpenChange: (open: boolean, details: InspectorOpenChangeDetails) => void;
  open: boolean;
  openerRef: RefObject<HTMLElement | null>;
  recoveryMessage?: string;
  /** A status badge beside the title, outside the heading so it is not part of the panel name. */
  status?: ReactNode;
  suspendDismissal?: boolean;
  title: ReactNode;
}

function InspectorDismissalSuspension({
  active,
  children,
  refusals = 0,
}: {
  active: boolean;
  children: ReactNode;
  /** Counts the dismissals turned down; each one leaves the inspector open as it was. */
  refusals?: number;
}) {
  const value = useMemo(() => ({ active, refusals }), [active, refusals]);

  return (
    <InspectorDismissalSuspensionContext.Provider value={value}>
      {children}
    </InspectorDismissalSuspensionContext.Provider>
  );
}

function InspectorShell({
  body,
  disablePointerDismissal = false,
  floatingLayer,
  footer,
  focusKey,
  id,
  navigationDisabledReason,
  onBack,
  onDismissGuard,
  onOpenChange,
  open,
  openerRef,
  recoveryMessage,
  status,
  suspendDismissal = false,
  title,
}: InspectorShellProps) {
  const presentation = usePanelPresentation();
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const previousPanel = useRef({ focusKey, open: false });
  const dismissalReason = useRef<InspectorDismissReason | undefined>(undefined);
  const outsideDismissTarget = useRef<HTMLElement | null>(null);
  const floatingLayers = useRef(new Set<symbol>());
  const [openFloatingLayerCount, setOpenFloatingLayerCount] = useState(0);
  const suspension = useContext(InspectorDismissalSuspensionContext);
  const providerSuspendsDismissal = suspension.active;
  const onOpenChangeRef = useRef(onOpenChange);
  const openRef = useRef(open);
  const isNarrow = useNarrowInspector();
  const navigationIsDisabled = Boolean(navigationDisabledReason);
  const floatingLayerRegistry = useMemo<InspectorFloatingLayerRegistry>(
    () => ({
      register(token, layerOpen) {
        if (layerOpen) {
          floatingLayers.current.add(token);
          setOpenFloatingLayerCount(floatingLayers.current.size);
          return;
        }

        floatingLayers.current.delete(token);
        // Keep the parent suspended through the outside activation that closed this child layer.
        queueMicrotask(() => setOpenFloatingLayerCount(floatingLayers.current.size));
      },
    }),
    [],
  );
  const floatingLayerOpen = openFloatingLayerCount > 0;
  const coveredByModal = suspendDismissal || providerSuspendsDismissal;
  const childModalLayerStyle = coveredByModal
    ? { zIndex: "calc(var(--layer-modal) - 1)" }
    : undefined;

  useLayoutEffect(() => {
    onOpenChangeRef.current = onOpenChange;
  }, [onOpenChange]);

  useLayoutEffect(() => {
    openRef.current = open;
  }, [open]);

  const requestDismiss = useCallback(
    (reason: InspectorDismissReason, target?: EventTarget | null) => {
      if (
        floatingLayerOpen ||
        providerSuspendsDismissal ||
        suspendDismissal ||
        navigationIsDisabled
      )
        return false;
      if (onDismissGuard) {
        onDismissGuard();
        return false;
      }
      dismissalReason.current = reason;
      outsideDismissTarget.current =
        reason === "outside-press" && target instanceof HTMLElement ? target : null;
      onOpenChangeRef.current(false, { reason });
      return true;
    },
    [
      floatingLayerOpen,
      navigationIsDisabled,
      onDismissGuard,
      providerSuspendsDismissal,
      suspendDismissal,
    ],
  );

  // A turned-down dismissal leaves the panel open, so a later unmount while open is no close.
  useLayoutEffect(() => {
    dismissalReason.current = undefined;
    outsideDismissTarget.current = null;
  }, [suspension.refusals]);

  useLayoutEffect(() => {
    const previous = previousPanel.current;
    previousPanel.current = { focusKey, open };

    if (open && !previous.open) {
      dismissalReason.current = undefined;
      outsideDismissTarget.current = null;
      return;
    }
    if (open && previous.open && focusKey !== undefined && focusKey !== previous.focusKey) {
      headingRef.current?.focus();
    }
  }, [focusKey, open]);

  const header = {
    navigationDisabledReason,
    onBack,
    status,
    title,
  };

  const inspector = !isNarrow ? (
    <WideInspector
      body={body}
      footer={footer}
      header={header}
      closeButtonRef={closeButtonRef}
      headingRef={headingRef}
      id={id}
      coveredByModal={coveredByModal}
      dismissalBlocked={floatingLayerOpen || coveredByModal || navigationIsDisabled}
      disablePointerDismissal={
        disablePointerDismissal || floatingLayerOpen || coveredByModal || navigationIsDisabled
      }
      finalFocus={() => {
        // Crossing into the narrow modal unmounts this panel while open; the modal takes focus. A
        // dismissal the operator requested is a real close, even when the host unmounts the panel
        // before its open state updates.
        if (openRef.current && dismissalReason.current === undefined) return false;
        if (dismissalReason.current === "outside-press") {
          return outsideDismissTarget.current ?? false;
        }
        return openerRef.current;
      }}
      initialFocusRef={focusKey !== undefined ? headingRef : closeButtonRef}
      onDismiss={requestDismiss}
      open={open}
      recoveryMessage={recoveryMessage}
      presentation={presentation}
      childModalLayerStyle={childModalLayerStyle}
    />
  ) : (
    <Dialog
      disablePointerDismissal={
        disablePointerDismissal || floatingLayerOpen || coveredByModal || navigationIsDisabled
      }
      key="narrow-inspector"
      modal={!coveredByModal}
      open={open}
      onOpenChange={(nextOpen, details) => {
        if (!nextOpen) requestDismiss(dialogDismissReason(details));
      }}
    >
      <DialogContent
        aria-hidden={coveredByModal ? "true" : undefined}
        aria-modal={coveredByModal ? undefined : "true"}
        className={panelClassName(
          presentation,
          "inset-0 flex h-[100dvh] max-w-none flex-col gap-0 rounded-none border-0 p-0 sm:max-w-none",
        )}
        data-testid="inspector-content"
        finalFocus={openerRef}
        inert={coveredByModal ? true : undefined}
        initialFocus={coveredByModal ? false : focusKey !== undefined ? headingRef : closeButtonRef}
        showCloseButton={false}
        style={childModalLayerStyle}
        suppressLayoutTransform
        suppressMotion
      >
        <InspectorHeader
          {...header}
          dialogTitle
          closeControl={
            <CloseButton
              disabled={navigationIsDisabled}
              disabledReason={navigationDisabledReason}
              onClick={() => requestDismiss("close-press")}
              ref={closeButtonRef}
            />
          }
          headingRef={headingRef}
        />
        {recoveryMessage ? <RecoveryNotice message={recoveryMessage} /> : null}
        <InspectorBody>{body}</InspectorBody>
        {footer ? <InspectorFooter>{footer}</InspectorFooter> : null}
        {/* Last, so the header's refs are attached when it runs. */}
        <FocusOnMount
          target={coveredByModal ? undefined : focusKey !== undefined ? headingRef : closeButtonRef}
        />
      </DialogContent>
    </Dialog>
  );

  return (
    <InspectorFloatingLayerContext.Provider value={floatingLayerRegistry}>
      {inspector}
      {floatingLayer}
    </InspectorFloatingLayerContext.Provider>
  );
}

function WideInspector({
  body,
  footer,
  header,
  childModalLayerStyle,
  closeButtonRef,
  coveredByModal,
  headingRef,
  id,
  dismissalBlocked,
  disablePointerDismissal,
  finalFocus,
  initialFocusRef,
  onDismiss,
  open,
  recoveryMessage,
  presentation,
}: {
  body: ReactNode;
  footer: ReactNode;
  header: Omit<InspectorHeaderProps, "closeControl" | "headingRef">;
  childModalLayerStyle?: CSSProperties;
  closeButtonRef: RefObject<HTMLButtonElement | null>;
  coveredByModal: boolean;
  headingRef: RefObject<HTMLHeadingElement | null>;
  id: string;
  dismissalBlocked: boolean;
  disablePointerDismissal: boolean;
  finalFocus: () => HTMLElement | false | null;
  initialFocusRef: RefObject<HTMLElement | null>;
  onDismiss: (reason: InspectorDismissReason, target?: EventTarget | null) => boolean;
  open: boolean;
  recoveryMessage?: string;
  presentation: ReturnType<typeof usePanelPresentation>;
}) {
  const titleId = useId();
  const navigationIsDisabled = Boolean(header.navigationDisabledReason);

  return (
    <Dialog
      disablePointerDismissal={disablePointerDismissal}
      key="wide-inspector"
      modal={false}
      onOpenChange={(nextOpen, details) => {
        if (nextOpen) return;
        if (dismissalBlocked) {
          details.cancel();
          details.allowPropagation();
          return;
        }
        if (details.reason === "focus-out") {
          details.cancel();
          return;
        }
        if (
          (details.reason === "outside-press" || details.reason === "escape-key") &&
          isInspectorCompanionTarget(details.event.target)
        ) {
          details.cancel();
          details.allowPropagation();
          return;
        }
        if (details.reason === "outside-press" && isPanelNavigationTarget(details.event.target)) {
          details.cancel();
          details.allowPropagation();
          return;
        }
        const reason = dialogDismissReason(details);
        if (reason === "outside-press") {
          details.cancel();
          if (onDismiss(reason, details.event.target)) details.allowPropagation();
          return;
        }
        onDismiss(reason, details.event.target);
      }}
      open={open}
    >
      <DialogContent
        aria-hidden={coveredByModal ? "true" : undefined}
        aria-labelledby={titleId}
        className={panelClassName(
          presentation,
          "fixed z-50 flex flex-col gap-0 rounded-l-md rounded-r-none border-l p-2 ring-0",
        )}
        data-testid="inspector-content"
        id={id}
        finalFocus={finalFocus}
        inert={coveredByModal ? true : undefined}
        initialFocus={coveredByModal ? false : initialFocusRef}
        // Lets the console chat find the wide inspector it may sit beside.
        data-inspector-panel=""
        render={<aside />}
        role="complementary"
        showCloseButton={false}
        showOverlay={false}
        suppressLayoutTransform
        suppressMotion
        style={{
          ...childModalLayerStyle,
          bottom: 0,
          left: "auto",
          maxWidth: "calc(100vw - 28px)",
          right: 0,
          top: "var(--header-height)",
          width: 460,
        }}
      >
        <InspectorHeader
          {...header}
          closeControl={
            <CloseButton
              disabled={navigationIsDisabled}
              disabledReason={header.navigationDisabledReason}
              onClick={() => onDismiss("close-press")}
              ref={closeButtonRef}
            />
          }
          headingId={titleId}
          headingRef={headingRef}
        />
        {recoveryMessage ? <RecoveryNotice message={recoveryMessage} /> : null}
        <InspectorBody>{body}</InspectorBody>
        {footer ? <InspectorFooter>{footer}</InspectorFooter> : null}
      </DialogContent>
    </Dialog>
  );
}

interface InspectorHeaderProps {
  closeControl: ReactNode;
  dialogTitle?: boolean;
  headingId?: string;
  headingRef: RefObject<HTMLHeadingElement | null>;
  navigationDisabledReason?: string;
  onBack?: () => void;
  status?: ReactNode;
  title: ReactNode;
}

function InspectorHeader({
  closeControl,
  dialogTitle = false,
  headingId,
  headingRef,
  navigationDisabledReason,
  onBack,
  status,
  title,
}: InspectorHeaderProps) {
  const navigationIsDisabled = Boolean(navigationDisabledReason);
  const descriptionId = navigationIsDisabled
    ? `${headingId ?? "inspector"}-navigation-disabled`
    : undefined;

  // The back target is the panel beneath the active one; pages gate `onBack` on their own state.
  const parent = usePanelParent();
  const hasBack = Boolean(parent && onBack);
  const heading = dialogTitle ? (
    <DialogTitle
      className="min-w-0 self-center text-[17px] leading-6 font-semibold outline-none"
      id={headingId}
      ref={headingRef}
      tabIndex={-1}
    >
      {title}
    </DialogTitle>
  ) : (
    <h2
      className="min-w-0 self-center text-[17px] leading-6 font-semibold outline-none"
      id={headingId}
      ref={headingRef}
      tabIndex={-1}
    >
      {title}
    </h2>
  );

  return (
    <DialogHeader
      className={cn(
        "relative grid shrink-0 border-b px-[17px] py-2",
        hasBack ? "grid-cols-[22px_minmax(0,1fr)_44px]" : "grid-cols-[minmax(0,1fr)_44px]",
      )}
    >
      {hasBack ? (
        <Button
          aria-describedby={descriptionId}
          aria-label={`Back to ${parent!.label}`}
          className={cn(
            "-ml-3.5 size-11 justify-center self-center text-muted-foreground",
            !navigationIsDisabled && "cursor-pointer",
          )}
          disabled={navigationIsDisabled}
          onClick={onBack}
          type="button"
          variant="ghost"
        >
          <ChevronLeftIcon aria-hidden="true" className="size-4" />
        </Button>
      ) : null}
      {status ? (
        <div className="flex min-w-0 items-center justify-between gap-3 self-center">
          {heading}
          {status}
        </div>
      ) : (
        heading
      )}
      {navigationIsDisabled ? (
        <span className="sr-only" id={descriptionId}>
          {navigationDisabledReason}
        </span>
      ) : null}
      {closeControl}
    </DialogHeader>
  );
}

function CloseButton({
  disabled = false,
  disabledReason,
  onClick,
  ref,
}: {
  disabled?: boolean;
  disabledReason?: string;
  onClick?: () => void;
  ref?: RefObject<HTMLButtonElement | null>;
}) {
  const descriptionId = disabledReason ? "inspector-close-disabled" : undefined;
  return (
    <>
      <Button
        aria-describedby={descriptionId}
        aria-label="Close inspector"
        className={cn("size-11 self-center text-muted-foreground", !disabled && "cursor-pointer")}
        disabled={disabled}
        onClick={onClick}
        ref={ref}
        size="icon"
        type="button"
        variant="ghost"
      >
        <XIcon aria-hidden="true" className="size-4" />
      </Button>
      {disabledReason ? (
        <span className="sr-only" id={descriptionId}>
          {disabledReason}
        </span>
      ) : null}
    </>
  );
}

/**
 * Focuses `target` as soon as the modal content mounts. Base UI hides the rest of the page from
 * assistive tech before its own initial focus runs, which leaves focus on a hidden opener, so the
 * narrow inspector takes focus first.
 */
function FocusOnMount({ target }: { target?: RefObject<HTMLElement | null> }) {
  // Mount only: later focus moves belong to the operator and the focus-key effect.
  const initialTarget = useRef(target);

  useLayoutEffect(() => {
    initialTarget.current?.current?.focus();
  }, []);

  return null;
}

function RecoveryNotice({ message }: { message: string }) {
  return (
    <p className="sr-only" role="status">
      {message}
    </p>
  );
}

function InspectorBody({ children }: { children: ReactNode }) {
  return (
    <div
      className="min-h-0 flex-1 overflow-y-auto px-[17px] py-[17px]"
      data-testid="inspector-body"
    >
      {children}
    </div>
  );
}

function InspectorFooter({ children }: { children: ReactNode }) {
  return (
    // Actions keep their natural width in one right-aligned row on every viewport, instead of the
    // dialog footer's full-width stack, wrapping only when their labels cannot fit side by side.
    <DialogFooter
      aria-label="Inspector actions"
      className="sticky bottom-0 z-10 m-0 shrink-0 flex-row flex-wrap items-center justify-end rounded-none border-t border-input bg-card px-[17px] py-3"
      data-slot="inspector-footer"
      data-testid="inspector-footer"
    >
      {children}
    </DialogFooter>
  );
}

function dialogDismissReason(
  details: DialogPrimitive.Root.ChangeEventDetails,
): InspectorDismissReason {
  if (details.reason === "outside-press") return "outside-press";
  if (details.reason === "escape-key") return "escape-key";
  return "close-press";
}

function isPanelNavigationTarget(target: EventTarget | null): boolean {
  return target instanceof Element && Boolean(target.closest("[data-panel-navigation]"));
}

/** A target inside a companion surface, such as the console chat, that sits beside the inspector. */
function isInspectorCompanionTarget(target: EventTarget | null): boolean {
  return target instanceof Element && Boolean(target.closest("[data-inspector-companion]"));
}

function panelClassName(presentation: ReturnType<typeof usePanelPresentation>, layout: string) {
  const openingMotion =
    presentation === "enter"
      ? "transition-transform duration-160 ease-out data-[starting-style]:translate-x-full motion-reduce:transition-none"
      : "";
  return `${layout} bg-card text-sm text-card-foreground ${openingMotion}`;
}

function viewportIsNarrow(): boolean {
  return typeof window !== "undefined" && window.innerWidth <= narrowInspectorMaxWidth;
}

function useNarrowInspector(): boolean {
  const [isNarrow, setIsNarrow] = useState(viewportIsNarrow);

  useEffect(() => {
    const update = () => setIsNarrow(viewportIsNarrow());
    window.addEventListener("resize", update);
    window.visualViewport?.addEventListener("resize", update);
    update();

    return () => {
      window.removeEventListener("resize", update);
      window.visualViewport?.removeEventListener("resize", update);
    };
  }, []);

  return isNarrow;
}

// eslint-disable-next-line react-refresh/only-export-components
export { InspectorDismissalSuspension, InspectorShell, useInspectorFloatingLayerRegistration };
export type { InspectorDismissReason, InspectorOpenChangeDetails, InspectorShellProps };
