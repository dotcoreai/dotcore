import { readdirSync, readFileSync } from "node:fs";

import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { StatusBadge } from "@/components/ui/status-badge";

describe("StatusBadge", () => {
  it("renders the projected word and carries its tone", () => {
    render(<StatusBadge status={{ label: "Not ready", tone: "destructive" }} />);

    const badge = screen.getByText("Not ready");
    expect(badge).toHaveAttribute("data-variant", "destructive");
    expect(badge).toHaveAttribute("data-slot", "badge");
  });

  it("carries the attention label on its tooltip rather than as text", () => {
    render(
      <StatusBadge
        status={{ attention: "Waiting for endpoint", label: "Starting", tone: "info" }}
      />,
    );

    const badge = screen.getByText("Starting");
    expect(badge).toHaveAttribute("title", "Waiting for endpoint");
    expect(screen.queryByText("Waiting for endpoint")).not.toBeInTheDocument();
  });

  it("renders the warning tone with its warning token classes", () => {
    render(<StatusBadge status={{ label: "Degraded", tone: "warning" }} />);

    const badge = screen.getByText("Degraded");
    expect(badge).toHaveAttribute("data-variant", "warning");
    expect(badge).toHaveClass("bg-warning");
    expect(badge).toHaveClass("text-warning-foreground");
  });

  it("renders no tooltip when nothing qualifies the status", () => {
    render(<StatusBadge status={{ label: "Ready", tone: "success" }} />);

    expect(screen.getByText("Ready")).not.toHaveAttribute("title");
  });
});

/**
 * A status tone is always computed from a state, and a label badge always names a fixed property, so
 * the two are distinguishable in source: a computed `variant` belongs to a status and must reach the
 * DOM through `StatusBadge`, which can only be handed a projection.
 *
 * This keeps a surface from reintroducing the defect this phase fixed — an inspector deciding its own
 * colour for a state the card already projects — without restricting the badges that carry a label
 * rather than a state.
 */
describe("status tone enforcement", () => {
  const TONE_VARIANTS = ["success", "info", "warning", "neutral", "destructive"];

  /** Every authored `.ts`/`.tsx` source file under `src/`, excluding tests and generated code. */
  function sourceFiles(directory: string): string[] {
    return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
      const path = `${directory}/${entry.name}`;
      if (entry.isDirectory()) return entry.name === "gen" ? [] : sourceFiles(path);
      const authored = /\.tsx?$/.test(entry.name) && !entry.name.includes(".test.");
      return authored ? [path] : [];
    });
  }

  // Vitest runs from `frontend/`, so the authored tree is a fixed step from the working directory.
  const src = `${process.cwd()}/src`;
  const files = sourceFiles(src).map((path) => [
    path.slice(src.length + 1),
    readFileSync(path, "utf8"),
  ]);

  it("reads a meaningful number of source files, so it cannot pass vacuously", () => {
    expect(files.length).toBeGreaterThan(40);
  });

  it("gives no Badge a computed variant, since a computed tone belongs to a status", () => {
    const offenders = files
      .filter(([path]) => path !== "components/ui/status-badge.tsx")
      .filter(
        ([, source]) =>
          /<Badge\b[^>]*\bvariant=\{/.test(source) || /<Badge\n[^>]*variant=\{/.test(source),
      )
      .map(([path]) => path);

    expect(offenders).toEqual([]);
  });

  it("uses a literal tone variant only on badges that name a property, not a state", () => {
    const literal = files.flatMap(([path, source]) =>
      [...source.matchAll(/<Badge\s+variant="(\w+)"\s*>([^<]*)</g)]
        .filter(([, variant]) => TONE_VARIANTS.includes(variant))
        .map(([, , label]) => `${path}: ${label.trim()}`),
    );

    // An upstream model is External whatever it is doing, and a decision question's type is fixed
    // while it is edited; neither word is a state, so neither goes through a status projection.
    expect(literal.sort()).toEqual([
      "features/chat/decisions/decision-question-row.tsx: {typeLabel(type)}",
      "features/models/upstream/upstream-inspector.tsx: External",
    ]);
  });
});
