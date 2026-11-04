import { compile } from "@tailwindcss/node";
import { render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it } from "vitest";

import {
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { readIndexCss } from "@/test/css-tokens";

// The frontend package root, so @tailwindcss/node can resolve index.css's node_modules imports
// (shadcn/tailwind.css, tw-animate-css). Vitest's working directory is already the package root.
const FRONTEND_ROOT = process.cwd();

// The exact candidate strings Card's className emits for the selected state, so the compiled
// output carries the real utility rules rather than a hand-copied approximation.
const CARD_SELECTED_CANDIDATES = [
  "data-[selected=true]:bg-info",
  "data-[selected=true]:shadow-[inset_4px_0_0_var(--color-primary)]",
];

describe("Card", () => {
  it("uses a real border for its edge instead of a ring, so it occupies layout space", () => {
    render(<Card data-testid="card">Body</Card>);

    const card = screen.getByTestId("card");
    expect(card).toHaveClass("border");
    expect(card).toHaveClass("border-border");
    expect(card.className).not.toMatch(/\bring-/);
  });

  it("carries the sole horizontal inset on the outer card, not on its child regions", () => {
    render(
      <Card data-testid="card">
        <CardHeader data-testid="header">
          <CardTitle>Title</CardTitle>
          <CardDescription>Description</CardDescription>
        </CardHeader>
        <CardContent data-testid="content">Content</CardContent>
        <CardFooter data-testid="footer">Footer</CardFooter>
      </Card>,
    );

    expect(screen.getByTestId("card").className).toMatch(/\bp-\(--card-spacing\)/);
    for (const testId of ["header", "content", "footer"]) {
      expect(screen.getByTestId(testId).className).not.toMatch(/\bpx-/);
    }
  });

  it("no longer zeroes its own bottom padding when a footer is present", () => {
    render(
      <Card data-testid="card">
        <CardContent>Content</CardContent>
        <CardFooter>Footer</CardFooter>
      </Card>,
    );

    expect(screen.getByTestId("card").className).not.toMatch(/pb-0/);
  });

  it("renders the footer as flowed content, with no divider or tinted band the demo has no equivalent for", () => {
    render(<CardFooter data-testid="footer">Footer</CardFooter>);

    const footer = screen.getByTestId("footer");
    expect(footer.className).not.toMatch(/\bborder-t\b/);
    expect(footer.className).not.toMatch(/bg-muted/);
  });

  // jsdom ignores declarations inside CSS cascade layers. Inspect the real Tailwind output here,
  // and guard the unlayered consumer CSS separately for declarations that could outrank it.
  describe("selected state", () => {
    let compiledSelectedCss: string;

    beforeAll(async () => {
      const compiler = await compile(readIndexCss(), {
        base: FRONTEND_ROOT,
        onDependency: () => {},
      });
      compiledSelectedCss = compiler.build(CARD_SELECTED_CANDIDATES);
    });

    it("renders data-selected when selected, and omits the attribute otherwise", () => {
      const { rerender } = render(
        <Card data-testid="card" selected>
          Body
        </Card>,
      );
      expect(screen.getByTestId("card")).toHaveAttribute("data-selected", "true");

      rerender(<Card data-testid="card">Body</Card>);
      expect(screen.getByTestId("card")).not.toHaveAttribute("data-selected");

      rerender(
        <Card data-testid="card" selected={false}>
          Body
        </Card>,
      );
      expect(screen.getByTestId("card")).not.toHaveAttribute("data-selected");
    });

    it("compiles the selected classes to a 4px primary accent bar and an info background", () => {
      render(
        <Card data-testid="card" selected>
          Body
        </Card>,
      );

      const card = screen.getByTestId("card");
      for (const candidate of CARD_SELECTED_CANDIDATES) {
        expect(card.className.split(" ")).toContain(candidate);
      }

      const backgroundRule = ruleWithDeclaration(
        compiledSelectedCss,
        "background-color: var(--info);",
      );
      expect(backgroundRule.selector).toContain('[data-selected="true"]');

      const shadowRule = ruleWithDeclaration(
        compiledSelectedCss,
        "--tw-shadow: inset 4px 0 0 var(--tw-shadow-color, var(--color-primary));",
      );
      expect(shadowRule.selector).toContain('[data-selected="true"]');
      expect(shadowRule.body).toContain(
        "box-shadow: var(--tw-inset-shadow), var(--tw-inset-ring-shadow), var(--tw-ring-offset-shadow), var(--tw-ring-shadow), var(--tw-shadow);",
      );
    });

    it("renders an unselected card identically to one rendered without the selected prop", () => {
      render(
        <>
          <Card data-testid="explicit-false" selected={false}>
            Body
          </Card>
          <Card data-testid="omitted">Body</Card>
        </>,
      );

      const explicit = screen.getByTestId("explicit-false");
      const omitted = screen.getByTestId("omitted");
      expect(explicit.className).toBe(omitted.className);
      expect(explicit).not.toHaveAttribute("data-selected");
      expect(omitted).not.toHaveAttribute("data-selected");
    });

    // "consumer-card" is a stand-in for any caller-supplied className; it names no real consumer.
    it("keeps card consumer CSS from overriding the primitive selected treatment", () => {
      render(
        <Card data-testid="card" selected className="consumer-card">
          Body
        </Card>,
      );

      expect(screen.getByTestId("card")).toHaveAttribute("data-selected", "true");
      expect(cardSurfaceCollisions(readIndexCss())).toEqual([]);
    });

    it("detects an unlayered card consumer that overrides a selected-state property", () => {
      const cssWithCollision = `${readIndexCss()}\n.consumer-card { box-shadow: none; }`;

      expect(cardSurfaceCollisions(cssWithCollision)).toEqual([".consumer-card: box-shadow"]);
    });
  });
});

function ruleWithDeclaration(css: string, declaration: string): { body: string; selector: string } {
  for (const match of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const [, selector, body] = match;
    if (body.includes(declaration)) {
      return { body, selector: selector.trim() };
    }
  }

  throw new Error(`No compiled rule contains '${declaration}'`);
}

function cardSurfaceCollisions(css: string): string[] {
  const collisions: string[] = [];

  for (const match of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const [, rawSelector, body] = match;
    const selector = rawSelector.trim();
    if (!selector.includes("card")) {
      continue;
    }

    for (const declaration of body.matchAll(/(?:^|;)\s*(background(?:-color)?|box-shadow)\s*:/g)) {
      collisions.push(`${selector}: ${declaration[1]}`);
    }
  }

  return collisions;
}
