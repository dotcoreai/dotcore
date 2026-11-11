import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";

import { syntheticCatalog } from "@/features/models/catalog/catalog-table-test-support";
import {
  ListModelProfileCompatibilityResponseSchema,
  LocalModelDeploymentAdvisory,
  type ModelCompatibilityReason,
  ModelCompatibilityReasonCode,
  ModelCompatibilityReasonSchema,
  ModelCompatibilityStatus,
} from "@/gen/dotcore/model/v1/service_pb";
import {
  type ConfiguredExecutionTarget,
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
} from "@/gen/dotcore/site/v1/service_pb";

import { profileEstimateKey, type ProfileRow, projectProfileRows } from "./model-row";
import {
  type CompatibilityVerdict,
  indexProfileCompatibility,
  profileScopeVerdict,
  profileTargetVerdict,
  runsOnScopeKey,
  type TargetIdentity,
} from "./profile-compatibility";
import {
  compatibilityAdvisoryText,
  compatibilityReasonText,
  profileCompatibilityStatus,
} from "./profile-compatibility-copy";
import { profileSizing } from "./profile-compatibility-test-support";

const { RUNS, CANNOT_RUN, UNKNOWN } = ModelCompatibilityStatus;

type ReasonInit = MessageInitShape<typeof ModelCompatibilityReasonSchema>;

// A lab site: a unified GB10, two RTX workstations (one with an old driver, one with two GPUs on
// one target), and a Mac that isn't reporting.
const spark = target("spark", "gpu");
const ws1 = target("ws-1", "gpu0");
const ws2 = target("ws-2", "gpu0");
const ws3 = target("ws-3", "gpus");
const mac = target("mac", "native");

const compatibility = indexProfileCompatibility(
  create(ListModelProfileCompatibilityResponseSchema, {
    entries: [
      entry("q4", spark, RUNS, undefined, [
        LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
      ]),
      entry("q4", ws1, RUNS),
      entry("q4", ws2, RUNS),
      entry("q4", ws3, CANNOT_RUN, {
        acceleratorDeviceCount: 2,
        code: ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH,
        requiredAcceleratorDeviceCount: 1,
      }),
      entry("ninfer", spark, CANNOT_RUN, {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
      }),
      entry("ninfer", ws1, RUNS),
      entry("ninfer", ws2, CANNOT_RUN, {
        code: ModelCompatibilityReasonCode.NVIDIA_DRIVER_BELOW_MINIMUM,
        minimumNvidiaDriverMajorVersion: 575,
        observedNvidiaDriverMajorVersion: 560,
      }),
      entry("ninfer", ws3, CANNOT_RUN, {
        acceleratorDeviceCount: 2,
        code: ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH,
        requiredAcceleratorDeviceCount: 1,
      }),
      entry("apple", spark, CANNOT_RUN, {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
      }),
      entry("apple", ws1, CANNOT_RUN, {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
      }),
      entry("apple", ws2, CANNOT_RUN, {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
      }),
      entry("apple", ws3, CANNOT_RUN, {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
      }),
    ],
    targets: [
      {
        reason: { code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING },
        status: UNKNOWN,
        target: mac.placement,
      },
      { status: RUNS, target: spark.placement },
      { status: RUNS, target: ws1.placement },
      { status: RUNS, target: ws2.placement },
      { status: RUNS, target: ws3.placement },
    ],
  }),
);

