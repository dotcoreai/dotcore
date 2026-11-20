import { Detail } from "@/components/ui/detail-list";

/**
 * The License row of an inspector's details: the upstream licence identifier, linked to its terms
 * when the declaration has a link. An undeclared licence renders nothing.
 */
export function LicenseDetail({ license }: { license?: { id: string; termsUrl: string } }) {
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
