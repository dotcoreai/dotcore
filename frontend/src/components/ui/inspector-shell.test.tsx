import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { type ReactNode, useRef, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { PanelPresentationContext } from "@/app/panel-presentation";
import { PanelTitlesProvider } from "@/app/panel-titles";
import { PanelParentContext, usePublishPanelTitles } from "@/app/panel-titles-context";
import { Combobox, type ComboboxOption } from "@/components/ui/combobox";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import {
  InspectorDismissalSuspension,
  type InspectorDismissReason,
  InspectorShell,
} from "@/components/ui/inspector-shell";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { panelTitle } from "@/lib/panel-titles";

const popupOptions: ComboboxOption[] = [{ id: "deployment-alpha", label: "Deployment Alpha" }];

function ControlledInspector() {
  const [open, setOpen] = useState(false);
  const [dismissReason, setDismissReason] = useState<InspectorDismissReason>();
  const [dismissRequests, setDismissRequests] = useState(0);
  const [outsideActions, setOutsideActions] = useState(0);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open inspector
      </button>
      <button type="button" onClick={() => setOutsideActions((count) => count + 1)}>
        Outside action {outsideActions}
      </button>
      <output aria-label="Dismiss reason">{dismissReason ?? "none"}</output>
      <output aria-label="Dismiss requests">{dismissRequests}</output>
      <InspectorShell
        body={
          <>
            <button type="button">Body action</button>
            <p>Inspector content</p>
          </>
        }
        footer={<button type="button">Load</button>}
        focusKey="inspector-title"
        id="inspector-panel"
        onOpenChange={(nextOpen, details) => {
          setDismissReason(details.reason);
          setDismissRequests((count) => count + 1);
          setOpen(nextOpen);
        }}
        open={open}
        openerRef={openerRef}
        title="Inspector title"
      />
    </>
  );
}

function PopupInspector({ tooltip = false }: { tooltip?: boolean }) {
  const [open, setOpen] = useState(false);
  const [outsideActions, setOutsideActions] = useState(0);
  const [deployment, setDeployment] = useState<string | null>(null);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open popup inspector
      </button>
      <button type="button" onClick={() => setOutsideActions((count) => count + 1)}>
        Outside popup action {outsideActions}
      </button>
      <output aria-label="Selected deployment">{deployment ?? "none"}</output>
      <InspectorShell
        body={
          tooltip ? (
            <Tooltip>
              <TooltipTrigger render={<button type="button" />}>Deployment help</TooltipTrigger>
              <TooltipContent>Choose a deployment</TooltipContent>
            </Tooltip>
          ) : (
            <Combobox
              label="Deployment"
              onValueChange={setDeployment}
              options={popupOptions}
              value={deployment}
            />
          )
        }
        id="popup-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        title="Popup inspector"
      />
    </>
  );
}

/** A panel beneath the inspector, so the header has a parent to label its Back control with. */
function BeneathRoute({ children }: { children: ReactNode }) {
  return (
    <PanelTitlesProvider>
      <PanelParentContext.Provider value={{ page: "routes", screen: "route", resourceId: "r1" }}>
        <PublishRouteTitle />
        {children}
      </PanelParentContext.Provider>
    </PanelTitlesProvider>
  );
}

function PublishRouteTitle() {
  usePublishPanelTitles([panelTitle("routes", "route", "r1", "Route details")]);
  return null;
}

function NestedInspector({
  guarded = false,
  pending = false,
  recoveryMessage,
  title = "Edit route",
}: {
  guarded?: boolean;
  pending?: boolean;
  recoveryMessage?: string;
  title?: string;
}) {
  const [open, setOpen] = useState(false);
  const [backRequests, setBackRequests] = useState(0);
  const [discardRequests, setDiscardRequests] = useState(0);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <BeneathRoute>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open nested inspector
      </button>
      <output aria-label="Back requests">{backRequests}</output>
      <output aria-label="Discard requests">{discardRequests}</output>
      <InspectorShell
        body={<p>Inspector content</p>}
        footer={<button type="button">Save</button>}
        focusKey="edit-route"
        id="nested-inspector-panel"
        navigationDisabledReason={
          pending ? "Saving changes. Inspector navigation is unavailable." : undefined
        }
        onBack={() => setBackRequests((count) => count + 1)}
        onDismissGuard={guarded ? () => setDiscardRequests((count) => count + 1) : undefined}
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        recoveryMessage={recoveryMessage}
        title={title}
      />
    </BeneathRoute>
  );
}

