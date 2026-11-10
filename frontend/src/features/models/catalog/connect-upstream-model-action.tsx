import { Plus } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { UpstreamModelTemplate } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";

/**
 * Page action opening an UpstreamModelTemplate's deploy dialog. A single template opens directly;
 * several open a menu naming each one.
 */
export function ConnectUpstreamModelAction({
  onSelect,
  templates,
}: {
  onSelect: (template: UpstreamModelTemplate) => void;
  templates: readonly UpstreamModelTemplate[];
}) {
  const label = (
    <>
      <Plus data-icon="inline-start" />
      {/* Phone widths keep the page heading and this action on one row, so only "Connect" shows. */}
      <span>
        Connect<span className="max-sm:hidden"> upstream</span>
      </span>
    </>
  );

  if (templates.length === 1) {
    return (
      <Button
        aria-label={connectUpstreamLabel}
        onClick={() => onSelect(templates[0])}
        size="console"
        type="button"
        variant="outline"
      >
        {label}
      </Button>
    );
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label={connectUpstreamLabel}
        render={<Button size="console" type="button" variant="outline" />}
      >
        {label}
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="border border-border" sideOffset={6}>
        {templates.map((template) => (
          <DropdownMenuItem
            key={template.upstreamModelTemplateId}
            onClick={() => onSelect(template)}
          >
            {template.displayName || template.upstreamModelTemplateId}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

const connectUpstreamLabel = "Connect upstream";
