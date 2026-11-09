export type DeploymentMetadata = Readonly<{
  displayName: string;
}>;

export type DeploymentMetadataUpdate = {
  metadata: DeploymentMetadata;
  fields: readonly (keyof DeploymentMetadata)[];
};
