import assert from "node:assert/strict";
import test from "node:test";

import { ESLint } from "eslint";

const invalidImageSource = `
export function MissingAlt() {
  return <img src="/logo.svg" />;
}
`;

test(
  "accessibility lint configuration rejects an image without alternative text",
  { timeout: 5_000 },
  async () => {
    const eslint = new ESLint();
    const [result] = await eslint.lintText(invalidImageSource, {
      filePath: "src/features/models/deployed/models-page.tsx",
    });

    assert.ok(
      result.messages.some(
        (message) => message.ruleId === "jsx-a11y/alt-text" && message.severity === 2,
      ),
    );
  },
);
