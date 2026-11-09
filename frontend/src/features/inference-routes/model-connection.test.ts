import { describe, expect, it } from "vitest";

import { buildModelConnectionExamples } from "./model-connection";

describe("buildModelConnectionExamples", () => {
  it("builds complete unauthenticated client examples for an inference route", () => {
    const examples = buildModelConnectionExamples("http://100.64.0.9:6407", "route/model-alpha");

    expect(examples).toEqual({
      opencodeCommand:
        "dotc code opencode --model route/model-alpha --inference-addr http://100.64.0.9:6407",
      piCommand: "dotc code pi --model route/model-alpha --inference-addr http://100.64.0.9:6407",
      curl: `curl --fail-with-body --silent --show-error \\
  --request POST \\
  --url 'http://100.64.0.9:6407/v1/chat/completions' \\
  --header 'Content-Type: application/json' \\
  --data-raw '{"model":"route/model-alpha","messages":[{"role":"user","content":"Hello from Dotcore."}]}'`,
    });

    const generated = Object.values(examples).join("\n");
    expect(generated).not.toContain("Authorization");
    expect(generated).not.toContain("OPENAI_API_KEY");
    expect(generated).not.toContain("127.0.0.1:50051");
    expect(generated).not.toContain("model_deployment_id");
  });

  it("quotes only values with characters outside the safe set", () => {
    const origin = "https://gateway.example.test:7443/route/'$(touch pwned)";
    const routeName = `route/quote'";\n$(touch pwned)`;
    const examples = buildModelConnectionExamples(origin, routeName);

    expect(examples.opencodeCommand).toBe(
      `dotc code opencode --model 'route/quote'"'"'";\n$(touch pwned)' --inference-addr 'https://gateway.example.test:7443/route/'"'"'$(touch pwned)'`,
    );
    expect(examples.piCommand).toBe(
      `dotc code pi --model 'route/quote'"'"'";\n$(touch pwned)' --inference-addr 'https://gateway.example.test:7443/route/'"'"'$(touch pwned)'`,
    );
    expect(examples.curl).toContain(
      "--url 'https://gateway.example.test:7443/route/'\"'\"'$(touch pwned)/v1/chat/completions'",
    );
    expect(examples.curl).not.toContain("Authorization");
  });

  it("does not add a second v1 segment when the origin has a trailing slash", () => {
    const examples = buildModelConnectionExamples("http://127.0.0.1:50052/", "chat-model");

    expect(examples.opencodeCommand).toBe(
      "dotc code opencode --model chat-model --inference-addr http://127.0.0.1:50052",
    );
    expect(examples.piCommand).toBe(
      "dotc code pi --model chat-model --inference-addr http://127.0.0.1:50052",
    );
    expect(examples.curl).toContain("/v1/chat/completions'");
  });
});
