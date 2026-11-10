import { render, screen, waitFor, within } from "@testing-library/react";
import { ArrowUp, Ban, Code, ExternalLink, MessageSquare, Pencil, Trash2 } from "lucide-react";
import { useRef, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  type InspectorAction,
  InspectorActions,
} from "@/components/inspector-actions/inspector-actions";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { InspectorShell } from "@/components/ui/inspector-shell";
import { chooseMoreAction, moreActionNames } from "@/test/inspector-actions";
import { setupUser } from "@/test/user";

const edit: InspectorAction = { icon: Pencil, label: "Edit", onSelect: () => {} };
const disable: InspectorAction = { icon: Ban, label: "Disable", onSelect: () => {} };
const remove: InspectorAction = {
  destructive: true,
  icon: Trash2,
  label: "Delete",
  onSelect: () => {},
};

function ActionsInspector({
  more,
  primary,
  secondary,
}: {
  more?: InspectorAction[];
  primary?: InspectorAction;
  secondary?: InspectorAction[];
}) {
  const [open, setOpen] = useState(false);
  const [outsideActions, setOutsideActions] = useState(0);
  const [sorted, setSorted] = useState(false);
  const openerRef = useRef<HTMLButtonElement>(null);

  return (
    <>
      <button ref={openerRef} type="button" onClick={() => setOpen(true)}>
        Open inspector
      </button>
      <button type="button" onClick={() => setOutsideActions((count) => count + 1)}>
        Outside action {outsideActions}
      </button>
      {/* A data-table column menu: page content marked as panel navigation. */}
      <DropdownMenu>
        <DropdownMenuTrigger data-panel-navigation="">Name column options</DropdownMenuTrigger>
        <DropdownMenuContent data-panel-navigation="">
          <DropdownMenuItem onClick={() => setSorted(true)}>
            <ArrowUp aria-hidden="true" />
            Sort ascending
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <output aria-label="Sorted">{sorted ? "yes" : "no"}</output>
      <InspectorShell
        body={<p>Inspector content</p>}
        footer={<InspectorActions more={more} primary={primary} secondary={secondary} />}
        id="actions-inspector-panel"
        onOpenChange={setOpen}
        open={open}
        openerRef={openerRef}
        title="Route"
      />
    </>
  );
}

function setViewportWidth(width: number) {
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
  window.dispatchEvent(new Event("resize"));
}

afterEach(() => {
  setViewportWidth(1024);
});

