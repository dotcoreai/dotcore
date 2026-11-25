import { KvCachePrecisionField } from "@/features/models/runtime/kv-cache-precision-field";
import { SpeculativeDecodingField } from "@/features/models/runtime/speculative-decoding-field";
import { VisionField } from "@/features/models/runtime/vision-field";
import type { FieldErrors } from "@/lib/form-validation";

import type { ModelOptionControl, useModelDeployOptions } from "./use-model-deploy-options";

/** The given model option controls, in their order, as one step shows them. */
export function ModelDeployOptionFields({
  controls,
  disabled,
  fieldErrors,
  options,
}: {
  controls: readonly ModelOptionControl[];
  disabled: boolean;
  fieldErrors: FieldErrors;
  options: ReturnType<typeof useModelDeployOptions>;
}) {
  const { kvCachePrecision, speculative, vision } = options;

  return controls.map((control) => {
    switch (control) {
      case "vision":
        return vision.option ? (
          <VisionField
            disabled={disabled}
            enabled={vision.enabled}
            key={control}
            onChange={vision.onChange}
            option={vision.option}
          />
        ) : null;
      case "kvCachePrecision":
        return kvCachePrecision.option ? (
          <KvCachePrecisionField
            disabled={disabled}
            key={control}
            onChange={kvCachePrecision.onChange}
            option={kvCachePrecision.option}
            precision={kvCachePrecision.precision}
          />
        ) : null;
      case "speculative":
        return (
          <SpeculativeDecodingField
            choice={speculative.choice}
            countRef={speculative.countRef}
            description={speculative.description}
            disabled={disabled}
            error={fieldErrors.get("speculativeCount")}
            key={control}
            onCountBlur={speculative.onCountBlur}
            onCountChange={speculative.onCountChange}
            onMethodChange={speculative.onMethodChange}
            options={speculative.options}
          />
        );
    }
  });
}
