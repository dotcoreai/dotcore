import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { ParametersForm } from "@/components/parameters/parameters";

import { KvCachePrecisionField } from "./kv-cache-precision-field";
import { SpeculativeDecodingField } from "./speculative-decoding-field";
import { VisionField } from "./vision-field";

describe("model option fields", () => {
  it("render like an engine parameter field: no box, the same field structure", () => {
    const noop = () => {};
    render(
      <>
        <ParametersForm
          errors={new Map()}
          groups={[]}
          idPrefix="test"
          inputRefs={{ current: new Map() }}
          inputs={new Map([["flash", "true"]])}
          onBlur={noop}
          onChange={noop}
          parameters={[{ id: "flash", label: "Flash attention", type: { kind: "boolean" } }]}
        />
        <VisionField
          disabled={false}
          enabled
          onChange={noop}
          option={{
            defaultEnabled: true,
            description: "",
            displayName: "Vision",
            optionId: "vision",
          }}
        />
        <KvCachePrecisionField
          disabled={false}
          onChange={noop}
          option={{
            choices: ["FP16", "FP8"],
            defaultPrecision: "FP16",
            description: "",
            displayName: "KV cache precision",
            optionId: "kv",
          }}
          precision="FP16"
        />
        <SpeculativeDecodingField
          choice={{ count: "2", optionId: "mtp" }}
          countRef={{ current: null }}
          disabled={false}
          onCountBlur={noop}
          onCountChange={noop}
          onMethodChange={noop}
          options={[
            { defaultCount: 2, displayName: "MTP", maximum: 5, minimum: 1, optionId: "mtp" },
          ]}
        />
      </>,
    );

    const fields = [
      screen.getByRole("switch", { name: "Flash attention" }),
      screen.getByRole("switch", { name: "Vision" }),
      screen.getByRole("combobox", { name: "KV cache precision" }),
      screen.getByRole("combobox", { name: "Speculative decoding" }),
    ].map((control) => control.closest("[data-slot=field]")?.className);
    expect(fields).toEqual(["grid gap-1", "grid gap-1", "grid gap-1", "grid gap-1"]);
  });
});
