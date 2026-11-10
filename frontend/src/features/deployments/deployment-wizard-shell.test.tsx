import { render, screen, waitFor, within } from "@testing-library/react";
import { createRef, useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { Button } from "@/components/ui/button";
import { setupUser } from "@/test/user";

import { DeploymentFormSection } from "./deployment-form-layout";
import { DeploymentWizardShell } from "./deployment-wizard-shell";
import { useWizardSteps } from "./use-wizard-steps";

describe("DeploymentWizardShell", () => {
  it("renders a single-step page without progress, with the left slot and Cancel in the right group", async () => {
    const user = setupUser();
    const onCustomize = vi.fn();
    const onOpenChange = vi.fn();
    const onPrimary = vi.fn();

    render(
      <DeploymentWizardShell
        currentStep="Profile"
        footerStart={
          <Button onClick={onCustomize} type="button" variant="outline">
            Customize
          </Button>
        }
        onOpenChange={onOpenChange}
        onPrimary={onPrimary}
        onStepChange={vi.fn()}
        open
        primaryLabel="Deploy"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Profile"]}
        title="Deploy model"
      >
        <p>Profile cards</p>
      </DeploymentWizardShell>,
    );

    const dialog = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
    const footer = within(dialog).getByTestId("deployment-dialog-footer");
    const start = within(footer).getByTestId("deployment-dialog-footer-start");
    const actions = within(footer).getByTestId("deployment-dialog-actions");
    expect(screen.getByText("Profile cards")).toBeVisible();
    expect(dialog).not.toHaveAttribute("aria-describedby");
    expect(screen.getByRole("heading", { name: "Test Model — Deploy model" })).toBeVisible();
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
    expect(
      within(dialog).queryByRole("list", { name: "Deployment progress" }),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Back" })).not.toBeInTheDocument();
    expect(within(dialog).queryByRole("status")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Customize" }));
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await user.click(screen.getByRole("button", { name: "Deploy" }));

    expect(onCustomize).toHaveBeenCalledOnce();
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(onPrimary).toHaveBeenCalledOnce();
  });

  it("renders step headers by visit state and delegates header and Back navigation", async () => {
    const user = setupUser();
    const onStepChange = vi.fn();
    const onPrimary = vi.fn();

    render(
      <DeploymentWizardShell
        currentStep="Deployment"
        onOpenChange={vi.fn()}
        onPrimary={onPrimary}
        onStepChange={onStepChange}
        open
        primaryLabel="Continue"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Profile", "Deployment", "Runtime", "Review"]}
        title="Deploy model"
        visitedSteps={new Set(["Profile", "Deployment", "Runtime"])}
      >
        <p>Deployment details</p>
      </DeploymentWizardShell>,
    );

    const dialog = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
    const actions = within(dialog).getByTestId("deployment-dialog-actions");
    const progress = screen.getByRole("list", { name: "Deployment progress" });
    const headers = within(progress).getAllByRole("button");
    expect(headers.map((header) => header.textContent)).toEqual([
      "1. Profile",
      "2. DeploymentStep 2 of 4 — Deployment",
      "3. Runtime",
      "4. Review",
    ]);
    expect(headers.map((header) => header.getAttribute("aria-current"))).toEqual([
      null,
      "step",
      null,
      null,
    ]);
    expect(headers.map((header) => (header as HTMLButtonElement).disabled)).toEqual([
      false,
      false,
      false,
      true,
    ]);
    expect(
      within(actions)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Cancel", "Back", "Continue"]);

    await user.click(within(progress).getByRole("button", { name: "1. Profile" }));
    await user.click(within(progress).getByRole("button", { name: "3. Runtime" }));
    await user.click(within(progress).getByRole("button", { current: "step" }));
    await user.click(within(progress).getByRole("button", { name: "4. Review" }));
    await user.click(screen.getByRole("button", { name: "Back" }));
    await user.click(screen.getByRole("button", { name: "Continue" }));

    expect(onStepChange.mock.calls).toEqual([["Profile"], ["Runtime"], ["Profile"]]);
    expect(onPrimary).toHaveBeenCalledOnce();
  });

  it("shows only the current step header below the small breakpoint", () => {
    render(
      <DeploymentWizardShell
        currentStep="Deployment"
        onOpenChange={vi.fn()}
        onPrimary={vi.fn()}
        onStepChange={vi.fn()}
        open
        primaryLabel="Continue"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Profile", "Deployment", "Review"]}
        title="Deploy model"
        visitedSteps={new Set(["Profile", "Deployment"])}
      >
        <p>Deployment details</p>
      </DeploymentWizardShell>,
    );

    const progress = screen.getByRole("list", { name: "Deployment progress" });
    const items = within(progress).getAllByRole("listitem");
    expect(items.map((item) => item.classList.contains("hidden"))).toEqual([true, false, true]);
    expect(items.map((item) => item.classList.contains("sm:block"))).toEqual([true, true, true]);
    expect(screen.getByText("Step 2 of 3 — Deployment")).toHaveClass("sm:hidden");
    expect(within(items[1]).getByText("2. Deployment")).toHaveClass("hidden", "sm:inline");
    expect(within(progress).queryByText(/^Step [13] of 3/)).not.toBeInTheDocument();
  });

  it("keeps the current step and visited steps by name when the step list changes", async () => {
    const user = setupUser();

    function Harness() {
      const [advanced, setAdvanced] = useState(false);
      const steps = advanced
        ? ["Target", "Profile", "Runtime", "Routing", "Deploy"]
        : ["Profile", "Deploy"];
      const wizard = useWizardSteps(steps, (missing) =>
        missing === "Target" ? "Profile" : "Deploy",
      );
      const index = steps.indexOf(wizard.current);
      return (
        <DeploymentWizardShell
          currentStep={wizard.current}
          footerStart={
            <label>
              <input
                checked={advanced}
                onChange={(event) => setAdvanced(event.target.checked)}
                type="checkbox"
              />
              Advanced
            </label>
          }
          onOpenChange={vi.fn()}
          onPrimary={() => wizard.goTo(steps[index + 1] ?? wizard.current)}
          onStepChange={wizard.goTo}
          open
          primaryLabel="Next"
          resourceColorKey="model-1"
          resourceName="Test Model"
          steps={steps}
          title="Deploy model"
          visitedSteps={wizard.visited}
        >
          <p>{wizard.current} pane</p>
        </DeploymentWizardShell>
      );
    }

    render(<Harness />);
    const headerStates = () =>
      within(screen.getByRole("list", { name: "Deployment progress" }))
        .getAllByRole("button")
        .map((header) => {
          const name = header.querySelector("[data-step-label]")?.textContent;
          if (header.getAttribute("aria-current") === "step") return `${name}:current`;
          return `${name}:${(header as HTMLButtonElement).disabled ? "disabled" : "visited"}`;
        });

    expect(headerStates()).toEqual(["1. Profile:current", "2. Deploy:disabled"]);

    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Profile pane")).toBeVisible();
    // Earlier steps are reachable even when never visited; later ones only once visited.
    expect(headerStates()).toEqual([
      "1. Target:visited",
      "2. Profile:current",
      "3. Runtime:disabled",
      "4. Routing:disabled",
      "5. Deploy:disabled",
    ]);

    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Runtime pane")).toBeVisible();
    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Deploy pane")).toBeVisible();
    expect(headerStates()).toEqual(["1. Profile:visited", "2. Deploy:current"]);

    await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
    expect(screen.getByText("Deploy pane")).toBeVisible();
    expect(headerStates()).toEqual([
      "1. Target:visited",
      "2. Profile:visited",
      "3. Runtime:visited",
      "4. Routing:visited",
      "5. Deploy:current",
    ]);

    await user.click(screen.getByRole("button", { name: "Back" }));
    expect(screen.getByText("Routing pane")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "2. Profile" }));
    expect(screen.getByText("Profile pane")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "4. Routing" }));
    expect(screen.getByText("Routing pane")).toBeVisible();
  });

  it("falls back to the first step when the dialog's fallback is not in the new step list", async () => {
    const user = setupUser();

    function Harness() {
      const [short, setShort] = useState(false);
      const steps = short ? ["Connection"] : ["Connection", "Deployment", "Review"];
      const wizard = useWizardSteps(steps, () => "Review");
      return (
        <DeploymentWizardShell
          currentStep={wizard.current}
          footerStart={
            <Button onClick={() => setShort(true)} type="button">
              Shorten
            </Button>
          }
          onOpenChange={vi.fn()}
          onPrimary={() => wizard.goTo("Deployment")}
          onStepChange={wizard.goTo}
          open
          primaryLabel="Next"
          resourceColorKey="model-1"
          resourceName="Test Model"
          steps={steps}
          title="Deploy model"
          visitedSteps={wizard.visited}
        >
          <p>{wizard.current} pane</p>
        </DeploymentWizardShell>
      );
    }

    render(<Harness />);
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(screen.getByText("Deployment pane")).toBeVisible();

    await user.click(screen.getByRole("button", { name: "Shorten" }));
    expect(screen.getByText("Connection pane")).toBeVisible();
    expect(screen.queryByRole("list", { name: "Deployment progress" })).not.toBeInTheDocument();
  });

  it("focuses the destination heading only after navigation and announces dialog messages", async () => {
    const user = setupUser();

    function Harness() {
      const steps = ["Profile", "Deployment"];
      const wizard = useWizardSteps(steps);
      return (
        <DeploymentWizardShell
          announcement={wizard.current === "Deployment" ? "Advanced on: 2 steps" : ""}
          currentStep={wizard.current}
          onOpenChange={vi.fn()}
          onPrimary={() => wizard.goTo("Deployment")}
          onStepChange={wizard.goTo}
          open
          primaryLabel="Continue"
          resourceColorKey="model-1"
          resourceName="Test Model"
          steps={steps}
          title="Deploy model"
          visitedSteps={wizard.visited}
        >
          <DeploymentFormSection focusTarget title={wizard.current}>
            <p>{wizard.current} fields</p>
          </DeploymentFormSection>
        </DeploymentWizardShell>
      );
    }

    render(<Harness />);
    const dialog = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
    const liveRegion = within(dialog).getByRole("status");
    expect(liveRegion).toHaveAttribute("aria-live", "polite");
    expect(liveRegion).toHaveTextContent(/^$/);
    const initialHeading = screen.getByRole("heading", { level: 2, name: "Profile" });
    expect(initialHeading).not.toHaveFocus();

    await user.click(screen.getByRole("button", { name: "Continue" }));
    const deploymentHeading = screen.getByRole("heading", { level: 2, name: "Deployment" });
    await waitFor(() => expect(deploymentHeading).toHaveFocus());
    expect(deploymentHeading).toHaveClass("focus-visible:ring-2");
    expect(within(dialog).getByRole("status")).toHaveTextContent("Advanced on: 2 steps");

    await user.click(screen.getByRole("button", { name: "1. Profile" }));
    await waitFor(() =>
      expect(screen.getByRole("heading", { level: 2, name: "Profile" })).toHaveFocus(),
    );
  });

  it("renders the submit error where the dialog places it, above the step content", () => {
    const view = render(
      <DeploymentWizardShell
        currentStep="Review"
        onOpenChange={vi.fn()}
        onPrimary={vi.fn()}
        onStepChange={vi.fn()}
        open
        primaryLabel="Deploy"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Deployment", "Review"]}
        submitError="Route name is already in use."
        title="Deploy model"
        visitedSteps={new Set(["Deployment", "Review"])}
      >
        <p>Review summary</p>
      </DeploymentWizardShell>,
    );

    const body = screen.getByTestId("deployment-dialog-body");
    const alert = within(body).getByRole("alert");
    expect(alert).toHaveTextContent("Route name is already in use.");
    expect(
      alert.compareDocumentPosition(screen.getByText("Review summary")) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();

    view.rerender(
      <DeploymentWizardShell
        currentStep="Deployment"
        onOpenChange={vi.fn()}
        onPrimary={vi.fn()}
        onStepChange={vi.fn()}
        open
        primaryLabel="Continue"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Deployment", "Review"]}
        title="Deploy model"
        visitedSteps={new Set(["Deployment", "Review"])}
      >
        <p>Deployment fields</p>
      </DeploymentWizardShell>,
    );
    expect(within(body).queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps persistent content fixed above the footer on every step while the body scrolls", async () => {
    const user = setupUser();

    function Harness() {
      const steps = ["Profile", "Deploy"];
      const wizard = useWizardSteps(steps);
      return (
        <DeploymentWizardShell
          currentStep={wizard.current}
          onOpenChange={vi.fn()}
          onPrimary={() => wizard.goTo("Deploy")}
          onStepChange={wizard.goTo}
          open
          persistentContent={<p>Memory on gpu1</p>}
          primaryLabel="Next"
          resourceColorKey="model-1"
          resourceName="Test Model"
          steps={steps}
          title="Deploy model"
          visitedSteps={wizard.visited}
        >
          <p>{wizard.current} fields</p>
        </DeploymentWizardShell>
      );
    }

    render(<Harness />);
    const dialog = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
    const body = within(dialog).getByTestId("deployment-dialog-body");
    const footer = within(dialog).getByTestId("deployment-dialog-footer");
    const persistent = within(dialog).getByTestId("deployment-dialog-persistent");
    expect(dialog).toHaveClass("grid-rows-[auto_minmax(0,1fr)_auto_auto]");
    expect(persistent).toHaveTextContent("Memory on gpu1");
    expect(body).not.toContainElement(persistent);
    expect(
      body.compareDocumentPosition(persistent) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(
      persistent.compareDocumentPosition(footer) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "Next" }));

    expect(within(body).getByText("Deploy fields")).toBeVisible();
    expect(within(dialog).getByTestId("deployment-dialog-persistent")).toBe(persistent);
    expect(persistent).toHaveTextContent("Memory on gpu1");
  });

  it("leaves the frame at three rows without persistent content", () => {
    render(
      <DeploymentWizardShell
        currentStep="Profile"
        onOpenChange={vi.fn()}
        onPrimary={vi.fn()}
        onStepChange={vi.fn()}
        open
        primaryLabel="Deploy"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Profile"]}
        title="Deploy model"
      >
        <p>Profile cards</p>
      </DeploymentWizardShell>,
    );

    const dialog = screen.getByRole("dialog", { name: "Test Model — Deploy model" });
    expect(dialog).toHaveClass("grid-rows-[auto_minmax(0,1fr)_auto]");
    expect(within(dialog).queryByTestId("deployment-dialog-persistent")).not.toBeInTheDocument();
  });

  it("protects a pending deployment from dismissal, navigation, and duplicate submission", async () => {
    const user = setupUser();
    const onOpenChange = vi.fn();
    const onPrimary = vi.fn();
    const onStepChange = vi.fn();

    render(
      <DeploymentWizardShell
        currentStep="Review"
        onOpenChange={onOpenChange}
        onPrimary={onPrimary}
        onStepChange={onStepChange}
        open
        openerRef={createRef<HTMLElement>()}
        pending
        primaryLabel="Deploying…"
        resourceColorKey="model-1"
        resourceName="Test Model"
        steps={["Profile", "Review"]}
        title="Deploy model"
        visitedSteps={new Set(["Profile", "Review"])}
      >
        <p>Review summary</p>
      </DeploymentWizardShell>,
    );

    expect(screen.getByRole("button", { name: "1. Profile" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Back" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Cancel" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Deploying…" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Close" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Deploying…" }));
    await user.click(screen.getByRole("button", { name: "1. Profile" }));
    expect(onPrimary).not.toHaveBeenCalled();
    expect(onStepChange).not.toHaveBeenCalled();
    expect(onOpenChange).not.toHaveBeenCalled();
  });
});
