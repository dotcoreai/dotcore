import { Card } from "@/components/ui/card";
import {
  PageHeader,
  PageHeaderDescription,
  PageHeaderHeading,
  PageSectionHeading,
} from "@/components/ui/page-header";
import { PreferencesSectionIcon } from "@/components/ui/section-icons";
import { Switch } from "@/components/ui/switch";
import { DeveloperSection } from "@/features/preferences/developer-section";
import { ColorScheme } from "@/gen/dotcore/console/v1/preferences_pb";
import { useTheme } from "@/theme/theme-context";

/** Console preferences stored in this browser, grouped into sections. */
export function PreferencesPage() {
  return (
    <div className="flex max-w-2xl flex-col gap-6">
      <PageHeader>
        <div>
          <PageHeaderHeading icon={PreferencesSectionIcon}>Preferences</PageHeaderHeading>
          <PageHeaderDescription>Settings for this browser.</PageHeaderDescription>
        </div>
      </PageHeader>
      <AppearanceSection />
      <DeveloperSection />
    </div>
  );
}

function AppearanceSection() {
  const { colorScheme, setColorScheme } = useTheme();

  return (
    <section aria-labelledby="preferences-appearance-heading" className="flex flex-col gap-3">
      <PageSectionHeading id="preferences-appearance-heading">Appearance</PageSectionHeading>
      <Card>
        <PreferenceRow description="Use the dark color scheme." label="Dark mode">
          {(labelledBy) => (
            <Switch
              aria-labelledby={labelledBy}
              checked={colorScheme === ColorScheme.DARK}
              onCheckedChange={(checked) =>
                setColorScheme(checked ? ColorScheme.DARK : ColorScheme.LIGHT)
              }
            />
          )}
        </PreferenceRow>
      </Card>
    </section>
  );
}

/** One labelled setting: text on the left and its control on the right. */
export function PreferenceRow({
  children,
  description,
  label,
}: {
  children: (labelledBy: string) => React.ReactNode;
  description: string;
  label: string;
}) {
  const id = `preference-${label.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`;

  return (
    <div className="flex items-center justify-between gap-4">
      <div className="min-w-0">
        <p className="font-medium" id={id}>
          {label}
        </p>
        <p className="text-muted-foreground">{description}</p>
      </div>
      {children(id)}
    </div>
  );
}
