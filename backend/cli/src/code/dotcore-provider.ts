// Registers the site's inference API as Pi's `dotcore` provider, with one model per enabled
// inference route, from the definition `dotc code pi` passes in the environment. Without that
// definition it registers nothing. Routes do not expose their context and output sizes, so every
// model gets the same fixed placeholder sizes.
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  const raw = process.env.DOTCORE_PI_PROVIDER;
  if (!raw) return;
  const { baseUrl, models } = JSON.parse(raw);
  pi.registerProvider("dotcore", {
    name: "Dotcore",
    baseUrl,
    // Pi requires a key; the inference API has no authentication and ignores it.
    apiKey: "none",
    api: "openai-completions",
    models: models.map((id: string) => ({
      id,
      name: id,
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 128000,
      maxTokens: 8192,
    })),
  });
}
