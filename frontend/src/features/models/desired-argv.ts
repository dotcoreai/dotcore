import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

/** The argv tokens of a snapshot's desired launch plan, with runtime-resolved values shown as placeholders. */
export function desiredArgv(snapshot: ResolvedLocalModelDeploymentSnapshot): ArgumentToken[] {
  return (snapshot.desiredLaunchPlan?.arguments ?? []).map((token) => {
    switch (token.value.case) {
      case "literal":
        return { text: token.value.value, placeholder: false };
      case "artifactBindingId":
        return { text: `{artifact:${token.value.value}}`, placeholder: true };
      case "engineExecutable":
        return { text: "{engine.executable}", placeholder: true };
      case "serviceHost":
        return { text: `{service:${token.value.value}.host}`, placeholder: true };
      case "servicePort":
        return { text: `{service:${token.value.value}.port}`, placeholder: true };
      default:
        return { text: "<missing argument>", placeholder: true };
    }
  });
}

/** One argv entry: an exact literal, or a placeholder the runtime fills in at launch. */
export interface ArgumentToken {
  text: string;
  placeholder: boolean;
}
