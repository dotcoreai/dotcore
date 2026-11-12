import { act, render, screen } from "@testing-library/react";
import { useRef, useState } from "react";
import { describe, expect, it, vi } from "vitest";

import { Combobox, type ComboboxOption } from "@/components/ui/combobox";
import { expectResourceMarker } from "@/test/resource-marker";
import { setupUser } from "@/test/user";

const options: ComboboxOption[] = [
  { id: "alpha-01", label: "Alpha" },
  { id: "bravo-02", label: "Bravo" },
  { id: "charlie-03", label: "Charlie", disabled: true },
];

function SingleCombobox({
  clearable,
  loading = false,
  loadingMessageVisible,
  options: availableOptions = options,
  searchable,
}: {
  clearable?: boolean;
  loading?: boolean;
  loadingMessageVisible?: boolean;
  options?: ComboboxOption[];
  searchable?: boolean;
}) {
  const [value, setValue] = useState<string | null>(null);

  return (
    <>
      <Combobox
        label="Deployment"
        clearable={clearable}
        options={availableOptions}
        value={value}
        onValueChange={setValue}
        loading={loading}
        loadingMessageVisible={loadingMessageVisible}
        searchable={searchable}
        help="Choose one deployment."
      />
      <output data-testid="value">{value ?? "none"}</output>
    </>
  );
}

function InvalidCombobox() {
  const [value, setValue] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  return (
    <>
      <Combobox
        ariaInvalid
        errorDescriptionId="deployment-error"
        inputRef={inputRef}
        label="Deployment"
        onValueChange={setValue}
        options={options}
        value={value}
      />
      <p id="deployment-error">Choose a deployment before saving.</p>
      <button type="button" onClick={() => inputRef.current?.focus()}>
        Focus invalid deployment
      </button>
    </>
  );
}

function MultipleCombobox({
  closeOnSelect,
  initial = ["bravo-02", "alpha-01"],
}: {
  closeOnSelect?: boolean;
  initial?: string[];
}) {
  const [value, setValue] = useState(initial);

  return (
    <>
      <Combobox
        multiple
        closeOnSelect={closeOnSelect}
        label="Deployments"
        options={options}
        value={value}
        onValueChange={setValue}
      />
      <output data-testid="value">{value.join(",")}</output>
    </>
  );
}