function NavigationInspector() {
  const [open, setOpen] = useState(false);
  const [selection, setSelection] = useState("Alpha");
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open navigation inspector
      </button>
      <button data-panel-navigation="" type="button" onClick={() => setSelection("Bravo")}>
        Select Bravo
      </button>
      <output aria-label="Selection">{selection}</output>
      <InspectorShell
        body={<p>{selection} content</p>}
        focusKey={selection}
        id="navigation-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        title={`${selection} inspector`}
      />
    </>
  );
}

function NestedDialogInspector() {
  const [open, setOpen] = useState(false);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open dialog inspector
      </button>
      <button type="button">Outside dialog action</button>
      <InspectorShell
        body={
          <Dialog>
            <DialogTrigger render={<button type="button" />}>Open nested dialog</DialogTrigger>
            <DialogContent>
              <DialogTitle>Nested dialog</DialogTitle>
            </DialogContent>
          </Dialog>
        }
        id="dialog-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        title="Dialog inspector"
      />
    </>
  );
}

function SuspendedInspector() {
  const [open, setOpen] = useState(false);
  const [modalOpen, setModalOpen] = useState(false);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <InspectorDismissalSuspension active={modalOpen}>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open suspended inspector
      </button>
      <InspectorShell
        body={
          <>
            <p>Inspector content</p>
            <button type="button" onClick={() => setModalOpen(true)}>
              Open modal
            </button>
          </>
        }
        id="suspended-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        title="Suspended inspector"
      />
      {modalOpen ? (
        <div role="dialog" aria-label="Feature modal">
          <button type="button" onClick={() => setModalOpen(false)}>
            Cancel modal
          </button>
        </div>
      ) : null}
    </InspectorDismissalSuspension>
  );
}

function ResponsiveLayerInspector() {
  const [open, setOpen] = useState(false);
  const [deploymentOpen, setDeploymentOpen] = useState(false);
  const openerRef = useRef<HTMLButtonElement>(null);
  const deploymentOpenerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open responsive inspector
      </button>
      <InspectorShell
        body={<p>Inspector content</p>}
        footer={
          <button ref={deploymentOpenerRef} type="button" onClick={() => setDeploymentOpen(true)}>
            Deploy
          </button>
        }
        id="responsive-layer-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        suspendDismissal={deploymentOpen}
        title="Responsive inspector"
      />
      <Dialog open={deploymentOpen} onOpenChange={setDeploymentOpen}>
        <DialogContent finalFocus={deploymentOpenerRef}>
          <DialogTitle>Deploy resource</DialogTitle>
          <DialogClose render={<button type="button" />}>Cancel</DialogClose>
        </DialogContent>
      </Dialog>
    </>
  );
}

function TwoActionInspector() {
  const [open, setOpen] = useState(false);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open two-action inspector
      </button>
      <InspectorShell
        body={<p>Inspector content</p>}
        footer={
          <>
            <button type="button">Connect</button>
            <button type="button">Load</button>
          </>
        }
        id="two-action-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        title="Two-action inspector"
      />
    </>
  );
}

async function waitForAnimationFrame() {
  await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
}

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

afterEach(() => {
  setViewportWidth(1024);
});

