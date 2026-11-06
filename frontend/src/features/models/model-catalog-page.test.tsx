import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import {
  ModelDeploymentIntentSchema,
  ModelDeploymentSchema,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type CreateModelDeploymentResponse,
  CreateModelDeploymentResponseSchema,
  ListModelDeploymentsResponseSchema,
} from "@/gen/dotcore/model/v1/service_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";
import { isPanelHistoryState } from "@/lib/panel-stack";

import type { ModelDeployDialogProps } from "./model-deploy-dialog";
import {
  catalogModel,
  currentLocation,
  deferredRead,
  deployableModel,
  llamaRelease,
  localDeployment,
  modelTarget,
  readableMachine,
  renderModelsSection,
  unifiedSnapshot,
  upstreamDeployment,
  upstreamTemplate,
} from "./models-section-test-support";
import {
  compatibilityResponse,
  driverTooOld,
  targetCompatibility,
} from "./profile-compatibility-test-support";

// Records the props of each model deploy dialog render, still rendering the real dialog.
const deployDialogRender = vi.hoisted(() => vi.fn<(props: ModelDeployDialogProps) => void>());

vi.mock("./model-deploy-dialog", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./model-deploy-dialog")>();
  return {
    ...actual,
    ModelDeployDialog: (props: ModelDeployDialogProps) => {
      deployDialogRender(props);
      return <actual.ModelDeployDialog {...props} />;
    },
  };
});

describe("ModelCatalogPage panels", () => {
  it("opens a profile from the catalog model and returns to the model", async () => {
    const user = userEvent.setup();
    const { router } = renderModelsSection({
      initialEntry: "/models/catalog",
      models: [catalogModel("double", "Double", ["default", "large"])],
    });

    await user.click(await screen.findByRole("row", { name: "Double" }));
    const panel = await screen.findByRole("complementary", { name: "Double" });
    await user.click(within(panel).getByRole("row", { name: "large" }));

    expect(currentLocation(router)).toBe(
      "/models/catalog?panel=model-catalog%2Fcatalog%3Adouble&panel=model-catalog%2Fcatalog-profile%3Adouble%3Alarge",
    );
    await user.click(await screen.findByRole("button", { name: "Back to Double" }));
    expect(currentLocation(router)).toBe("/models/catalog?panel=model-catalog%2Fcatalog%3Adouble");
  });

  it("shares one catalog-wide compatibility read between the catalog model and its profiles", async () => {
    const user = userEvent.setup();
    const compatibility = vi.fn(() =>
      Promise.resolve(
        compatibilityResponse(
          "double",
          targetCompatibility(
            { executionTargetId: "pool", siteNodeId: "node" },
            { default: {}, large: { reason: driverTooOld } },
          ),
        ),
      ),
    );
    renderModelsSection({
      compatibility,
      initialEntry: "/models/catalog",
      models: [catalogModel("double", "Double", ["default", "large"])],
    });

    await user.click(await screen.findByRole("row", { name: "Double" }));
    const panel = await screen.findByRole("complementary", { name: "Double" });
    expect(
      await within(panel).findByRole("button", { name: "Runs on filter: this site" }),
    ).toBeVisible();
    expect(within(panel).queryByRole("row", { name: "large" })).not.toBeInTheDocument();

    await user.click(within(panel).getByRole("button", { name: "Remove Runs on filter" }));
    await user.click(within(panel).getByRole("row", { name: "large" }));
    const runsOn = await screen.findByRole("region", { name: "Runs on" });
    expect(runsOn).toHaveTextContent("Needs NVIDIA driver 575 or later (has 560)");
    const deploy = screen.getByRole("button", { name: "Deploy" });
    expect(deploy).toHaveAttribute("aria-disabled", "true");
    expect(deploy.querySelector("svg")).toHaveClass("lucide-rocket");
    expect(compatibility).toHaveBeenCalledOnce();
    expect(compatibility).toHaveBeenCalledWith({});
  });

  it("drops a stale Models page Catalog link instead of rewriting it", async () => {
    const gemma = catalogModel("gemma", "Gemma", ["default"]);
    const { router } = renderModelsSection({
      initialEntry: "/models?panel=models%2Fcatalog%3Agemma",
      models: [gemma],
    });

    await waitFor(() => expect(currentLocation(router)).toBe("/models"));
    expect(await screen.findByText("No models running")).toBeVisible();
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });
});

