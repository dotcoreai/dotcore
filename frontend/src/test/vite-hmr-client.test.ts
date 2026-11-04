import { createServer } from "vite";
import { describe, expect, it } from "vitest";

import { getDevServerConfig } from "../../tools/vite-dev-server";

describe("Vite HMR client endpoint", () => {
  it("dials the public ingress port on the page host, falling back to the frontend port", async () => {
    const serverConfig = getDevServerConfig({
      DOTCORE_MACHINE_HOST: "100.64.0.7",
      DOTCORE_FRONTEND_PORT: "5207",
      DOTCORE_FRONTEND_ORIGIN: "http://100.64.0.7:5207",
      DOTCORE_DEV_PUBLIC_PORT: "5212",
      DOTCORE_PUBLIC_ORIGIN: "http://100.64.0.7:5212",
    });
    const server = await createServer({ configFile: false, server: serverConfig });

    try {
      const transformedClient = await server.transformRequest("/@vite/client");

      expect(transformedClient?.code).toContain('const serverHost = "100.64.0.7:5207/";');
      expect(transformedClient?.code).toContain("const hmrPort = 5212;");
      expect(transformedClient?.code).toContain(
        "const socketHost = `${null || importMetaUrl.hostname}",
      );
      expect(transformedClient?.code).toContain('const directSocketHost = "100.64.0.7:5207/";');
    } finally {
      await server.close();
    }
  });
});
