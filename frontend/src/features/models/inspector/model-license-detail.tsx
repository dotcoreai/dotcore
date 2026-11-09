import { Detail } from "@/components/ui/detail-list";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";

/**
 * The License row of a catalog model inspector's details: the upstream licence identifier, linked
 * to its terms when the declaration has a link. A model without a licence renders nothing.
 */
export function ModelLicenseDetail({ model }: { model: Model }) {
  const license = model.license;
  if (!license) return null;

  return (
    <Detail
      label="License"
      value={
        license.termsUrl ? (
          <a
            className="underline underline-offset-2"
            href={license.termsUrl}
            rel="noopener noreferrer"
            target="_blank"
          >
            {license.id}
          </a>
        ) : (
          license.id
        )
      }
    />
  );
}
