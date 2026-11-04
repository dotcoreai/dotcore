export interface ModelConnectionExamples {
  opencodeCommand: string;
  piCommand: string;
  curl: string;
}

export function buildModelConnectionExamples(
  inferenceOrigin: string,
  modelId: string,
): ModelConnectionExamples {
  const baseUrl = `${inferenceOrigin.replace(/\/+$/, "")}/v1`;
  const requestBody = JSON.stringify({
    model: modelId,
    messages: [{ role: "user", content: "Hello from Dotcore." }],
  });

  return {
    opencodeCommand: codeCommand("opencode", inferenceOrigin, modelId),
    piCommand: codeCommand("pi", inferenceOrigin, modelId),
    curl: [
      "curl --fail-with-body --silent --show-error \\",
      "  --request POST \\",
      "  --url " + shellString(`${baseUrl}/chat/completions`) + " \\",
      "  --header 'Content-Type: application/json' \\",
      `  --data-raw ${shellString(requestBody)}`,
    ].join("\n"),
  };
}

/** A `dotc code` command launching one coding agent on a route, against the console's inference origin. */
function codeCommand(agent: string, inferenceOrigin: string, modelId: string): string {
  const origin = inferenceOrigin.replace(/\/+$/, "");

  return `dotc code ${agent} --model ${shellWord(modelId)} --inference-addr ${shellWord(origin)}`;
}

/** A shell word, left bare when it only holds characters a shell never interprets. */
function shellWord(value: string): string {
  return /^[A-Za-z0-9._\-/:]+$/.test(value) ? value : shellString(value);
}

function shellString(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}
