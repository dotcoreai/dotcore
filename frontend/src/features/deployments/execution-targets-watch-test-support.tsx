/* eslint-disable react-refresh/only-export-components -- test support exports a wrapper and its helpers */
import { create } from "@bufbuild/protobuf";
import type { ReactNode } from "react";

import {
  type ConfiguredExecutionTarget,
  ListConfiguredExecutionTargetsResponseSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { ResourceWatchProvider } from "@/resource-watch/resource-watch-provider";
import { FakeResourceWatch } from "@/test/fake-resource-watch";

/** An execution targets read, as a deploy dialog's loader answers it. */
export type ExecutionTargetsRead = () => Promise<{
  executionTargets: ConfiguredExecutionTarget[];
}>;

/** A resource watch answering the execution targets from `read`; none without one. */
export function executionTargetsWatch(read?: ExecutionTargetsRead): FakeResourceWatch {
  return new FakeResourceWatch({
    sources: read
      ? {
          executionTargets: async () =>
            create(ListConfiguredExecutionTargetsResponseSchema, await read()),
        }
      : {},
  });
}

/** Provides the resource watch a deploy dialog reads its execution targets from. */
export function ExecutionTargetsWatch({
  children,
  watch,
}: {
  children: ReactNode;
  watch: FakeResourceWatch;
}) {
  return <ResourceWatchProvider client={watch.client()}>{children}</ResourceWatchProvider>;
}
