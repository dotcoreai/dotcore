import type { UserConfig } from "vite";

const canonicalDevHost = "127.0.0.1";
const fallbackFrontendPort = 5173;

type Environment = Partial<
  Pick<
    NodeJS.ProcessEnv,
    | "DOTCORE_MACHINE_HOST"
    | "DOTCORE_FRONTEND_PORT"
    | "DOTCORE_FRONTEND_ORIGIN"
    | "DOTCORE_DEV_PUBLIC_PORT"
    | "DOTCORE_PUBLIC_ORIGIN"
  >
>;

/**
 * Returns the Vite development server settings for the workspace endpoints. Vite listens on the
 * frontend port, while browsers load the console, its assets and the HMR websocket through the
 * public ingress origin, which proxies to Vite.
 */
export function getDevServerConfig(
  environment: Environment = process.env,
): NonNullable<UserConfig["server"]> {
  const endpoint = validateEnvironment(environment);

  return {
    host: endpoint.host,
    port: endpoint.frontendPort,
    strictPort: true,
    origin: endpoint.publicOrigin,
    cors: false,
    // Vite allows IP and localhost Host headers by default; a DNS name must be listed explicitly.
    allowedHosts: isIpv4(endpoint.host) ? [] : [endpoint.host],
    // The websocket shares the Vite server; the browser dials it on the page host and public port.
    ws: {
      clientPort: endpoint.publicPort,
    },
  };
}

type ValidatedEnvironment = {
  host: string;
  frontendPort: number;
  publicPort: number;
  publicOrigin: string;
};

const environmentVariables = [
  "DOTCORE_MACHINE_HOST",
  "DOTCORE_FRONTEND_PORT",
  "DOTCORE_FRONTEND_ORIGIN",
  "DOTCORE_DEV_PUBLIC_PORT",
  "DOTCORE_PUBLIC_ORIGIN",
] as const satisfies ReadonlyArray<keyof Environment>;

function validateEnvironment(environment: Environment): ValidatedEnvironment {
  const hasAmbientEndpoint = environmentVariables.some(
    (variable) => environment[variable] !== undefined,
  );
  if (!hasAmbientEndpoint) {
    // Without the workspace environment no ingress runs, so browsers reach Vite directly.
    return {
      host: canonicalDevHost,
      frontendPort: fallbackFrontendPort,
      publicPort: fallbackFrontendPort,
      publicOrigin: `http://${canonicalDevHost}:${fallbackFrontendPort}`,
    };
  }

  const values = Object.fromEntries(
    environmentVariables.map((variable) => [
      variable,
      requiredEnvironmentValue(environment, variable),
    ]),
  ) as Record<(typeof environmentVariables)[number], string>;
  const host = validateHost(values.DOTCORE_MACHINE_HOST);
  const frontendPort = parsePort(values.DOTCORE_FRONTEND_PORT, "DOTCORE_FRONTEND_PORT");
  const frontendOrigin = `http://${host}:${frontendPort}`;
  if (values.DOTCORE_FRONTEND_ORIGIN !== frontendOrigin) {
    throw invalidEndpoint(`DOTCORE_FRONTEND_ORIGIN must equal '${frontendOrigin}'.`);
  }
  const publicPort = parsePort(values.DOTCORE_DEV_PUBLIC_PORT, "DOTCORE_DEV_PUBLIC_PORT");
  const publicOrigin = `http://${host}:${publicPort}`;
  if (values.DOTCORE_PUBLIC_ORIGIN !== publicOrigin) {
    throw invalidEndpoint(`DOTCORE_PUBLIC_ORIGIN must equal '${publicOrigin}'.`);
  }

  return { host, frontendPort, publicPort, publicOrigin };
}

function requiredEnvironmentValue(environment: Environment, variable: keyof Environment): string {
  const value = environment[variable];
  if (value === undefined || value === "") {
    throw invalidEndpoint(
      `a complete configuration must define ${environmentVariables.join(", ")} when any endpoint variable is set; '${variable}' is missing.`,
    );
  }
  return value;
}

// Mirrors the backend node host type: an IPv4 literal or an RFC 1123 DNS name in canonical form
// (lowercase, no trailing dot). Non-canonical values are rejected, as `site.yaml` validation does.
function validateHost(host: string): string {
  if (isIpv4(host)) {
    if (host === "0.0.0.0") {
      throw invalidHost();
    }
    return host;
  }
  if (!isDnsName(host)) {
    throw invalidHost();
  }
  return host;
}

function isIpv4(host: string): boolean {
  const octets = host.split(".");
  return (
    octets.length === 4 &&
    octets.every((octet) => /^(0|[1-9]\d{0,2})$/.test(octet) && Number(octet) <= 255)
  );
}

function isDnsName(host: string): boolean {
  if (host.length > 253) {
    return false;
  }
  const labels = host.split(".");
  const validLabels = labels.every((label) => /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/.test(label));
  // A numeric or 0x-hex last label would make a malformed IPv4 literal (ex: "100.64.0", or
  // "0x7f000001", which the system resolver reads as 127.0.0.1) look like a name.
  return validLabels && !/^(\d+|0x[0-9a-f]*)$/.test(labels[labels.length - 1]);
}

function invalidHost(): Error {
  return invalidEndpoint(
    "DOTCORE_MACHINE_HOST must be a specific IPv4 literal or a lowercase DNS name without trailing dot, not 0.0.0.0 or an IPv6 literal.",
  );
}

function parsePort(value: string, variable: string): number {
  const port = Number(value);
  if (!/^(0|[1-9]\d*)$/.test(value) || !Number.isInteger(port) || port < 1 || port > 65535) {
    throw invalidEndpoint(`${variable} must be an integer TCP port from 1 to 65535.`);
  }
  return port;
}

function invalidEndpoint(message: string): Error {
  return new Error(`Invalid Dotcore development endpoint environment: ${message}`);
}
