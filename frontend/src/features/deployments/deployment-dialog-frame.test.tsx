import { render, screen, within } from "@testing-library/react";
import { createRef } from "react";
import { describe, expect, it, vi } from "vitest";

import { Button } from "@/components/ui/button";
import { expectResourceMarker } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

import { DeploymentDialogFrame } from "./deployment-dialog-frame";

describe("DeploymentDialogFrame", () => {
  it("renders the shared content-height geometry and two-level resource heading", () => {
    render(
      <DeploymentDialogFrame
        actions={<Button type="submit">Deploy</Button>}
        footerStart={<Button type="button">Customize</Button>}
        onOpenChange={vi.fn()}
        onSubmit={vi.fn()}
        open
        resourceColorKey="model-1"
        resourceName="Test Model"
        title="Deploy model"
      >
        <p>Deployment content</p>
      </DeploymentDialogFrame>,
    );

    const dialog = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
    const heading = within(dialog).getByRole("heading", {
      level: 2,
      name: "Test Model — Deploy model",
    });
    const body = within(dialog).getByTestId("deployment-dialog-body");
    const footer = within(dialog).getByTestId("deployment-dialog-footer");
    const start = within(footer).getByTestId("deployment-dialog-footer-start");
    const actions = within(footer).getByTestId("deployment-dialog-actions");

    expectResourceMarker(heading, "model-1");
    expect(within(heading).getByText("Test Model")).toHaveClass("uppercase");
    expect(within(heading).getByText("Deploy model")).toHaveClass("text-xl");
    expect(dialog).toHaveClass(
      // Full screen below sm
      "inset-0",
      "h-dvh",
      "max-h-none",
      "w-full",
      "max-w-none",
      "rounded-none",
      // Centered card from sm
      "sm:inset-auto",
      "sm:top-1/2",
      "sm:left-1/2",
      "sm:h-[min(44rem,calc(100dvh-4rem))]",
      "sm:max-h-[calc(100dvh-2rem)]",
      "sm:w-[calc(100vw-2rem)]",
      "sm:max-w-[48rem]",
      "sm:-translate-x-1/2",
      "sm:-translate-y-1/2",
      "sm:rounded-xl",
      "grid-rows-[auto_minmax(0,1fr)_auto]",
      "overflow-hidden",
    );
    expect(body).toHaveClass("min-h-0", "overflow-x-hidden", "overflow-y-auto", "sm:px-6");
    expect(footer).toHaveClass("justify-between", "sm:px-6");
    expect(actions).toHaveClass("ml-auto", "justify-end");
    expect(
      within(start)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Customize"]);
    expect(
      within(actions)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Cancel", "Deploy"]);
    expect(start.compareDocumentPosition(actions) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(body.compareDocumentPosition(footer) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("submits through the frame and protects every dismissal path while pending", async () => {
    const user = setupUser();
    const onOpenChange = vi.fn();
    const onSubmit = vi.fn();
    const view = render(
      <DeploymentDialogFrame
        actions={<Button type="submit">Deploy</Button>}
        onOpenChange={onOpenChange}
        onSubmit={onSubmit}
        open
        openerRef={createRef<HTMLElement>()}
        resourceColorKey="model-1"
        resourceName="Test Model"
        title="Deploy model"
      >
        <input aria-label="Name" />
      </DeploymentDialogFrame>,
    );

    await user.type(screen.getByRole("textbox", { name: "Name" }), "deployment{Enter}");
    expect(onSubmit).toHaveBeenCalledOnce();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onOpenChange).toHaveBeenCalledWith(false);
    onOpenChange.mockClear();

    view.rerender(
      <DeploymentDialogFrame
        actions={
          <Button disabled type="submit">
            Deploying…
          </Button>
        }
        onOpenChange={onOpenChange}
        onSubmit={onSubmit}
        open
        openerRef={createRef<HTMLElement>()}
        pending
        resourceColorKey="model-1"
        resourceName="Test Model"
        title="Deploy model"
      >
        <input aria-label="Name" />
      </DeploymentDialogFrame>,
    );

    expect(screen.queryByTestId("deployment-dialog-footer-start")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Close" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
    await user.keyboard("{Escape}");
    expect(onOpenChange).not.toHaveBeenCalled();
  });
});