describe("profile compatibility", () => {
  it("reads a profile's entry on a target that passes target-level checks", () => {
    expect(verdictOn("q4", spark)).toEqual({
      advisories: [LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM],
      reason: undefined,
      status: "runs",
      target: spark.placement,
    });
    expect(verdictOn("ninfer", ws2)?.status).toBe("cannot-run");
    expect(verdictOn("ninfer", ws2)?.reason?.code).toBe(
      ModelCompatibilityReasonCode.NVIDIA_DRIVER_BELOW_MINIMUM,
    );
  });

  it("gives every profile the target's own verdict on a target that fails target-level checks", () => {
    for (const profileId of ["q4", "ninfer", "apple"]) {
      expect(verdictOn(profileId, mac)).toEqual({
        advisories: [],
        reason: expect.objectContaining({
          code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING,
        }) as ModelCompatibilityReason,
        status: "unknown",
        target: mac.placement,
      });
    }
  });

  it("knows nothing about a target or profile the read didn't evaluate", () => {
    const other = target("ws-9", "gpu0");

    expect(verdictOn("q4", other)).toBeUndefined();
    expect(verdictOn("missing", ws1)).toBeUndefined();
    expect(verdictOn("q4", { placement: { ...ws1.placement, siteNodeId: "ws-2" } })).toEqual(
      verdictOn("q4", ws2),
    );
  });

  it("orders targets as the read does", () => {
    expect(compatibility.targets).toEqual([
      mac.placement,
      spark.placement,
      ws1.placement,
      ws2.placement,
      ws3.placement,
    ]);
  });

  it("takes each profile's sizing-derived parameters from its estimates' derived values", () => {
    const derivedValue = (parameterId: string) => ({
      parameterId,
      value: { case: "unsignedInteger" as const, value: 1n },
    });
    const { derivedParameterIds } = indexProfileCompatibility(
      create(ListModelProfileCompatibilityResponseSchema, {
        profiles: [
          {
            hardwareClassEstimates: [
              {
                derivedEngineParameterValues: {
                  values: [derivedValue("context-per-request"), derivedValue("expert-vram-mib")],
                },
                hardwareClassId: "nvidia-gb202",
              },
              { derivedEngineParameterValues: { values: [] }, hardwareClassId: "nvidia" },
            ],
            modelId: "lab",
            profileId: "strata",
          },
          profileSizing("lab", "llama", { acceleratorMemoryBytes: 1n, hardwareClassId: "nvidia" }),
          profileSizing("lab", "unsized"),
        ],
      }),
    );

    expect(new Map([...derivedParameterIds].map(([key, ids]) => [key, [...ids]]))).toEqual(
      new Map([
        [profileEstimateKey("lab", "strata"), ["expert-vram-mib"]],
        [profileEstimateKey("lab", "llama"), []],
      ]),
    );
  });

  it("keeps each profile's per-class estimates, with or without a reporting target", () => {
    const { estimates } = indexProfileCompatibility(
      create(ListModelProfileCompatibilityResponseSchema, {
        profiles: [
          profileSizing(
            "lab",
            "q4",
            { acceleratorMemoryBytes: 7_500_000_000n, hardwareClassId: "nvidia" },
            {
              acceleratorMemoryBytes: 6_000_000_000n,
              hardwareClassId: "apple",
              hostMemoryBytes: 2_000_000_000n,
            },
          ),
          // A class without accelerator memory is skipped, and a profile left with none is omitted.
          profileSizing(
            "lab",
            "ninfer",
            { hardwareClassId: "nvidia", hostMemoryBytes: 1n },
            { acceleratorMemoryBytes: 30_000_000_000n, hardwareClassId: "nvidia-unified" },
          ),
          profileSizing("lab", "apple", { hardwareClassId: "apple", hostMemoryBytes: 1n }),
          profileSizing("lab", "unsized"),
        ],
        targets: [
          {
            reason: { code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING },
            status: UNKNOWN,
            target: mac.placement,
          },
        ],
      }),
    );

    expect([...estimates]).toEqual([
      [
        profileEstimateKey("lab", "q4"),
        [
          {
            acceleratorMemoryBytes: 7_500_000_000n,
            hardwareClassId: "nvidia",
            hostMemoryBytes: undefined,
          },
          {
            acceleratorMemoryBytes: 6_000_000_000n,
            hardwareClassId: "apple",
            hostMemoryBytes: 2_000_000_000n,
          },
        ],
      ],
      [
        profileEstimateKey("lab", "ninfer"),
        [
          {
            acceleratorMemoryBytes: 30_000_000_000n,
            hardwareClassId: "nvidia-unified",
            hostMemoryBytes: undefined,
          },
        ],
      ],
    ]);
  });

  describe("scope derivation", () => {
    it("runs on the site when one target runs it, preferring one without advisories", () => {
      expect(siteVerdict("q4")).toMatchObject({ status: "runs", target: ws1.placement });
      expect(siteVerdict("ninfer")).toMatchObject({ status: "runs", target: ws1.placement });
    });

    it("runs with an advisory when every target that runs it has one", () => {
      const scoped = profileScopeVerdict(compatibility, "model", "q4", {
        kind: "node",
        siteNodeId: "spark",
      });

      expect(scoped).toMatchObject({ status: "runs", target: spark.placement });
      expect(scoped?.advisories).toEqual([
        LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
      ]);
    });

    it("is unknown when no target runs it and one is unknown", () => {
      expect(siteVerdict("apple")).toMatchObject({ status: "unknown", target: mac.placement });
    });

    it("can't run when every target in scope can't run it", () => {
      const scoped = profileScopeVerdict(compatibility, "model", "ninfer", {
        kind: "node",
        siteNodeId: "ws-3",
      });

      expect(scoped).toMatchObject({ status: "cannot-run", target: ws3.placement });
    });

    it("reads nothing in an empty scope", () => {
      expect(
        profileScopeVerdict(compatibility, "model", "q4", { kind: "node", siteNodeId: "ws-9" }),
      ).toBeUndefined();
      expect(
        profileScopeVerdict(
          indexProfileCompatibility(create(ListModelProfileCompatibilityResponseSchema)),
          "model",
          "q4",
          {
            kind: "site",
          },
        ),
      ).toBeUndefined();
    });

    it("keys each scope for the Runs on filter", () => {
      expect(runsOnScopeKey({ kind: "site" })).toBe("site");
      expect(runsOnScopeKey({ kind: "node", siteNodeId: "ws-1" })).toBe("node:ws-1");
    });
  });
});

