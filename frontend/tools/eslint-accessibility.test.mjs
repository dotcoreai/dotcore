import assert from "node:assert/strict";
import test from "node:test";

import { ESLint, Linter } from "eslint";
import jsxA11y from "eslint-plugin-jsx-a11y";
import tseslint from "typescript-eslint";

const invalidImageSource = `
export function MissingAlt() {
  return <img src="/logo.svg" />;
}
`;

const componentPath = "src/features/models/deployed/models-page.tsx";

test(
  "accessibility lint configuration rejects an image without alternative text",
  { timeout: 5_000 },
  async () => {
    // Linting through the project config builds the type-checked TypeScript program, which
    // takes seconds. Resolving the config is cheap, and its accessibility rules don't need types.
    const config = await new ESLint().calculateConfigForFile(componentPath);
    const accessibilityRules = Object.fromEntries(
      Object.entries(config.rules).filter(([name]) => name.startsWith("jsx-a11y/")),
    );

    const messages = new Linter().verify(
      invalidImageSource,
      [
        {
          files: ["**/*.tsx"],
          languageOptions: {
            parser: tseslint.parser,
            parserOptions: { ecmaFeatures: { jsx: true } },
          },
          plugins: { "jsx-a11y": jsxA11y },
          rules: accessibilityRules,
        },
      ],
      componentPath,
    );

    assert.ok(
      messages.some((message) => message.ruleId === "jsx-a11y/alt-text" && message.severity === 2),
    );
  },
);