describe("Combobox", () => {
  it("describes the input with its help and shows it in a popover beside the label", async () => {
    const user = setupUser();
    render(<SingleCombobox />);

    expect(screen.getByRole("combobox", { name: "Deployment" })).toHaveAccessibleDescription(
      "Choose one deployment.",
    );
    await user.click(screen.getByRole("button", { name: "About Deployment" }));
    expect(await screen.findByRole("dialog", { name: "About Deployment" })).toHaveTextContent(
      "Choose one deployment.",
    );
  });

  it("shows its status under the input and in the input's description", () => {
    render(
      <Combobox
        label="Deployment"
        onValueChange={() => {}}
        options={options}
        status="Loading deployments…"
        value={null}
      />,
    );

    expect(screen.getByText("Loading deployments…")).toBeVisible();
    expect(screen.getByRole("combobox", { name: "Deployment" })).toHaveAccessibleDescription(
      "Loading deployments…",
    );
  });

  it("filters options by label and stable ID", async () => {
    const user = setupUser();
    render(<SingleCombobox />);

    const input = screen.getByRole("combobox", { name: "Deployment" });
    await user.type(input, "bravo");
    expect(screen.getByRole("option", { name: /Bravo bravo-02/i })).toBeVisible();
    expect(screen.queryByRole("option", { name: /Alpha alpha-01/i })).not.toBeInTheDocument();

    await user.clear(input);
    await user.type(input, "alpha-01");
    expect(screen.getByRole("option", { name: /Alpha alpha-01/i })).toBeVisible();
  });

  it("shows an option's detail instead of its composite ID and filters by it", async () => {
    const user = setupUser();
    render(
      <SingleCombobox
        options={[
          { id: "node-a\u0000default", label: "Node A", detail: "default" },
          { id: "node-b\u0000default", label: "Node B", detail: "default" },
          { id: "node-b\u0000gpu", label: "Node B (gpu)", detail: "gpu" },
        ]}
      />,
    );

    const input = screen.getByRole("combobox", { name: "Deployment" });
    await user.click(input);
    expect(
      screen.getAllByRole("option").map((option) => option.getAttribute("aria-label")),
    ).toEqual(["Node A default", "Node B default", "Node B (gpu) gpu"]);

    await user.type(input, "gpu");
    expect(screen.getAllByRole("option")).toHaveLength(1);
    await user.click(screen.getByRole("option", { name: "Node B (gpu) gpu" }));
    expect(screen.getByTestId("value")).toHaveTextContent("node-b\u0000gpu");
    expect(input).toHaveValue("Node B (gpu)");
  });

  it("acts as a plain pick list when not searchable", async () => {
    const user = setupUser();
    render(<SingleCombobox searchable={false} />);

    const input = screen.getByRole("combobox", { name: "Deployment" });
    expect(input).toHaveAttribute("readonly");
    await user.click(input);
    await user.keyboard("charlie");
    expect(input).toHaveValue("");
    expect(screen.getAllByRole("option")).toHaveLength(3);

    await user.click(screen.getByRole("option", { name: /Bravo bravo-02/i }));
    expect(screen.getByTestId("value")).toHaveTextContent("bravo-02");
    expect(input).toHaveValue("Bravo");

    await user.click(screen.getByRole("button", { name: "Show Deployment options" }));
    await user.keyboard("{ArrowDown}{Enter}");
    expect(screen.getByTestId("value")).not.toHaveTextContent("none");
  });

  it("lists every option on first open when the selected value's label changed after it was set", async () => {
    const user = setupUser();
    const props = {
      label: "Execution target",
      onValueChange: () => {},
      searchable: false,
      value: "node\0gpu-0",
    };
    const { rerender } = render(<Combobox {...props} options={[]} />);
    rerender(
      <Combobox
        {...props}
        options={[
          { detail: "linux/amd64", id: "node\0gpu-0", label: "node / RTX 4090 · GPU 0" },
          { detail: "linux/amd64", id: "node\0gpu-1", label: "node / RTX 5090 · GPU 1" },
        ]}
      />,
    );

    const input = screen.getByRole("combobox", { name: "Execution target" });
    expect(input).toHaveValue("node / RTX 4090 · GPU 0");
    await user.click(input);
    expect(screen.getAllByRole("option").map((option) => option.textContent)).toEqual([
      "node / RTX 4090 · GPU 0linux/amd64",
      "node / RTX 5090 · GPU 1linux/amd64",
    ]);
  });

  it("selects a single controlled value with keyboard and clears it", async () => {
    const user = setupUser();
    render(<SingleCombobox />);

    const input = screen.getByRole("combobox", { name: "Deployment" });
    await user.type(input, "alpha");
    await user.keyboard("{ArrowDown}{Enter}");

    expect(screen.getByTestId("value")).toHaveTextContent("alpha-01");
    await user.click(screen.getByRole("button", { name: "Clear Deployment" }));
    expect(screen.getByTestId("value")).toHaveTextContent("none");
  });

  it("shows no clear button on a single combobox that is not clearable", async () => {
    const user = setupUser();
    render(<SingleCombobox clearable={false} />);

    await user.click(screen.getByRole("combobox", { name: "Deployment" }));
    await user.click(screen.getByRole("option", { name: /Alpha alpha-01/i }));

    expect(screen.getByTestId("value")).toHaveTextContent("alpha-01");
    expect(screen.queryByRole("button", { name: "Clear Deployment" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show Deployment options" })).toBeInTheDocument();
  });

  it("selects with the pointer and does not select disabled options", async () => {
    const user = setupUser();
    render(<SingleCombobox />);

    await user.click(screen.getByRole("combobox", { name: "Deployment" }));
    await user.click(screen.getByRole("option", { name: /Bravo bravo-02/i }));
    expect(screen.getByTestId("value")).toHaveTextContent("bravo-02");

    const input = screen.getByRole("combobox", { name: "Deployment" });
    await user.clear(input);
    await user.type(input, "charlie");
    expect(screen.getByRole("option", { name: /Charlie charlie-03/i })).toHaveAttribute(
      "aria-disabled",
      "true",
    );
    await user.click(screen.getByRole("option", { name: /Charlie charlie-03/i }));
    expect(screen.getByTestId("value")).toHaveTextContent("none");
  });

  it("marks only the selected option as selected, not the active keyboard option", async () => {
    const user = setupUser();
    render(<SingleCombobox />);

    const input = screen.getByRole("combobox", { name: "Deployment" });
    await user.type(input, "alpha");
    await user.keyboard("{ArrowDown}{Enter}");
    await user.click(screen.getByRole("button", { name: "Show Deployment options" }));
    await user.keyboard("{ArrowDown}");

    const alpha = screen.getByRole("option", { name: /Alpha alpha-01/i });
    const bravo = screen.getByRole("option", { name: /Bravo bravo-02/i });
    expect(alpha).toHaveAttribute("aria-selected", "true");
    expect(alpha.querySelector("svg")).not.toBeNull();
    expect(bravo).toHaveAttribute("aria-selected", "false");
    expect(bravo.querySelector("svg")).toBeNull();
  });

  it("wires invalid state and the described error to the input ref", async () => {
    const user = setupUser();
    render(<InvalidCombobox />);

    const input = screen.getByRole("combobox", { name: "Deployment" });
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAttribute("aria-describedby", "deployment-error");

    await user.click(screen.getByRole("button", { name: "Focus invalid deployment" }));
    expect(input).toHaveFocus();
  });

  it("preserves multiple value order and removes selected values through their chips", async () => {
    const user = setupUser();
    render(<MultipleCombobox />);

    expect(screen.getByTestId("value")).toHaveTextContent("bravo-02,alpha-01");
    expect(screen.queryByRole("button", { name: "Clear Deployments" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Remove Bravo" }));
    expect(screen.getByTestId("value")).toHaveTextContent("alpha-01");
  });

  it("keeps the multiple popup open after a pick by default", async () => {
    const user = setupUser();
    render(<MultipleCombobox initial={["bravo-02"]} />);

    await user.click(screen.getByRole("button", { name: "Show Deployments options" }));
    await user.click(await screen.findByRole("option", { name: /Alpha alpha-01/i }));
    expect(screen.getByTestId("value")).toHaveTextContent("bravo-02,alpha-01");
    expect(screen.getByRole("listbox")).toBeVisible();
  });

  it("closes the multiple popup after a pointer pick when asked", async () => {
    const user = setupUser();
    render(<MultipleCombobox closeOnSelect initial={["bravo-02"]} />);

    await user.click(screen.getByRole("button", { name: "Show Deployments options" }));
    await user.click(await screen.findByRole("option", { name: /Alpha alpha-01/i }));
    expect(screen.getByTestId("value")).toHaveTextContent("bravo-02,alpha-01");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  });

  it("closes the multiple popup after a keyboard pick when asked", async () => {
    const user = setupUser();
    render(<MultipleCombobox closeOnSelect initial={[]} />);

    await user.click(screen.getByRole("button", { name: "Show Deployments options" }));
    await screen.findByRole("listbox");
    await user.keyboard("{ArrowDown}{Enter}");
    expect(screen.getByTestId("value")).toHaveTextContent("alpha-01");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  });

  it("keeps the multiple popup closed when a chip is removed", async () => {
    const user = setupUser();
    render(<MultipleCombobox closeOnSelect initial={["bravo-02", "alpha-01"]} />);

    await user.click(screen.getByRole("button", { name: "Remove Bravo" }));
    expect(screen.getByTestId("value")).toHaveTextContent("alpha-01");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  });

  it("keeps multiple selected values when Escape is pressed with the popup closed", async () => {
    const user = setupUser();
    const onKeyDown = vi.fn<(key: string) => void>();
    render(
      // eslint-disable-next-line jsx-a11y/no-static-element-interactions -- observes propagation only
      <div
        onKeyDown={(event) => {
          onKeyDown(event.key);
        }}
      >
        <MultipleCombobox />
      </div>,
    );

    act(() => screen.getByRole("combobox", { name: "Deployments" }).focus());
    await user.keyboard("{Escape}");
    expect(screen.getByTestId("value")).toHaveTextContent("bravo-02,alpha-01");
    // Escape still reaches the surrounding surface, such as an inspector that closes on it.
    expect(onKeyDown).toHaveBeenCalledWith("Escape");
  });

  it("renders decorative option and chip markers without changing names or filtering", async () => {
    const user = setupUser();
    const markedOptions: ComboboxOption[] = [
      { id: "alpha-01", label: "Alpha", markerColorKey: "model-alpha" },
      { id: "bravo-02", label: "Bravo" },
    ];
    render(
      <Combobox
        multiple
        label="Deployments"
        options={markedOptions}
        value={["alpha-01"]}
        onValueChange={vi.fn()}
      />,
    );

    expectResourceMarker(
      screen.getByRole("button", { name: "Remove Alpha" }).parentElement,
      "model-alpha",
    );
    const input = screen.getByRole("combobox", { name: "Deployments" });
    await user.type(input, "alpha");
    const alpha = screen.getByRole("option", { name: "Alpha alpha-01" });
    expectResourceMarker(
      alpha.querySelector('[data-slot="resource-marker"]')?.parentElement,
      "model-alpha",
    );
    expect(screen.queryByRole("option", { name: "Bravo bravo-02" })).not.toBeInTheDocument();

    await user.clear(input);
    expect(
      screen
        .getByRole("option", { name: "Bravo bravo-02" })
        .querySelector('[data-slot="resource-marker"]'),
    ).toBeNull();
  });

  it("shows each option's status beside it and as a dot on its selected chip", async () => {
    const user = setupUser();
    render(
      <Combobox
        multiple
        label="Deployments"
        options={[
          { id: "alpha-01", label: "Alpha", status: { label: "Ready", tone: "success" } },
          { id: "bravo-02", label: "Bravo", status: { label: "Paused", tone: "neutral" } },
        ]}
        value={["alpha-01"]}
        onValueChange={vi.fn()}
      />,
    );

    const chip = screen.getByRole("button", { name: "Remove Alpha" }).parentElement!;
    expect(chip.querySelector('[data-slot="status-dot"]')).toHaveAttribute("data-tone", "success");
    expect(chip).toHaveTextContent("AlphaReady");

    await user.click(screen.getByRole("combobox", { name: "Deployments" }));
    const bravo = screen.getByRole("option", { name: "Bravo bravo-02 Paused" });
    expect(bravo.querySelector('[data-slot="status-dot"]')).toHaveAttribute("data-tone", "neutral");
    expect(bravo).toHaveTextContent("BravoPausedbravo-02");
  });

  it("keeps a hidden label as the input's name, including while the popup is open", async () => {
    const user = setupUser();
    render(
      <Combobox
        hideLabel
        label="Primary deployments"
        onValueChange={vi.fn()}
        options={options}
        value={null}
      />,
    );

    expect(screen.getByText("Primary deployments")).toHaveClass("sr-only");
    const input = screen.getByRole("combobox", { name: "Primary deployments" });
    await user.click(input);
    expect(screen.getByRole("listbox")).toBeVisible();
    expect(input).toHaveAttribute(
      "aria-labelledby",
      screen.getByText("Primary deployments").getAttribute("id"),
    );
  });

  it("announces loading, filtered empty, and no-options states", async () => {
    const user = setupUser();
    const { rerender } = render(<SingleCombobox loading />);
    const input = screen.getByRole("combobox", { name: "Deployment" });
    await user.click(input);
    expect(screen.getByText("Loading options…")).toBeVisible();

    rerender(<SingleCombobox loading loadingMessageVisible={false} />);
    expect(screen.queryByText("Loading options…")).not.toBeInTheDocument();
    expect(screen.queryByRole("option")).not.toBeInTheDocument();
    expect(screen.queryByText("No options available.")).not.toBeInTheDocument();
    expect(screen.queryByText("No matching options.")).not.toBeInTheDocument();

    rerender(<SingleCombobox />);
    await user.type(input, "missing");
    expect(screen.getByText("No matching options.")).toBeVisible();

    rerender(<SingleCombobox options={[]} />);
    expect(screen.getByText("No options available.")).toBeVisible();
  });
});