describe("profile compatibility copy", () => {
  it.each<[string, ReasonInit, string, Partial<ProfileRow>?]>([
    [
      "target not reporting",
      { code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING },
      "Workstation isn't reporting",
    ],
    [
      "NVIDIA driver unknown",
      { code: ModelCompatibilityReasonCode.NVIDIA_DRIVER_UNKNOWN },
      "Workstation's NVIDIA driver version is unknown",
    ],
    [
      "accelerator memory unknown",
      { code: ModelCompatibilityReasonCode.ACCELERATOR_MEMORY_UNKNOWN },
      "Workstation's GPU memory type is unknown",
    ],
    [
      "target platform invalid",
      { code: ModelCompatibilityReasonCode.TARGET_PLATFORM_INVALID },
      "Workstation / NVIDIA GeForce RTX 5090 has no supported platform configured",
    ],
    [
      "platform mismatch",
      {
        code: ModelCompatibilityReasonCode.PLATFORM_MISMATCH,
        configuredPlatform: { architecture: "amd64", operatingSystem: "linux" },
        observedPlatform: { architecture: "arm64", operatingSystem: "linux", variant: "v8" },
      },
      "Workstation reports linux/arm64/v8, not the configured linux/amd64",
    ],
    [
      "no accelerator",
      { code: ModelCompatibilityReasonCode.NO_ACCELERATOR },
      "Workstation reports no usable GPU",
    ],
    [
      "no engine artifact for the target's platform",
      {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
        supportedSelectors: [
          {
            accelerators: ["apple"],
            platform: { architecture: "arm64", operatingSystem: "darwin" },
          },
        ],
      },
      "Needs darwin/arm64",
    ],
    [
      "no engine artifact on several platforms",
      {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
        supportedSelectors: [
          {
            accelerators: ["nvidia"],
            platform: { architecture: "arm64", operatingSystem: "linux" },
          },
          {
            accelerators: ["nvidia"],
            platform: { architecture: "arm64", operatingSystem: "linux" },
          },
          {
            accelerators: ["apple"],
            platform: { architecture: "arm64", operatingSystem: "darwin" },
          },
        ],
      },
      "Needs linux/arm64 or darwin/arm64",
    ],
    [
      "no engine artifact for the target's accelerator",
      {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
        supportedSelectors: [
          {
            accelerators: ["nvidia"],
            platform: { architecture: "amd64", operatingSystem: "linux" },
          },
        ],
      },
      "Needs an NVIDIA GPU",
    ],
    [
      "no engine artifact for the accelerator of a target matching an artifact's platform variant",
      {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
        supportedSelectors: [
          {
            accelerators: ["apple"],
            platform: { architecture: "amd64", operatingSystem: "linux", variant: "v3" },
          },
        ],
      },
      "Needs Apple silicon",
    ],
    [
      "no engine artifact for an Apple or other accelerator",
      {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
        supportedSelectors: [
          {
            accelerators: ["apple"],
            platform: { architecture: "amd64", operatingSystem: "linux" },
          },
          { accelerators: ["amd"], platform: { architecture: "amd64", operatingSystem: "linux" } },
        ],
      },
      "Needs Apple silicon or an accelerator from amd",
    ],
    [
      "no engine artifact for a selector listing several accelerators",
      {
        code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT,
        supportedSelectors: [
          {
            accelerators: ["nvidia", "apple"],
            platform: { architecture: "amd64", operatingSystem: "linux" },
          },
        ],
      },
      "Needs an NVIDIA GPU or Apple silicon",
    ],
    [
      "no engine build for the target's runtime",
      { code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT, supportedSelectors: [] },
      "No llama.cpp build for this target's runtime",
    ],
    [
      "no engine build for the target's runtime on another platform",
      { code: ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT, supportedSelectors: [] },
      "Needs darwin/arm64",
      { platforms: [{ architecture: "arm64", operatingSystem: "darwin" }] },
    ],
    [
      "no supported hardware class",
      { code: ModelCompatibilityReasonCode.NO_SUPPORTED_HARDWARE_CLASS },
      "Not supported on Workstation's hardware",
    ],
    [
      "NVIDIA driver below minimum",
      {
        code: ModelCompatibilityReasonCode.NVIDIA_DRIVER_BELOW_MINIMUM,
        minimumNvidiaDriverMajorVersion: 575,
        observedNvidiaDriverMajorVersion: 560,
      },
      "Needs NVIDIA driver 575 or later (has 560)",
    ],
    [
      "more accelerator devices than the launch uses",
      {
        acceleratorDeviceCount: 2,
        code: ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH,
        requiredAcceleratorDeviceCount: 1,
      },
      "Needs 1 GPU, target has 2",
    ],
    [
      "a target without accelerator devices",
      {
        code: ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH,
        requiredAcceleratorDeviceCount: 1,
      },
      "Needs 1 GPU, target has 0",
    ],
    [
      "fewer accelerator devices than a multi-GPU launch uses",
      {
        acceleratorDeviceCount: 1,
        code: ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH,
        requiredAcceleratorDeviceCount: 2,
      },
      "Needs 2 GPUs, target has 1",
    ],
    [
      "profile unresolvable",
      { code: ModelCompatibilityReasonCode.PROFILE_UNRESOLVABLE },
      "Its launch settings don't resolve on Workstation",
    ],
    [
      "unspecified reason",
      { code: ModelCompatibilityReasonCode.UNSPECIFIED },
      "Can't run on Workstation",
    ],
  ])("words %s", (_, reason, text, profileValues = {}) => {
    expect(reasonText("cannot-run", reason, profileValues)).toBe(text);
  });

  it("words a missing reason by status", () => {
    expect(reasonText("unknown", undefined)).toBe("Can't check Workstation right now");
    expect(reasonText("cannot-run", undefined)).toBe("Can't run on Workstation");
  });

  it("names an unconfigured machine and target by their IDs", () => {
    const verdict: CompatibilityVerdict = {
      advisories: [],
      reason: create(ModelCompatibilityReasonSchema, {
        code: ModelCompatibilityReasonCode.TARGET_PLATFORM_INVALID,
      }),
      status: "cannot-run",
      target: { executionTargetId: "gpu0", siteNodeId: "ws-9" },
    };

    expect(compatibilityReasonText(verdict, { profile: profile(), targets: [] })).toBe(
      "ws-9 / gpu0 has no supported platform configured",
    );
  });

  it("words every advisory from the profile's declared minimum", () => {
    const sized = profile({
      acceleratorRequirements: [
        {
          displayName: "NVIDIA",
          hardwareClassId: "nvidia",
          minimumMemoryBytesPerDevice: 16 * 1024 ** 3,
          vendors: ["nvidia"],
        },
        {
          displayName: "NVIDIA",
          hardwareClassId: "nvidia",
          minimumMemoryBytesPerDevice: 24 * 1024 ** 3,
          vendors: ["nvidia"],
        },
      ],
    });

    expect(
      compatibilityAdvisoryText(
        LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
        sized,
      ),
    ).toBe("System memory is below this profile's 24 GiB minimum");
    expect(
      compatibilityAdvisoryText(
        LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
        profile({ acceleratorRequirements: [] }),
      ),
    ).toBe("System memory is below this profile's minimum");
    expect(
      compatibilityAdvisoryText(LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY, sized),
    ).toBe("KV cache offload adds no capacity on this target");
    expect(
      compatibilityAdvisoryText(LocalModelDeploymentAdvisory.UNSPECIFIED, sized),
    ).toBeUndefined();
  });

  it("tones the status line by verdict, with no line for a profile that just runs", () => {
    const context = { profile: profile(), targets: [workstation] };
    const on = { executionTargetId: "gpu0", siteNodeId: "ws-1" };

    expect(
      profileCompatibilityStatus({ advisories: [], status: "runs", target: on }, context),
    ).toBeUndefined();
    expect(profileCompatibilityStatus(undefined, context)).toBeUndefined();
    expect(
      profileCompatibilityStatus(
        {
          advisories: [
            LocalModelDeploymentAdvisory.UNSPECIFIED,
            LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY,
            LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM,
          ],
          status: "runs",
          target: on,
        },
        context,
      ),
    ).toEqual({
      text: "KV cache offload adds no capacity on this target; System memory is below this profile's 7.5 GiB minimum",
      tone: "advisory",
    });
    expect(
      profileCompatibilityStatus(
        {
          advisories: [],
          reason: create(ModelCompatibilityReasonSchema, {
            code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING,
          }),
          status: "unknown",
          target: on,
        },
        context,
      ),
    ).toEqual({ text: "Workstation isn't reporting", tone: "unknown" });
    expect(
      profileCompatibilityStatus(
        {
          advisories: [],
          reason: create(ModelCompatibilityReasonSchema, {
            code: ModelCompatibilityReasonCode.NO_ACCELERATOR,
          }),
          status: "cannot-run",
          target: on,
        },
        context,
      ),
    ).toEqual({ text: "Workstation reports no usable GPU", tone: "blocked" });
  });
});

