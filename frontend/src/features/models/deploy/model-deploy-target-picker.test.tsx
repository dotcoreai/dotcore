import { create } from "@bufbuild/protobuf";
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import {
  ConfiguredExecutionTargetSchema,
  ExecutionTargetWorkloadKind,
  NodeObservationState,
} from "@/gen/dotcore/site/v1/service_pb";

import { ModelDeployTargetPicker } from "./model-deploy-target-picker";

describe("ModelDeployTargetPicker", () => {
  it("names the only machine with why it can't run the profile, and hands it the focus", () => {
    const inputRef = { current: null as HTMLElement | null };
    render(
      <ModelDeployTargetPicker
        disabled={false}
        error="Execution target is required."
        executionTargets={[onlyTarget()]}
        inputRef={inputRef}
        loading={false}
        loadingMessageVisible={false}
        loadingPresentation={false}
        onBlur={() => {}}
        onSelect={() => {}}
        placement={null}
        targetsCurrent
        unavailableReason={() => "Needs NVIDIA driver 575 or later (has 560)"}
      />,
    );

    const notice = screen.getByText(
      "node / NVIDIA GeForce RTX 5090 — Needs NVIDIA driver 575 or later (has 560)",
    );
    expect(screen.queryByText("Execution target is required.")).not.toBeInTheDocument();
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();

    // A blocked Next moves focus to the control the ref holds.
    inputRef.current?.focus();
    expect(notice.closest("[tabindex='-1']")).toHaveFocus();
  });
});

function onlyTarget() {
  return create(ConfiguredExecutionTargetSchema, {
    displayName: "NVIDIA GeForce RTX 5090",
    executionTargetId: "pool",
    siteNodeId: "node",
    siteNodeDisplayName: "node",
    workloadKind: ExecutionTargetWorkloadKind.MODEL,
    siteManager: true,
    availability: {
      connectionState: NodeObservationState.CONNECTED,
      runtimeAvailability: { availability: { case: "available", value: {} } },
    },
  });
}