describe("InspectorActions", () => {
  it("renders the primary, secondary and menu actions in that order", async () => {
    const user = setupUser();
    render(
      <InspectorActions
        more={[remove, edit, disable]}
        primary={{ icon: MessageSquare, label: "Chat", onSelect: () => {} }}
        secondary={[{ icon: Code, label: "Connect", onSelect: () => {} }]}
      />,
    );

    expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual([
      "Chat",
      "Connect",
      "",
    ]);
    expect(screen.getByRole("button", { name: "Chat" })).toHaveAttribute(
      "class",
      expect.stringContaining("bg-primary"),
    );
    expect(screen.getByRole("button", { name: "Connect" })).toHaveAttribute(
      "class",
      expect.stringContaining("border-border"),
    );

    expect(await moreActionNames(user)).toEqual(["Edit", "Disable", "Delete…"]);
    const menu = screen.getByRole("menu");
    const separator = within(menu).getByRole("separator");
    expect(separator.nextElementSibling).toBe(
      within(menu).getByRole("menuitem", { name: "Delete…" }),
    );
    expect(within(menu).getByRole("menuitem", { name: "Delete…" })).toHaveAttribute(
      "data-variant",
      "destructive",
    );
  });

  it("omits the separator when the menu holds no destructive action", async () => {
    const user = setupUser();
    render(<InspectorActions more={[edit, disable]} />);

    expect(await moreActionNames(user)).toEqual(["Edit", "Disable"]);
    expect(within(screen.getByRole("menu")).queryByRole("separator")).not.toBeInTheDocument();
  });

  it("renders a lone menu action as a button, keeping its destructive style", () => {
    render(<InspectorActions more={[remove]} secondary={[edit]} />);

    expect(screen.queryByRole("button", { name: "More actions" })).not.toBeInTheDocument();
    expect(screen.getAllByRole("button").map((button) => button.textContent)).toEqual([
      "Edit",
      "Delete…",
    ]);
    expect(screen.getByRole("button", { name: "Delete…" })).toHaveAttribute(
      "class",
      expect.stringContaining("text-destructive"),
    );
  });

  it("runs the handler of the chosen menu item and closes the menu", async () => {
    const user = setupUser();
    const onEdit = vi.fn();
    render(<InspectorActions more={[{ ...edit, onSelect: onEdit }, remove]} />);

    await chooseMoreAction(user, "Edit");

    expect(onEdit).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
  });

  it("shows the pending label on a pending button", () => {
    render(
      <InspectorActions
        primary={{ ...edit, label: "Resume", pending: true, pendingLabel: "Resuming…" }}
      />,
    );

    expect(screen.getByRole("button", { name: "Resuming…" })).toBeDisabled();
  });

  it("disables the menu trigger with a progress icon while a menu action is pending", async () => {
    const user = setupUser();
    const { container, rerender } = render(<InspectorActions more={[edit, remove]} />);
    const trigger = screen.getByRole("button", { name: "More actions" });

    expect(trigger).toBeEnabled();
    expect(trigger).not.toHaveAttribute("aria-busy");
    expect(container.querySelector(".animate-spin")).toBeNull();

    rerender(
      <InspectorActions more={[edit, { ...remove, pending: true, pendingLabel: "Deleting…" }]} />,
    );

    expect(trigger).toBeDisabled();
    expect(trigger).toHaveAttribute("aria-busy", "true");
    expect(container.querySelector(".animate-spin")).not.toBeNull();
    await user.click(trigger);
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
  });

  it("shows disabled reasons in a popup for buttons and as text for menu items", async () => {
    const user = setupUser();
    const onSelect = vi.fn();
    render(
      <InspectorActions
        more={[{ ...edit, disabled: true, disabledReason: "The route is being deleted." }, remove]}
        primary={{
          disabled: true,
          disabledReason: "No available route serves this deployment.",
          icon: MessageSquare,
          label: "Chat",
          onSelect,
        }}
      />,
    );

    const chat = screen.getByRole("button", { name: "Chat" });
    expect(chat).toHaveAttribute("aria-disabled", "true");
    expect(chat).toHaveAccessibleDescription("No available route serves this deployment.");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await user.click(chat);
    expect(await screen.findByRole("dialog")).toHaveTextContent(
      "No available route serves this deployment.",
    );
    expect(onSelect).not.toHaveBeenCalled();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    await user.click(screen.getByRole("button", { name: "More actions" }));
    const item = await screen.findByRole("menuitem", { name: "Edit" });
    expect(item).toHaveAttribute("aria-disabled", "true");
    expect(item).toHaveAccessibleDescription("The route is being deleted.");
    expect(within(item).getByText("The route is being deleted.")).toBeVisible();
  });

  it("renders link actions as external links", () => {
    render(
      <InspectorActions
        primary={{ href: "https://webui.example.test/", icon: ExternalLink, label: "Open" }}
      />,
    );

    const link = screen.getByRole("link", { name: "Open" });
    expect(link).toHaveAttribute("href", "https://webui.example.test/");
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noreferrer");
  });

  it("points an action's focus target at the menu trigger while it sits in the menu", () => {
    const focusRef: { current: HTMLElement | null } = { current: null };
    const { rerender } = render(<InspectorActions more={[edit, { ...remove, focusRef }]} />);

    expect(focusRef.current).toBe(screen.getByRole("button", { name: "More actions" }));

    rerender(<InspectorActions more={[{ ...remove, focusRef }]} />);

    expect(focusRef.current).toBe(screen.getByRole("button", { name: "Delete…" }));
  });

  it("opens and closes the menu from the keyboard, returning focus to the trigger", async () => {
    const user = setupUser();
    render(<InspectorActions more={[edit, remove]} />);
    const trigger = screen.getByRole("button", { name: "More actions" });

    trigger.focus();
    await user.keyboard("{Enter}");
    expect(await screen.findByRole("menu")).toBeVisible();

    await user.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
    expect(trigger).toHaveFocus();
  });

  it("keeps the inspector open while its menu is used", async () => {
    const user = setupUser();
    const onEdit = vi.fn();
    setViewportWidth(1440);
    render(<ActionsInspector more={[{ ...edit, onSelect: onEdit }, remove]} />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    const inspector = screen.getByRole("complementary", { name: "Route" });
    await chooseMoreAction(user, "Edit", inspector);

    expect(onEdit).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("complementary", { name: "Route" })).toBeVisible();

    await user.click(within(inspector).getByRole("button", { name: "More actions" }));
    await screen.findByRole("menu");
    await user.click(screen.getByRole("button", { name: /^Outside action/ }));

    await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
    expect(screen.getByRole("complementary", { name: "Route" })).toBeVisible();
  });

  it("keeps the inspector open when a data-table menu is used", async () => {
    const user = setupUser();
    setViewportWidth(1440);
    render(<ActionsInspector more={[edit, remove]} />);

    await user.click(screen.getByRole("button", { name: "Open inspector" }));
    await user.click(screen.getByRole("button", { name: "Name column options" }));
    await user.click(await screen.findByRole("menuitem", { name: "Sort ascending" }));

    expect(screen.getByRole("status", { name: "Sorted" })).toHaveTextContent("yes");
    expect(screen.getByRole("complementary", { name: "Route" })).toBeVisible();
  });
});