const workstation: ConfiguredExecutionTarget = create(ConfiguredExecutionTargetSchema, {
  displayName: "NVIDIA GeForce RTX 5090",
  executionTargetId: "gpu0",
  platformLabel: "linux/amd64",
  siteNodeDisplayName: "Workstation",
  siteNodeId: "ws-1",
  workloadKind: ExecutionTargetWorkloadKind.MODEL,
});

function reasonText(
  status: CompatibilityVerdict["status"],
  reason: ReasonInit | undefined,
  profileValues: Partial<ProfileRow> = {},
): string {
  return compatibilityReasonText(
    {
      advisories: [],
      reason: reason && create(ModelCompatibilityReasonSchema, reason),
      status,
      target: { executionTargetId: "gpu0", siteNodeId: "ws-1" },
    },
    { profile: profile({ acceleratorRequirements: [], ...profileValues }), targets: [workstation] },
  );
}

function profile(values: Partial<ProfileRow> = {}): ProfileRow {
  const { engineReleases, hardwareClasses, models } = syntheticCatalog();
  return {
    ...projectProfileRows(models[0], { engineReleases, estimates: new Map(), hardwareClasses })[0],
    ...values,
  };
}

function verdictOn(profileId: string, on: { placement: TargetIdentity }) {
  return profileTargetVerdict(compatibility, "model", profileId, on.placement);
}

function siteVerdict(profileId: string) {
  return profileScopeVerdict(compatibility, "model", profileId, { kind: "site" });
}

function target(siteNodeId: string, executionTargetId: string) {
  return { placement: { executionTargetId, siteNodeId } };
}

function entry(
  profileId: string,
  on: { placement: TargetIdentity },
  status: ModelCompatibilityStatus,
  reason?: ReasonInit,
  advisories: LocalModelDeploymentAdvisory[] = [],
) {
  return { advisories, modelId: "model", profileId, reason, status, target: on.placement };
}
