import { describe, expect, it } from "vitest";

import { getDevServerConfig } from "../../tools/vite-dev-server";

const secondaryEnvironment = {
  DOTCORE_MACHINE_HOST: "100.64.0.7",
  DOTCORE_FRONTEND_PORT: "5207",
  DOTCORE_FRONTEND_ORIGIN: "http://100.64.0.7:5207",
  DOTCORE_DEV_PUBLIC_PORT: "5212",
  DOTCORE_PUBLIC_ORIGIN: "http://100.64.0.7:5212",
};

describe("Vite development server configuration", () => {
  it("uses canonical loopback endpoints without direnv", () => {
    const server = getDevServerConfig({});

    expect(server).toMatchObject({
      host: "127.0.0.1",
      port: 5173,
      origin: "http://127.0.0.1:5173",
      strictPort: true,
    });
    expect(server.ws).toEqual({ clientPort: 5173 });
  });

  it("listens on the frontend port and serves assets and HMR through the public ingress", () => {
    const server = getDevServerConfig(secondaryEnvironment);

    expect(server).toMatchObject({
      host: "100.64.0.7",
      port: 5207,
      origin: "http://100.64.0.7:5212",
      strictPort: true,
      allowedHosts: [],
    });
    // The HMR websocket shares the Vite server; the browser dials it through the ingress port.
    expect(server.ws).toEqual({ clientPort: 5212 });
  });

  it("accepts a DNS host name", () => {
    const server = getDevServerConfig({
      DOTCORE_MACHINE_HOST: "node-1.example.ts.net",
      DOTCORE_FRONTEND_PORT: "5207",
      DOTCORE_FRONTEND_ORIGIN: "http://node-1.example.ts.net:5207",
      DOTCORE_DEV_PUBLIC_PORT: "5212",
      DOTCORE_PUBLIC_ORIGIN: "http://node-1.example.ts.net:5212",
    });

    expect(server).toMatchObject({
      host: "node-1.example.ts.net",
      port: 5207,
      origin: "http://node-1.example.ts.net:5212",
      ws: { clientPort: 5212 },
      allowedHosts: ["node-1.example.ts.net"],
    });
  });

  it("rejects a partial ambient endpoint configuration", () => {
    expect(() => getDevServerConfig({ DOTCORE_MACHINE_HOST: "100.64.0.7" })).toThrow(
      "complete configuration",
    );
    expect(() =>
      getDevServerConfig({ ...secondaryEnvironment, DOTCORE_PUBLIC_ORIGIN: undefined }),
    ).toThrow("'DOTCORE_PUBLIC_ORIGIN' is missing");
  });

  it.each([
    ["wildcard host", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "0.0.0.0" }],
    ["malformed host", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "100.64.0" }],
    [
      "hex address host",
      {
        ...secondaryEnvironment,
        DOTCORE_MACHINE_HOST: "0x7f000001",
        DOTCORE_FRONTEND_ORIGIN: "http://0x7f000001:5207",
      },
    ],
    [
      "hex zero host",
      {
        ...secondaryEnvironment,
        DOTCORE_MACHINE_HOST: "0x0",
        DOTCORE_FRONTEND_ORIGIN: "http://0x0:5207",
      },
    ],
    [
      "hex last label",
      {
        ...secondaryEnvironment,
        DOTCORE_MACHINE_HOST: "1.0x2",
        DOTCORE_FRONTEND_ORIGIN: "http://1.0x2:5207",
      },
    ],
    ["IPv6 literal", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "::1" }],
    ["uppercase host name", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "Node-1.example" }],
    [
      "trailing-dot host name",
      { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "node-1.example." },
    ],
    ["leading-hyphen label", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "-node.example" }],
    ["trailing-hyphen label", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "node-.example" }],
    ["underscore label", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "node_1.example" }],
    ["empty label", { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: "node..example" }],
    [
      "64-character label",
      { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: `${"a".repeat(64)}.example` },
    ],
    [
      "254-character host name",
      { ...secondaryEnvironment, DOTCORE_MACHINE_HOST: `${"a.".repeat(126)}bc` },
    ],
    ["NaN frontend port", { ...secondaryEnvironment, DOTCORE_FRONTEND_PORT: "not-a-number" }],
    ["out-of-range frontend port", { ...secondaryEnvironment, DOTCORE_FRONTEND_PORT: "70000" }],
    [
      "mismatched frontend origin",
      { ...secondaryEnvironment, DOTCORE_FRONTEND_ORIGIN: "http://100.64.0.7:5208" },
    ],
    ["NaN public port", { ...secondaryEnvironment, DOTCORE_DEV_PUBLIC_PORT: "not-a-number" }],
    [
      "mismatched public origin",
      { ...secondaryEnvironment, DOTCORE_PUBLIC_ORIGIN: "http://100.64.0.7:5213" },
    ],
    [
      "public origin on another host",
      { ...secondaryEnvironment, DOTCORE_PUBLIC_ORIGIN: "http://100.64.0.8:5212" },
    ],
  ])("rejects %s", (_, environment) => {
    expect(() => getDevServerConfig(environment)).toThrow("Invalid Dotcore development endpoint");
  });

  it("does not opt into wildcard hosts or permissive CORS", () => {
    const server = getDevServerConfig({});

    expect(server.host).not.toBe(true);
    expect(server.allowedHosts).not.toBe(true);
    expect(server.cors).toBe(false);
    expect(server.allowedHosts ?? []).not.toContain("*");
  });
});
