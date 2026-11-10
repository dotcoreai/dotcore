import { CodeCopyControl } from "@/components/ui/copy-control";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { weightQuantizationLabel } from "@/domain/model-projection";
import {
  ArgumentTokenList,
  DesiredArgumentList,
} from "@/features/models/runtime/desired-arguments";
import { desiredArgv } from "@/features/models/runtime/desired-argv";
import { EngineParametersDetails } from "@/features/models/runtime/engine-parameters";
import { resolvedModelEngineDiagnostics } from "@/features/models/runtime/model-engine-diagnostics";
import { type ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * A model deployment's Configuration screen: the resolved configuration, artifacts, engine
 * parameters and custom arguments, then the launch command with a copy control.
 */
export function ResolvedDeploymentDetails({
  snapshot,
}: {
  snapshot: ResolvedLocalModelDeploymentSnapshot;
}) {
  const argv = desiredArgv(snapshot);
  const resolvedSettings = resolvedModelEngineDiagnostics(snapshot);

  return (
    <>
      <section aria-labelledby="resolved-configuration-heading">
        <InspectorSectionHeader headingId="resolved-configuration-heading" title="Resolved" />
        <DetailList>
          <Detail label="Configuration" value={snapshot.configurationId} />
          <Detail label="Model profile" value={snapshot.profileId} />
          <Detail label="Artifact" value={snapshot.modelArtifact?.artifactId ?? "Unavailable"} />
          <Detail
            label="Weight quantization"
            value={weightQuantizationLabel(snapshot.modelArtifact?.weightQuantization ?? "")}
          />
          <Detail
            label="Engine"
            value={
              snapshot.engineArtifact
                ? `${snapshot.engineArtifact.engineReleaseId} / ${snapshot.engineArtifact.engineVersion}`
                : "Unavailable"
            }
          />
          <EngineParametersDetails
            schema={snapshot.engineParameterSchema}
            values={snapshot.engineParameterValues}
          />
          {resolvedSettings ? (
            <>
              <Detail label="GPU layers" value={String(resolvedSettings.gpuLayers)} />
              <Detail
                label="Flash Attention"
                value={resolvedSettings.flashAttention ? "Enabled" : "Disabled"}
              />
            </>
          ) : null}
        </DetailList>
        {snapshot.customArgumentTokens.length ? (
          <div className="mt-4">
            <h3 className="text-xs font-medium text-muted-foreground">Custom arguments</h3>
            <div className="mt-2 rounded-md border border-border p-3">
              <ArgumentTokenList
                ariaLabel="Custom argument tokens"
                tokens={snapshot.customArgumentTokens.map((text) => ({ text, placeholder: false }))}
              />
            </div>
          </div>
        ) : null}
      </section>
      <section aria-labelledby="deployment-command-heading">
        <InspectorSectionHeader
          action={
            <CodeCopyControl label="command" value={argv.map((token) => token.text).join("\n")} />
          }
          headingId="deployment-command-heading"
          title="Command"
        />
        <DesiredArgumentList argv={argv} className="mt-3" />
      </section>
    </>
  );
}