describe("InspectorShell", () => {
  it("uses controlled state and restores focus from the close control", async () => {
    const user = userEvent.setup();
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));

    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBeVisible();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Inspector title" })).toHaveFocus(),
    );

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent("close-press");
    expect(screen.getByRole("button", { name: "Open inspector" })).toHaveFocus();
  });

  it("restores focus to the opener when the host unmounts the inspector on close", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);

    function UnmountingHost() {
      const [open, setOpen] = useState(false);
      const openerRef = useRef<HTMLButtonElement>(null);

      return (
        <>
          <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
            Open inspector
          </button>
          {open ? (
            <InspectorShell
              body={<p>Inspector content</p>}
              focusKey="inspector-title"
              id="inspector-panel"
              onOpenChange={() => setOpen(false)}
              open
              openerRef={openerRef}
              title="Inspector title"
            />
          ) : null}
        </>
      );
    }
    render(<UnmountingHost />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    await user.click(screen.getByRole("button", { name: "Close inspector" }));

    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open inspector" })).toHaveFocus();
  });

  it("forgets a turned-down close, so a later unmount while open keeps focus off the opener", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);

    function GuardedHost({ mounted }: { mounted: boolean }) {
      const [confirming, setConfirming] = useState(false);
      const [refusals, setRefusals] = useState(0);
      const openerRef = useRef<HTMLButtonElement>(null);

      return (
        <InspectorDismissalSuspension active={confirming} refusals={refusals}>
          <button ref={openerRef} type="button">
            Opener
          </button>
          {mounted ? (
            <InspectorShell
              body={<input aria-label="Draft" />}
              id="guarded-inspector-panel"
              onOpenChange={() => setConfirming(true)}
              open
              openerRef={openerRef}
              title="Guarded inspector"
            />
          ) : null}
          {confirming ? (
            <div role="dialog" aria-label="Discard changes?">
              <button
                type="button"
                onClick={() => {
                  setConfirming(false);
                  setRefusals((count) => count + 1);
                }}
              >
                Keep editing
              </button>
            </div>
          ) : null}
        </InspectorDismissalSuspension>
      );
    }
    const { rerender } = render(<GuardedHost mounted />);

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    await user.click(screen.getByRole("textbox", { name: "Draft" }));
    expect(screen.getByRole("complementary")).toBeInTheDocument();

    // Unmounted without any press, like a host crossing into the narrow modal.
    rerender(<GuardedHost mounted={false} />);
    // Focus returns on the next frame, so let one pass before reading where it went.
    await act(() => new Promise((resolve) => requestAnimationFrame(resolve)));

    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Opener" })).not.toHaveFocus();
  });

  it("renders a non-modal wide panel at the viewport right below the header", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    const initialBodyOverflow = document.body.style.overflow;
    await user.click(screen.getByRole("button", { name: "Open inspector" }));

    const content = screen.getByRole("complementary", { name: "Inspector title" });
    expect(content).toHaveStyle({
      bottom: "0px",
      left: "auto",
      maxWidth: "calc(100vw - 28px)",
      right: "0px",
      top: "var(--header-height)",
      width: "460px",
    });
    expect(content).toHaveClass("border-l", "p-2", "rounded-l-md", "rounded-r-none", "ring-0");
    expect(content).not.toHaveAttribute("aria-modal");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(document.querySelector("[data-slot='dialog-overlay']")).toBeNull();
    const baseContent = screen
      .getByRole("button", { name: "Outside action 0" })
      .closest("body > *");
    expect(baseContent).not.toHaveAttribute("inert");
    expect(baseContent).not.toHaveAttribute("aria-hidden");
    expect(document.body.style.overflow).toBe(initialBodyOverflow);

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent("escape-key");
    expect(screen.getByRole("button", { name: "Open inspector" })).toHaveFocus();
  });

  it("closes a clean wide inspector once on outside activation and leaves focus with that target", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const outsideAction = screen.getByRole("button", { name: "Outside action 0" });

    await user.click(outsideAction);

    expect(
      screen.queryByRole("complementary", { name: "Inspector title" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent(
      "outside-press",
    );
    expect(screen.getByRole("button", { name: "Outside action 1" })).toHaveFocus();
  });

  it("closes a clean inspector once after a primary touch activation", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    await user.pointer({
      keys: "[TouchA]",
      target: screen.getByRole("button", { name: "Outside action 0" }),
    });

    expect(
      screen.queryByRole("complementary", { name: "Inspector title" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent(
      "outside-press",
    );
    expect(screen.getByRole("status", { name: "Dismiss requests" })).toHaveTextContent("1");
  });

  it("ignores right-click and a pointer drag that starts inside the inspector", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const outsideAction = screen.getByRole("button", { name: "Outside action 0" });

    fireEvent.pointerDown(outsideAction, { button: 2, buttons: 2, pointerType: "mouse" });
    fireEvent.pointerUp(outsideAction, { button: 2, buttons: 0, pointerType: "mouse" });
    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBeVisible();
    expect(screen.getByRole("status", { name: "Dismiss requests" })).toHaveTextContent("0");

    const bodyAction = screen.getByRole("button", { name: "Body action" });
    fireEvent.pointerDown(bodyAction, { button: 0, buttons: 1, pointerType: "mouse" });
    fireEvent.pointerMove(outsideAction, { buttons: 1, pointerType: "mouse" });
    fireEvent.pointerUp(outsideAction, { button: 0, buttons: 0, pointerType: "mouse" });
    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBeVisible();
    expect(screen.getByRole("status", { name: "Dismiss requests" })).toHaveTextContent("0");
  });

  it("keeps the same inspector mounted when Shift+Tab leaves for base content", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const inspector = screen.getByRole("complementary", { name: "Inspector title" });
    const outsideAction = screen.getByRole("button", { name: "Outside action 0" });
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Inspector title" })).toHaveFocus(),
    );
    screen.getByRole("button", { name: "Close inspector" }).focus();
    await user.tab({ shift: true });
    await waitForAnimationFrame();

    expect(outsideAction).toHaveFocus();
    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBe(inspector);
    expect(screen.getByRole("heading", { name: "Inspector title" })).not.toHaveFocus();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent("none");
    expect(screen.getByRole("status", { name: "Dismiss requests" })).toHaveTextContent("0");
  });

  it("keeps the same inspector mounted when Tab leaves for base content", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const inspector = screen.getByRole("complementary", { name: "Inspector title" });
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Inspector title" })).toHaveFocus(),
    );
    screen.getByRole("button", { name: "Load" }).focus();
    await user.tab();
    await user.tab();
    await user.tab();
    await waitForAnimationFrame();

    expect(screen.getByRole("button", { name: "Open inspector" })).toHaveFocus();
    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBe(inspector);
    expect(screen.getByRole("heading", { name: "Inspector title" })).not.toHaveFocus();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent("none");
    expect(screen.getByRole("status", { name: "Dismiss requests" })).toHaveTextContent("0");
  });

  it("keeps the same inspector mounted when focus moves directly into base content", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const inspector = screen.getByRole("complementary", { name: "Inspector title" });
    const outsideAction = screen.getByRole("button", { name: "Outside action 0" });
    await waitFor(() =>
      expect(screen.getByRole("heading", { name: "Inspector title" })).toHaveFocus(),
    );
    outsideAction.focus();
    await waitForAnimationFrame();

    expect(outsideAction).toHaveFocus();
    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBe(inspector);
    expect(screen.getByRole("heading", { name: "Inspector title" })).not.toHaveFocus();
    expect(screen.getByRole("status", { name: "Dismiss reason" })).toHaveTextContent("none");
    expect(screen.getByRole("status", { name: "Dismiss requests" })).toHaveTextContent("0");
  });

  it("dismisses a nested popup before the inspector on outside activation", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<PopupInspector />);

    await user.click(screen.getByRole("button", { name: "Open popup inspector" }));
    const outsideAction = screen.getByRole("button", { name: "Outside popup action 0" });
    await user.click(screen.getByRole("combobox", { name: "Deployment" }));
    await user.click(outsideAction);

    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(screen.getByRole("complementary", { name: "Popup inspector" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Outside popup action 1" })).toHaveFocus();

    await user.click(screen.getByRole("button", { name: "Outside popup action 1" }));
    expect(
      screen.queryByRole("complementary", { name: "Popup inspector" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Outside popup action 2" })).toHaveFocus();
  });

  it("dismisses a tooltip before the inspector on outside activation", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<PopupInspector tooltip />);

    await user.click(screen.getByRole("button", { name: "Open popup inspector" }));
    const trigger = screen.getByRole("button", { name: "Deployment help" });
    trigger.focus();
    expect(await screen.findByText("Choose a deployment")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Outside popup action 0" }));

    await waitFor(() => expect(screen.queryByText("Choose a deployment")).not.toBeInTheDocument());
    expect(screen.getByRole("complementary", { name: "Popup inspector" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Outside popup action 1" }));
    expect(
      screen.queryByRole("complementary", { name: "Popup inspector" }),
    ).not.toBeInTheDocument();
  });

  it("keeps the inspector open when a nested combobox portal option is activated", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<PopupInspector />);

    await user.click(screen.getByRole("button", { name: "Open popup inspector" }));
    const inspector = screen.getByRole("complementary", { name: "Popup inspector" });
    await user.click(screen.getByRole("combobox", { name: "Deployment" }));
    expect(inspector).not.toHaveAttribute("aria-hidden");
    expect(inspector).not.toHaveAttribute("inert");
    await user.click(screen.getByRole("option", { name: /Deployment Alpha deployment-alpha/i }));

    expect(screen.getByRole("status", { name: "Selected deployment" })).toHaveTextContent(
      "deployment-alpha",
    );
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(screen.getByRole("complementary", { name: "Popup inspector" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Outside popup action 0" }));
    expect(
      screen.queryByRole("complementary", { name: "Popup inspector" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Outside popup action 1" })).toHaveFocus();
  });

  it("lets a nested Base UI dialog close before its inspector", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<NestedDialogInspector />);

    await user.click(screen.getByRole("button", { name: "Open dialog inspector" }));
    await user.click(screen.getByRole("button", { name: "Open nested dialog" }));
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "Nested dialog" })).not.toBeInTheDocument();
    expect(screen.getByRole("complementary", { name: "Dialog inspector" })).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Outside dialog action" }));
    expect(
      screen.queryByRole("complementary", { name: "Dialog inspector" }),
    ).not.toBeInTheDocument();
  });

  it("keeps an active child dialog isolated when the inspector crosses into compact mode", async () => {
    const user = userEvent.setup();
    setViewportWidth(721);
    render(<ResponsiveLayerInspector />);

    await user.click(screen.getByRole("button", { name: "Open responsive inspector" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    setViewportWidth(720);
    await waitForAnimationFrame();

    const inspector = screen.getByTestId("inspector-content");
    expect(inspector).toHaveStyle({ zIndex: "calc(var(--layer-modal) - 1)" });
    expect(inspector).toHaveAttribute("aria-hidden", "true");
    expect(inspector).toHaveAttribute("inert");
    expect(screen.getAllByRole("dialog")).toEqual([
      screen.getByRole("dialog", { name: "Deploy resource" }),
    ]);

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "Deploy resource" })).not.toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Responsive inspector" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Deploy" })).toHaveFocus();
  });

  it("keeps an active child dialog isolated when the inspector crosses into wide mode", async () => {
    const user = userEvent.setup();
    setViewportWidth(720);
    render(<ResponsiveLayerInspector />);

    await user.click(screen.getByRole("button", { name: "Open responsive inspector" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    setViewportWidth(721);
    await waitForAnimationFrame();

    const inspector = screen.getByTestId("inspector-content");
    expect(inspector).toHaveAttribute("aria-hidden", "true");
    expect(inspector).toHaveAttribute("inert");
    expect(screen.getAllByRole("dialog")).toEqual([
      screen.getByRole("dialog", { name: "Deploy resource" }),
    ]);

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog", { name: "Deploy resource" })).not.toBeInTheDocument();
    expect(screen.getByRole("complementary", { name: "Responsive inspector" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Deploy" })).toHaveFocus();
  });

  it("gives an active feature modal precedence over inspector dismissal", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<SuspendedInspector />);

    await user.click(screen.getByRole("button", { name: "Open suspended inspector" }));
    await user.click(screen.getByRole("button", { name: "Open modal" }));
    await user.keyboard("{Escape}");

    expect(screen.getByRole("dialog", { name: "Feature modal" })).toBeVisible();
    expect(screen.getByTestId("inspector-content")).toHaveAttribute("aria-hidden", "true");

    await user.click(screen.getByRole("button", { name: "Cancel modal" }));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("delegates marked lateral navigation without issuing a competing Close", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    render(<NavigationInspector />);

    await user.click(screen.getByRole("button", { name: "Open navigation inspector" }));
    await user.click(screen.getByRole("button", { name: "Select Bravo" }));

    expect(screen.getByRole("status", { name: "Selection" })).toHaveTextContent("Bravo");
    expect(screen.getByRole("complementary", { name: "Bravo inspector" })).toBeVisible();
    expect(screen.getByRole("heading", { name: "Bravo inspector" })).toHaveFocus();
  });

  it("blocks dirty and pending outside dismissal requests", async () => {
    const user = userEvent.setup();
    setViewportWidth(1440);
    const guarded = render(<NestedInspector guarded />);

    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));
    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));
    expect(screen.getByRole("status", { name: "Discard requests" })).toHaveTextContent("1");
    expect(screen.getByRole("complementary", { name: "Edit route" })).toBeVisible();

    guarded.unmount();
    render(<NestedInspector pending />);
    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));
    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));
    expect(screen.getByRole("complementary", { name: "Edit route" })).toBeVisible();
    expect(screen.getByRole("button", { name: "Back to Route details" })).toBeDisabled();
  });

  it("uses an inline arrow-only Back/title/Close header with the parent-labelled accessible name", async () => {
    const user = userEvent.setup();
    render(
      <NestedInspector title="A route title that wraps across the available inspector header" />,
    );

    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));

    const panel = screen.getByRole("complementary", {
      name: "A route title that wraps across the available inspector header",
    });
    const header = within(panel).getByRole("heading").parentElement!;
    const back = within(panel).getByRole("button", { name: "Back to Route details" });
    const title = within(panel).getByRole("heading");
    const close = within(panel).getByRole("button", { name: "Close inspector" });

    expect(header).toHaveClass("grid", "grid-cols-[22px_minmax(0,1fr)_44px]", "py-2");
    expect(header).not.toHaveClass("py-4");
    expect(Array.from(header.children).indexOf(back)).toBeLessThan(
      Array.from(header.children).indexOf(title),
    );
    expect(Array.from(header.children).indexOf(title)).toBeLessThan(
      Array.from(header.children).indexOf(close),
    );
    expect(back).toHaveTextContent("");
    expect(back).toHaveClass("-ml-3.5", "size-11", "justify-center", "self-center");
    expect(close).toHaveClass("size-11", "self-center");
    expect(close).not.toHaveClass("absolute");
    expect(title).toHaveClass("text-[17px]", "leading-6", "font-semibold");

    await waitFor(() => expect(title).toHaveFocus());
    fireEvent.keyDown(document.body, { key: "Tab" });
    back.focus();
    expect(back).toHaveFocus();
    expect(back).not.toHaveAttribute("title");
    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });

  it("renders a header status beside the title without joining the panel name", () => {
    const openerRef = { current: null };
    render(
      <InspectorShell
        body={<p>Body</p>}
        id="status-inspector"
        onOpenChange={() => undefined}
        open
        openerRef={openerRef}
        status={<span data-testid="header-status">Degraded</span>}
        title="chat"
      />,
    );

    const panel = screen.getByRole("complementary", { name: "chat" });
    const heading = within(panel).getByRole("heading", { name: "chat" });
    const status = within(panel).getByTestId("header-status");
    expect(heading).not.toContainElement(status);
    expect(heading.parentElement).toBe(status.parentElement);
  });

  it("uses column flex layout and self-centers every nested header item when narrow", async () => {
    const user = userEvent.setup();
    setViewportWidth(500);
    render(
      <NestedInspector title="A route title that wraps across the available inspector header" />,
    );

    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));

    const content = screen.getByTestId("inspector-content");
    const header = content.querySelector("[data-slot='dialog-header']")!;
    const back = screen.getByRole("button", { name: "Back to Route details" });
    const title = header.querySelector("h2")!;
    const close = screen.getByRole("button", { name: "Close inspector" });

    expect(content).toHaveClass("flex", "flex-col", "gap-0");
    expect(title).toHaveClass("self-center");
    expect(back).toHaveClass("self-center");
    expect(close).toHaveClass("self-center");
  });

  it("renders the parent-labelled header and routes Back separately from Close", async () => {
    const user = userEvent.setup();
    render(<NestedInspector />);

    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));

    const panel = screen.getByRole("complementary", { name: "Edit route" });
    expect(within(panel).getByRole("button", { name: "Back to Route details" })).toBeVisible();
    await waitFor(() =>
      expect(within(panel).getByRole("heading", { name: "Edit route" })).toHaveFocus(),
    );
    expect(within(panel).getByRole("button", { name: "Close inspector" })).toBeVisible();
    expect(
      within(screen.getByTestId("inspector-footer")).queryByRole("button", {
        name: /back|close|cancel/i,
      }),
    ).toBeNull();

    const back = screen.getByRole("button", { name: "Back to Route details" });
    const close = screen.getByRole("button", { name: "Close inspector" });
    expect(back).toHaveClass("cursor-pointer");
    expect(close).toHaveClass("cursor-pointer");

    await user.click(back);
    expect(screen.getByRole("status", { name: "Back requests" })).toHaveTextContent("1");

    await user.click(close);
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("uses a dirty dismissal guard and disables stack navigation while pending", async () => {
    const user = userEvent.setup();
    render(<NestedInspector guarded />);

    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));
    await user.keyboard("{Escape}");
    expect(screen.getByRole("status", { name: "Discard requests" })).toHaveTextContent("1");
    expect(screen.getByRole("complementary", { name: "Edit route" })).toBeVisible();

    render(<NestedInspector pending />);
    const openers = screen.getAllByRole("button", { name: "Open nested inspector" });
    await user.click(openers.at(-1)!);
    const disabledBack = screen.getAllByRole("button", { name: "Back to Route details" }).at(-1)!;
    expect(disabledBack).toBeDisabled();
    expect(disabledBack).not.toHaveClass("cursor-pointer");
    expect(screen.getAllByRole("button", { name: "Close inspector" }).at(-1)).not.toHaveClass(
      "cursor-pointer",
    );
    expect(disabledBack).toHaveAccessibleDescription(
      "Saving changes. Inspector navigation is unavailable.",
    );
  });

  it("keeps inspector replacement immediate while reserving motion for a true open", async () => {
    const user = userEvent.setup();
    render(<NavigationInspector />);

    await user.click(screen.getByRole("button", { name: "Open navigation inspector" }));
    const panel = screen.getByRole("complementary", { name: "Alpha inspector" });
    await user.click(screen.getByRole("button", { name: "Select Bravo" }));

    expect(screen.getByRole("complementary", { name: "Bravo inspector" })).toBe(panel);
    expect(panel.className).not.toMatch(/animate-(in|out)|fade|slide|zoom|duration|transition/);
  });

  it("uses only full-width right-edge opening motion and leaves Close and ordinary dialogs unchanged", async () => {
    const user = userEvent.setup();
    render(
      <PanelPresentationContext.Provider value="enter">
        <ControlledInspector />
      </PanelPresentationContext.Provider>,
    );

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const inspector = screen.getByRole("complementary", { name: "Inspector title" });
    expect(inspector).not.toHaveClass("translate-x-full");
    expect(inspector).toHaveClass(
      "transition-transform",
      "duration-160",
      "ease-out",
      "data-[starting-style]:translate-x-full",
      "motion-reduce:transition-none",
    );
    expect(inspector).not.toHaveClass("data-[ending-style]:translate-x-full");
    expect(inspector.className).not.toMatch(/fade|zoom|slide-in-from-right-4/);

    await user.click(screen.getByRole("button", { name: "Close inspector" }));
    expect(
      screen.queryByRole("complementary", { name: "Inspector title" }),
    ).not.toBeInTheDocument();

    render(
      <Dialog open>
        <DialogContent>
          <DialogTitle>Ordinary dialog</DialogTitle>
        </DialogContent>
      </Dialog>,
    );
    expect(screen.getByRole("dialog", { name: "Ordinary dialog" }).className).toMatch(
      /fade|zoom|animate/,
    );
  });

  it("announces canonical recovery once without transition motion", async () => {
    const user = userEvent.setup();
    render(
      <NestedInspector recoveryMessage="That route is no longer available. Returned to Routes." />,
    );

    await user.click(screen.getByRole("button", { name: "Open nested inspector" }));

    expect(
      screen.getByText("That route is no longer available. Returned to Routes."),
    ).toHaveAttribute("role", "status");
    expect(screen.getByTestId("inspector-content").className).not.toMatch(
      /transition|duration|translate|fade|zoom|slide/,
    );
  });

  it("uses modal and non-modal presentation on either side of the 720px breakpoint", async () => {
    const user = userEvent.setup();
    setViewportWidth(721);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    expect(screen.getByRole("complementary", { name: "Inspector title" })).toBeVisible();
    await user.keyboard("{Escape}");

    setViewportWidth(720);
    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    expect(screen.getByRole("dialog", { name: "Inspector title" })).toBeVisible();
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("exposes one accessible title in the narrow inspector", async () => {
    const user = userEvent.setup();
    setViewportWidth(500);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));

    const dialog = screen.getByRole("dialog", { name: "Inspector title" });
    expect(within(dialog).getAllByRole("heading", { name: "Inspector title" })).toHaveLength(1);
  });

  it("moves focus into the narrow modal before hiding the page from assistive tech", async () => {
    const user = userEvent.setup();
    setViewportWidth(500);
    const { container } = render(<ControlledInspector />);
    const hiddenWhileFocused: string[] = [];
    // Records which focused element each aria-hidden call would hide at the moment it is applied.
    const spy = vi.spyOn(HTMLElement.prototype, "setAttribute").mockImplementation(function (
      this: HTMLElement,
      name: string,
      value: string,
    ) {
      if (name === "aria-hidden" && value === "true" && this.contains(document.activeElement))
        hiddenWhileFocused.push(document.activeElement?.textContent ?? "");
      Element.prototype.setAttribute.call(this, name, value);
    });

    try {
      await user.click(screen.getByRole("button", { name: "Open inspector" }));
      await waitFor(() =>
        expect(screen.getByRole("heading", { name: "Inspector title" })).toHaveFocus(),
      );
    } finally {
      spy.mockRestore();
    }

    expect(container).toHaveAttribute("aria-hidden", "true");
    expect(hiddenWhileFocused).toEqual([]);
  });

  it("keeps keyboard focus inside the narrow modal", async () => {
    const user = userEvent.setup();
    setViewportWidth(500);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    screen.getByRole("button", { name: "Close inspector" }).focus();
    await user.tab();
    expect(screen.getByRole("button", { name: "Body action" })).toHaveFocus();

    await user.tab();
    expect(screen.getByRole("button", { name: "Load" })).toHaveFocus();

    await user.tab();
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Close inspector" })).toHaveFocus();
    });
  });

  it("renders a full-screen narrow modal with a backdrop", async () => {
    const user = userEvent.setup();
    setViewportWidth(500);
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));

    const content = screen.getByTestId("inspector-content");
    expect(content).toHaveClass("inset-0", "max-w-none", "sm:max-w-none", "p-0", "rounded-none");
    expect(content).not.toHaveClass("p-2", "rounded-l-md", "rounded-r-none");
    expect(screen.getByRole("dialog", { name: "Inspector title" })).toHaveAttribute(
      "aria-modal",
      "true",
    );
    expect(document.querySelector("[data-slot='dialog-overlay']")).not.toBeNull();

    expect(document.body.style.overflow).toBe("hidden");

    await user.click(document.querySelector("[data-slot='dialog-overlay']")!);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Open inspector" })).toHaveFocus();
  });

  it("keeps a flat sticky footer on the body surface outside the scrolling body", async () => {
    const user = userEvent.setup();
    render(<ControlledInspector />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));

    const body = screen.getByTestId("inspector-body");
    const footer = screen.getByTestId("inspector-footer");

    expect(body).toHaveClass("overflow-y-auto");
    expect(footer).toHaveAttribute("data-slot", "inspector-footer");
    expect(footer).toHaveClass(
      "sticky",
      "bottom-0",
      "shrink-0",
      "border-t",
      "border-input",
      "bg-card",
    );
    expect(footer.className).not.toMatch(/shadow|elevation/);
    expect(footer.parentElement).toBe(body.parentElement);
    expect(body.nextElementSibling).toBe(footer);
    expect(footer).toContainElement(screen.getByRole("button", { name: "Load" }));
  });

  it("lays footer actions out at natural width in one right-aligned row", async () => {
    const user = userEvent.setup();
    setViewportWidth(390);
    render(<TwoActionInspector />);

    await user.click(screen.getByRole("button", { name: "Open two-action inspector" }));

    const footer = screen.getByTestId("inspector-footer");
    expect(footer).toHaveClass("flex-row", "flex-wrap", "justify-end");
    expect(footer).not.toHaveClass("flex-col-reverse");
    expect(footer.className).not.toMatch(/flex-1|w-full/);
    expect(within(footer).getByRole("button", { name: "Connect" })).toBeVisible();
    expect(within(footer).getByRole("button", { name: "Load" })).toBeVisible();
  });
});
