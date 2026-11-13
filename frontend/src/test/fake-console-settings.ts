import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";

import type { ResourceWatchClient } from "@/api/resource-watch-client";
import type { SiteClient } from "@/api/site-client";
import {
  GetConsoleSettingsResponseSchema,
  type UpdateConsoleSettingsRequest,
  UpdateConsoleSettingsRequestSchema,
  UpdateConsoleSettingsResponseSchema,
} from "@/gen/dotcore/site/v1/service_pb";
import { ConsoleSettingsSchema } from "@/gen/dotcore/site/v1/site_pb";

import { FakeResourceWatch } from "./fake-resource-watch";

/**
 * The site's console settings as the manager keeps them: a watch that reports them each tick and
 * an update call that saves them, recording every request. Updates reach the watch on its next
 * `tick`, like the manager's.
 */
export class FakeConsoleSettings {
  getStartedHidden: boolean;
  readonly updates: UpdateConsoleSettingsRequest[] = [];
  /** Makes the next updates fail with an unavailable error while set. */
  failing = false;
  readonly watch: FakeResourceWatch;

  constructor(getStartedHidden = false) {
    this.getStartedHidden = getStartedHidden;
    this.watch = new FakeResourceWatch({
      sources: {
        consoleSettings: () =>
          Promise.resolve(
            create(GetConsoleSettingsResponseSchema, {
              settings: { getStartedHidden: this.getStartedHidden },
            }),
          ),
      },
    });
  }

  resourceWatchClient(): ResourceWatchClient {
    return this.watch.client();
  }

  updateConsoleSettings: SiteClient["updateConsoleSettings"] = (request) => {
    this.updates.push(create(UpdateConsoleSettingsRequestSchema, request));
    if (this.failing) {
      return Promise.reject(new ConnectError("saving is uncertain", Code.Unavailable));
    }
    this.getStartedHidden = request.settings?.getStartedHidden ?? false;

    return Promise.resolve(
      create(UpdateConsoleSettingsResponseSchema, {
        settings: create(ConsoleSettingsSchema, { getStartedHidden: this.getStartedHidden }),
      }),
    );
  };
}
