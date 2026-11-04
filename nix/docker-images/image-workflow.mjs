import { spawnSync } from "node:child_process";

/**
 * Docker plumbing shared by the application image workflows: native platform detection, image
 * inspection, base-layer checks, command results and workload removal. `subject` names the
 * application in error messages, such as `ComfyUI`.
 */
export function imageWorkflowKit(subject) {
  function dockerNativePlatform(timeoutMs) {
    const output = commandOutput(
      runDocker(
        ["info", "--format", "{{json .OSType}}|{{json .Architecture}}"],
        {
          timeout: timeoutMs,
        },
      ),
    ).trim();
    const [osName, architectureName] = output
      .split("|")
      .map((value) => JSON.parse(value));
    const architecture =
      architectureName === "x86_64"
        ? "amd64"
        : architectureName === "aarch64"
          ? "arm64"
          : architectureName;
    const platform = `${osName}/${architecture}`;
    if (!["linux/amd64", "linux/arm64"].includes(platform)) {
      throw new Error(
        `${subject} image test requires native linux/amd64 or linux/arm64 Docker, observed ${platform}.`,
      );
    }
    return platform;
  }

  /** Returns the Dockerfile's single `FROM` reference, which must match `pattern`. */
  function dockerfileBaseImage(dockerfile, pattern, expectation) {
    const references = [...dockerfile.matchAll(/^FROM\s+(\S+)\s*$/gm)].map(
      (match) => match[1],
    );
    if (references.length !== 1 || !pattern.test(references[0])) {
      throw new Error(`${subject} Dockerfile must contain ${expectation}.`);
    }
    return references[0];
  }

  function inspectImage(imageReference, timeoutMs) {
    return JSON.parse(
      commandOutput(
        runDocker(
          ["image", "inspect", imageReference, "--format", "{{json .}}"],
          {
            timeout: timeoutMs,
          },
        ),
      ),
    );
  }

  /** Compares the image's platform with the native one; the arm64 variant label (v8) is not part of it. */
  function verifyPlatform(label, image, nativePlatform) {
    const realized = `${image.Os}/${image.Architecture}`;
    if (realized !== nativePlatform) {
      throw new Error(
        `${subject} ${label} platform mismatch: expected ${nativePlatform}, observed ${realized}.`,
      );
    }
  }

  function verifyBaseLayers(image, upstreamImage) {
    const imageLayers = image.RootFS?.Layers;
    const upstreamLayers = upstreamImage.RootFS?.Layers;
    const containsUpstream =
      Array.isArray(imageLayers) &&
      Array.isArray(upstreamLayers) &&
      upstreamLayers.length > 0 &&
      imageLayers.length > upstreamLayers.length &&
      upstreamLayers.every((layer, index) => imageLayers[index] === layer);
    if (!containsUpstream) {
      throw new Error(
        `${subject} built image does not contain the authored upstream base layers.`,
      );
    }
  }

  function removeWorkload(workload, timeoutMs) {
    commandOutput(
      runDocker(["rm", "--force", "--volumes", workload], {
        timeout: timeoutMs,
      }),
    );
  }

  function commandOutput(result) {
    if (result.error)
      throw new Error(
        `${subject} image workflow could not start: ${result.error.message}`,
      );
    if (result.status !== 0)
      throw new Error(
        `${subject} image workflow failed: ${result.stderr.trim() || `exit status ${result.status}`}`,
      );
    return result.stdout;
  }

  return {
    commandOutput,
    dockerNativePlatform,
    dockerfileBaseImage,
    inspectImage,
    removeWorkload,
    runDocker,
    verifyBaseLayers,
    verifyPlatform,
  };
}

function runDocker(arguments_, { timeout }) {
  return spawnSync("docker", arguments_, {
    cwd: process.cwd(),
    encoding: "utf8",
    timeout,
  });
}