describe("ModelCatalogPage Connect upstream", () => {
  it("hides the action when the catalog has no UpstreamModelTemplate", async () => {
    renderModelsSection({
      initialEntry: "/models/catalog",
      models: [catalogModel("gemma", "Gemma", ["default"])],
    });

    expect(await screen.findByRole("row", { name: "Gemma" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Connect upstream" })).not.toBeInTheDocument();
  });

  it("opens the only UpstreamModelTemplate's deploy dialog without a panel", async () => {
    const user = userEvent.setup();
    const { router } = renderModelsSection({
      initialEntry: "/models/catalog",
      templates: [upstreamTemplate()],
    });

    await user.click(await screen.findByRole("button", { name: "Connect upstream" }));

    expect(
      await screen.findByRole("dialog", { name: "Upstream model — Deploy model" }),
    ).toBeVisible();
    expect(currentLocation(router)).toBe("/models/catalog");
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("lists several UpstreamModelTemplates and deploys the chosen one", async () => {
    const user = userEvent.setup();
    const { client, router } = renderModelsSection({
      initialEntry: "/models/catalog",
      templates: [upstreamTemplate(), upstreamTemplate("anthropic", "Anthropic Messages")],
    });

    await user.click(await screen.findByRole("button", { name: "Connect upstream" }));
    const menu = await screen.findByRole("menu");
    expect(
      within(menu)
        .getAllByRole("menuitem")
        .map((item) => item.textContent),
    ).toEqual(["OpenAI-compatible", "Anthropic Messages"]);
    await user.click(within(menu).getByRole("menuitem", { name: "Anthropic Messages" }));

    const dialog = await screen.findByRole("dialog", { name: "Upstream model — Deploy model" });
    expect(currentLocation(router)).toBe("/models/catalog");
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    await waitFor(() => expect(client.model.createModelDeployment).toHaveBeenCalledOnce());
    expect(vi.mocked(client.model.createModelDeployment).mock.calls[0][0].source).toMatchObject({
      case: "upstream",
      value: { upstreamModelTemplateId: "anthropic" },
    });
  });
});

describe("ModelCatalogPage deploy", () => {
  it("lands on the accepted local deployment on /models before the next read, and Back returns to the Catalog", async () => {
    const user = userEvent.setup();
    const model = deployableModel();
    const { client, router } = renderModelsSection({
      engineReleases: [llamaRelease()],
      initialEntry: "/models/catalog",
      models: [model],
    });

    await user.click(await screen.findByRole("row", { name: "Test Model" }));
    const nextRead = deferredRead();
    vi.mocked(client.model.listModelDeployments).mockImplementation(() => nextRead.promise);
    vi.mocked(client.model.createLocalModelDeployment!).mockResolvedValue(
      create(CreateModelDeploymentResponseSchema, {
        deployment: localDeployment("deployment-new", model, {
          status: ModelDeploymentStatus.QUEUED,
        }),
      }),
    );
    await user.click(
      within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByText("Step 1 of 3 — Profile");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
    );
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models?panel=models%2Fdeployment%3Adeployment-new"),
    );
    // The router reports /models before React commits it, so the Catalog page's own "Test Model"
    // panel can still be in the DOM; query the deployment panel afresh until it is the one shown.
    await waitFor(() =>
      expect(
        within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("region", {
          name: "Status",
        }),
      ).toHaveTextContent("Queued"),
    );
    const running = screen.getByRole("table", { name: "Running models" });
    const row = within(running).getByRole("row", { name: "Test Model" });
    expect(row).toHaveTextContent("Queued");

    await act(async () => {
      nextRead.resolve(
        create(ListModelDeploymentsResponseSchema, {
          deployments: [
            localDeployment("deployment-new", model, { status: ModelDeploymentStatus.LOADING }),
          ],
        }),
      );
      await nextRead.promise;
    });
    await waitFor(() => expect(row).toHaveTextContent("Loading"));

    await act(() => router.navigate(-1));
    expect(currentLocation(router)).toBe("/models/catalog?panel=model-catalog%2Fcatalog%3Amodel-1");
    expect(await screen.findByRole("complementary", { name: "Test Model" })).toHaveTextContent(
      "Catalog model",
    );
  });

  it("leaves the Catalog view parameters behind when the accepted deployment opens on /models", async () => {
    const user = userEvent.setup();
    const model = deployableModel();
    const { client, router } = renderModelsSection({
      engineReleases: [llamaRelease()],
      initialEntry: "/models/catalog?q=test&sort=-name",
      models: [model],
    });

    await user.click(await screen.findByRole("row", { name: "Test Model" }));
    expect(currentLocation(router)).toBe(
      "/models/catalog?q=test&sort=-name&panel=model-catalog%2Fcatalog%3Amodel-1",
    );
    const nextRead = deferredRead();
    vi.mocked(client.model.listModelDeployments).mockImplementation(() => nextRead.promise);
    vi.mocked(client.model.createLocalModelDeployment!).mockResolvedValue(
      create(CreateModelDeploymentResponseSchema, {
        deployment: localDeployment("deployment-new", model, {
          status: ModelDeploymentStatus.QUEUED,
        }),
      }),
    );
    await user.click(
      within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByText("Step 1 of 3 — Profile");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
    );
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models?panel=models%2Fdeployment%3Adeployment-new"),
    );
  });

  it("connects an UpstreamModelTemplate from the header action, lands on /models, and closes to /models", async () => {
    const user = userEvent.setup();
    const template = upstreamTemplate();
    const { client, router } = renderModelsSection({
      initialEntry: "/models/catalog",
      templates: [template],
    });
    const nextRead = deferredRead();
    vi.mocked(client.model.createModelDeployment).mockResolvedValue(
      create(CreateModelDeploymentResponseSchema, {
        deployment: upstreamDeployment(template, ModelDeploymentStatus.QUEUED),
      }),
    );

    await user.click(await screen.findByRole("button", { name: "Connect upstream" }));
    vi.mocked(client.model.listModelDeployments).mockImplementation(() => nextRead.promise);
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models?panel=models%2Fdeployment%3Aupstream-new"),
    );
    expect(await screen.findByRole("complementary", { name: "Private model" })).toBeVisible();
    const running = screen.getByRole("table", { name: "Running models" });
    expect(
      within(running).getByRole("row", { name: "Private model (upstream-new)" }),
    ).toHaveTextContent("Queued");

    await act(() => router.navigate(-1));
    expect(currentLocation(router)).toBe("/models/catalog");
    await waitFor(() => expect(screen.queryByRole("complementary")).not.toBeInTheDocument());
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await act(() => router.navigate(1));
    await user.click(await screen.findByRole("button", { name: "Close inspector" }));
    await waitFor(() => expect(currentLocation(router)).toBe("/models"));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });

  it("points an uncertain deploy outcome to Running on the Models page", async () => {
    const user = userEvent.setup();
    const { client, router } = renderModelsSection({
      engineReleases: [llamaRelease()],
      initialEntry: "/models/catalog",
      models: [deployableModel()],
    });
    vi.mocked(client.model.createLocalModelDeployment!).mockRejectedValue(
      new ConnectError("transport interrupted", Code.Unavailable),
    );

    await user.click(await screen.findByRole("row", { name: "Test Model" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByText("Step 1 of 3 — Profile");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
    );
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "Deploy outcome is uncertain (Unavailable): transport interrupted. Check Running on the Models page and service logs before retrying.",
    );
    expect(currentLocation(router)).toBe("/models/catalog?panel=model-catalog%2Fcatalog%3Amodel-1");
  });

  it("shows the service's reason when it rejects the total context", async () => {
    const user = userEvent.setup();
    const release = llamaRelease();
    release.parameterSchema!.parameters[0].exposed = true;
    const { client } = renderModelsSection({
      engineReleases: [release],
      initialEntry: "/models/catalog",
      models: [deployableModel()],
    });
    const reason =
      'invalid engine parameter values: total context of 16385 tokens is above the maximum context of 16384 tokens of configuration "local", which processes prompts in a single llama.cpp micro-batch; lower the total context';
    vi.mocked(client.model.createLocalModelDeployment!).mockRejectedValue(
      new ConnectError(reason, Code.InvalidArgument),
    );

    await user.click(await screen.findByRole("row", { name: "Test Model" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByText("Step 1 of 3 — Profile");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    const total = within(dialog).getByRole("textbox", { name: "Total context" });
    await user.clear(total);
    await user.type(total, "16385");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
    );
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      `Deploy was rejected (InvalidArgument): ${reason}. Check the request and service logs.`,
    );
    const submitted = vi.mocked(client.model.createLocalModelDeployment!).mock.calls[0]?.[0]
      .configuration?.engineParameterValues?.values;
    expect(
      submitted?.find((parameter) => parameter.parameterId === "total-context")?.value,
    ).toEqual({ case: "unsignedInteger", value: 16385n });
    expect(within(dialog).getByText("Step 3 of 3 — Deploy")).toBeVisible();
  });

  it("stays where the operator went when a local deploy is accepted after leaving the Catalog", async () => {
    const user = userEvent.setup();
    const model = deployableModel();
    const { client, router } = renderModelsSection({
      engineReleases: [llamaRelease()],
      initialEntry: "/models/catalog",
      models: [model],
    });
    const accepted = deferred<CreateModelDeploymentResponse>();
    vi.mocked(client.model.createLocalModelDeployment!).mockImplementation(() => accepted.promise);

    await user.click(await screen.findByRole("row", { name: "Test Model" }));
    await user.click(
      within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("button", {
        name: "Deploy",
      }),
    );
    const dialog = await screen.findByRole("dialog");
    await within(dialog).findByText("Step 1 of 3 — Profile");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await waitFor(() =>
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
    );
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    await act(() => router.navigate("/models"));
    vi.mocked(client.model.listModelDeployments).mockImplementation(() => deferredRead().promise);
    await act(async () => {
      accepted.resolve(
        create(CreateModelDeploymentResponseSchema, {
          deployment: localDeployment("deployment-new", model, {
            status: ModelDeploymentStatus.QUEUED,
          }),
        }),
      );
      await accepted.promise;
    });

    const running = await screen.findByRole("table", { name: "Running models" });
    expect(within(running).getByRole("row", { name: "Test Model" })).toHaveTextContent("Queued");
    expect(currentLocation(router)).toBe("/models");
  });

  it("stays where the operator went when an upstream deploy is accepted after leaving the Catalog", async () => {
    const user = userEvent.setup();
    const template = upstreamTemplate();
    const { client, router } = renderModelsSection({
      initialEntry: "/models/catalog",
      templates: [template],
    });
    const accepted = deferred<CreateModelDeploymentResponse>();
    vi.mocked(client.model.createModelDeployment).mockImplementation(() => accepted.promise);

    await user.click(await screen.findByRole("button", { name: "Connect upstream" }));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
    await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
    await user.click(within(dialog).getByRole("button", { name: "Next" }));
    await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
    await act(() => router.navigate("/models"));
    vi.mocked(client.model.listModelDeployments).mockImplementation(() => deferredRead().promise);
    await act(async () => {
      accepted.resolve(
        create(CreateModelDeploymentResponseSchema, {
          deployment: upstreamDeployment(template, ModelDeploymentStatus.QUEUED),
        }),
      );
      await accepted.promise;
    });

    const running = await screen.findByRole("table", { name: "Running models" });
    expect(
      within(running).getByRole("row", { name: "Private model (upstream-new)" }),
    ).toHaveTextContent("Queued");
    expect(currentLocation(router)).toBe("/models");
  });

  describe("route name conflict from the backend", () => {
    async function openLocalDialog(rejection: ConnectError) {
      const user = userEvent.setup();
      const { client } = renderModelsSection({
        engineReleases: [llamaRelease()],
        initialEntry: "/models/catalog",
        models: [deployableModel()],
      });
      vi.mocked(client.model.createLocalModelDeployment!).mockRejectedValue(rejection);
      await user.click(await screen.findByRole("row", { name: "Test Model" }));
      await user.click(
        within(screen.getByRole("complementary", { name: "Test Model" })).getByRole("button", {
          name: "Deploy",
        }),
      );
      const dialog = await screen.findByRole("dialog");
      await within(dialog).findByText("Step 1 of 3 — Profile");
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      await waitFor(() =>
        expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled(),
      );
      return { dialog, user };
    }

    async function openUpstreamDialog(rejection: ConnectError) {
      const user = userEvent.setup();
      const { client } = renderModelsSection({
        initialEntry: "/models/catalog",
        templates: [upstreamTemplate()],
      });
      vi.mocked(client.model.createModelDeployment).mockRejectedValue(rejection);
      await user.click(await screen.findByRole("button", { name: "Connect upstream" }));
      const dialog = await screen.findByRole("dialog");
      await user.type(within(dialog).getByLabelText("Base URL"), "https://endpoint.example/v1");
      await user.type(within(dialog).getByLabelText("Upstream model ID"), "private-model");
      await user.click(within(dialog).getByRole("button", { name: "Next" }));
      await within(dialog).findByText("Step 2 of 2 — Deploy");
      return { dialog, user };
    }

    const conflict = new ConnectError(
      "Inference route name already exists: test-model",
      Code.AlreadyExists,
    );
    const upstreamConflict = new ConnectError(
      "Inference route name already exists: private-model",
      Code.AlreadyExists,
    );

    const takenLocal =
      'An inference route named "test-model" already exists. Change the route or the deployment name. Follows the deployment name.';
    const takenUpstream =
      'An inference route named "private-model" already exists. Change the route or the deployment name. Follows the deployment name.';

    it("shows the taken route on the route field and blocks Deploy after a rejected local deploy", async () => {
      const { dialog, user } = await openLocalDialog(conflict);
      await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

      await waitFor(() =>
        expect(
          within(dialog).getByRole("textbox", { name: "Route name" }),
        ).toHaveAccessibleDescription(takenLocal),
      );
      expect(within(dialog).getByText("Step 3 of 3 — Deploy")).toBeVisible();
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();
    });

    it("uses a message without a route name when the backend message has none", async () => {
      const { dialog, user } = await openLocalDialog(
        new ConnectError("route exists", Code.AlreadyExists),
      );
      await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

      await waitFor(() =>
        expect(
          within(dialog).getByRole("textbox", { name: "Route name" }),
        ).toHaveAccessibleDescription(
          "An inference route with this name already exists. Change the route or the deployment name. Follows the deployment name.",
        ),
      );
    });

    it("drops the local conflict once the deployment is hidden", async () => {
      const { dialog, user } = await openLocalDialog(conflict);
      await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
      await waitFor(() =>
        expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled(),
      );
      await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
      await user.click(within(dialog).getByRole("radio", { name: "Hidden" }));

      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();
      expect(within(dialog).queryByText(/inference route named/)).not.toBeInTheDocument();
    });

    it("shows the taken route on the route field and blocks Deploy after a rejected upstream deploy", async () => {
      const { dialog, user } = await openUpstreamDialog(upstreamConflict);
      await user.click(within(dialog).getByRole("button", { name: "Deploy" }));

      await waitFor(() =>
        expect(within(dialog).getByLabelText("Route name")).toHaveAccessibleDescription(
          takenUpstream,
        ),
      );
      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled();
    });

    it("drops the upstream conflict once the deployment is hidden", async () => {
      const { dialog, user } = await openUpstreamDialog(upstreamConflict);
      await user.click(within(dialog).getByRole("button", { name: "Deploy" }));
      await waitFor(() =>
        expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeDisabled(),
      );
      await user.click(within(dialog).getByRole("checkbox", { name: "Advanced" }));
      await user.click(within(dialog).getByRole("radio", { name: "Hidden" }));

      expect(within(dialog).getByRole("button", { name: "Deploy" })).toBeEnabled();
      expect(within(dialog).queryByText(/inference route named/)).not.toBeInTheDocument();
    });
  });
});

describe("ModelCatalogPage deploy memory", () => {
  it("hands the deploy dialog the site's model deployments without watching machine memory", async () => {
    const user = userEvent.setup();
    const { watch } = renderModelsSection({
      engineReleases: [llamaRelease()],
      initialEntry: "/models/catalog",
      machines: [readableMachine("spark-01", "Spark one")],
      models: [deployableModel()],
      snapshots: { "spark-01": unifiedSnapshot() },
    });

    await user.click(await screen.findByRole("row", { name: "Test Model" }));
    const panel = await screen.findByRole("complementary", { name: "Test Model" });
    await user.click(within(panel).getByRole("button", { name: "Deploy" }));
    await screen.findByRole("dialog");

    expect(latestDeployDialogProps().deployments).toEqual([]);
    expect(watch.watchedKinds()).not.toContain("machines");
    expect(watch.watchedKinds()).not.toContain("hardwareSnapshots");
  });
});

describe("ModelCatalogPage deployments", () => {
  const gemma = catalogModel("gemma", "Gemma", ["default", "long"]);
  const qwen = catalogModel("qwen", "Qwen", ["default"]);

  function gemmaDeployment(
    modelDeploymentId: string,
    modelDeploymentName: string,
    profileId: string,
    status: ModelDeploymentStatus,
  ) {
    const deployment = create(ModelDeploymentSchema, {
      ...localDeployment(modelDeploymentId, gemma, { status }),
      intent: create(ModelDeploymentIntentSchema, {
        placement: { executionTargetId: "spark-gpu", siteNodeId: "spark-01" },
      }),
      modelDeploymentName,
    });
    if (deployment.snapshot?.source.case === "local") {
      deployment.snapshot.source.value.profileId = profileId;
    }
    return deployment;
  }

  function renderGemmaDeployments() {
    return renderModelsSection({
      deployments: [
        gemmaDeployment("deployment-a", "Gemma chat", "default", ModelDeploymentStatus.READY),
        gemmaDeployment("deployment-b", "Gemma long", "long", ModelDeploymentStatus.LOADING),
        localDeployment("deployment-q", qwen, { status: ModelDeploymentStatus.READY }),
        upstreamDeployment(upstreamTemplate(), ModelDeploymentStatus.READY),
      ],
      executionTargets: [modelTarget("spark-01", "spark-gpu", "NVIDIA GB10", "Spark one")],
      initialEntry: "/models/catalog",
      models: [gemma, qwen, catalogModel("idle", "Idle", ["default"])],
    });
  }

  it("lists the catalog model's deployments with their profile, placement and status", async () => {
    const user = userEvent.setup();
    renderGemmaDeployments();

    const row = await screen.findByRole("row", { name: "Gemma" });
    expect(within(row).getByText("2 deployed")).toBeVisible();
    await user.click(row);
    const panel = await screen.findByRole("complementary", { name: "Gemma" });
    const section = within(panel).getByRole("region", { name: "Deployments" });

    expect(within(section).getByText("2 deployments")).toBeVisible();
    const list = within(section).getByRole("list", { name: "Deployments of Gemma" });
    expect(
      within(list)
        .getAllByRole("button")
        .map((button) => button.getAttribute("aria-label")),
    ).toEqual(["View deployment Gemma chat, Ready", "View deployment Gemma long, Loading"]);
    expect(within(list).getByText("default · Spark one / NVIDIA GB10")).toBeVisible();
    expect(within(list).getByText("long · Spark one / NVIDIA GB10")).toBeVisible();
  });

  it("opens a listed deployment above the catalog model, and Back returns to the model", async () => {
    const user = userEvent.setup();
    const { router } = renderGemmaDeployments();

    await user.click(await screen.findByRole("row", { name: "Gemma" }));
    const panel = await screen.findByRole("complementary", { name: "Gemma" });
    await user.click(
      within(panel).getByRole("button", { name: "View deployment Gemma chat, Ready" }),
    );

    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models?panel=model-catalog%2Fcatalog%3Agemma&panel=models%2Fdeployment%3Adeployment-a",
      ),
    );
    expect(await screen.findByRole("complementary", { name: "Gemma chat" })).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Back to Gemma" }));

    await waitFor(() =>
      expect(currentLocation(router)).toBe("/models/catalog?panel=model-catalog%2Fcatalog%3Agemma"),
    );
    expect(await screen.findByRole("complementary", { name: "Gemma" })).toBeVisible();
  });

  it("shows no deployments section for an undeployed catalog model", async () => {
    const user = userEvent.setup();
    renderGemmaDeployments();

    await user.click(await screen.findByRole("row", { name: "Idle" }));
    const panel = await screen.findByRole("complementary", { name: "Idle" });

    expect(within(panel).getByRole("region", { name: "Catalog model" })).toBeVisible();
    expect(within(panel).queryByRole("region", { name: "Deployments" })).not.toBeInTheDocument();
  });
});

describe("ModelCatalogPage from a deployment", () => {
  const gemma = catalogModel("gemma", "Gemma", ["default"]);
  const deployment = create(ModelDeploymentSchema, {
    ...localDeployment("deployment-a", gemma, { status: ModelDeploymentStatus.READY }),
    modelDeploymentName: "Gemma chat",
  });
  const deploymentLocation = "/models?panel=models%2Fdeployment%3Adeployment-a";
  const catalogLocation =
    "/models/catalog?panel=models%2Fdeployment%3Adeployment-a&panel=model-catalog%2Fcatalog%3Agemma";

  async function openCatalogFromDeployment(models = [gemma]) {
    const user = userEvent.setup();
    const { router } = renderModelsSection({
      deployments: [deployment],
      initialEntry: "/models",
      models,
    });
    await user.click(await screen.findByRole("row", { name: "Gemma chat" }));
    const deploymentPanel = await screen.findByRole("complementary", { name: "Gemma chat" });
    await user.click(within(deploymentPanel).getByRole("link", { name: "Gemma" }));
    await waitFor(() => expect(currentLocation(router)).toBe(catalogLocation));
    return { router, user };
  }

  it("labels the catalog model's Back with the deployment and returns to it on /models", async () => {
    const { router, user } = await openCatalogFromDeployment();

    const catalogPanel = await screen.findByRole("complementary", { name: "Gemma" });
    expect(within(catalogPanel).getByText("1 of 1 profile")).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Back to Gemma chat" }));

    await waitFor(() => expect(currentLocation(router)).toBe(deploymentLocation));
    expect(await screen.findByRole("complementary", { name: "Gemma chat" })).toBeVisible();
  });

  it("returns to the deployment from another catalog model opened from the table", async () => {
    const { router, user } = await openCatalogFromDeployment([
      gemma,
      catalogModel("qwen", "Qwen", ["default"]),
    ]);

    await user.click(await screen.findByRole("row", { name: "Qwen" }));
    await waitFor(() =>
      expect(currentLocation(router)).toBe(
        "/models/catalog?panel=models%2Fdeployment%3Adeployment-a&panel=model-catalog%2Fcatalog%3Aqwen",
      ),
    );
    await screen.findByRole("complementary", { name: "Qwen" });
    await user.click(screen.getByRole("button", { name: "Back to Gemma chat" }));

    await waitFor(() => expect(currentLocation(router)).toBe(deploymentLocation));
    expect(await screen.findByRole("complementary", { name: "Gemma chat" })).toBeVisible();
  });

  it("returns to the deployment the catalog model was opened from when it is selected in the list", async () => {
    const { router, user } = await openCatalogFromDeployment();

    const catalogPanel = await screen.findByRole("complementary", { name: "Gemma" });
    await user.click(
      within(catalogPanel).getByRole("button", { name: "View deployment Gemma chat, Ready" }),
    );

    await waitFor(() => expect(currentLocation(router)).toBe(deploymentLocation));
    expect(await screen.findByRole("complementary", { name: "Gemma chat" })).toBeVisible();
  });

  it("counts the cross-page push as one more panel step and closes to /models", async () => {
    const { router, user } = await openCatalogFromDeployment();

    const state: unknown = router.state.location.state;
    expect(isPanelHistoryState(state) && state.__dotcorePanel.step).toBe(2);
    await user.click(await screen.findByRole("button", { name: "Close inspector" }));

    await waitFor(() => expect(currentLocation(router)).toBe("/models"));
    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
    expect(await screen.findByRole("table", { name: "Running models" })).toBeVisible();
  });
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });

  return { promise, resolve };
}

function latestDeployDialogProps(): ModelDeployDialogProps {
  const props = deployDialogRender.mock.lastCall?.[0];
  if (!props) throw new Error("The model deploy dialog has not rendered.");
  return props;
}
