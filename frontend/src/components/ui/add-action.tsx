import { type LucideIcon, Plus } from "lucide-react";
import { Link } from "react-router-dom";

import { Button, buttonVariants } from "@/components/ui/button";

/**
 * The primary page action: a button with a leading icon, a plus unless `icon` names another, and
 * the given label. It navigates to `to` when given, and otherwise runs `onClick`.
 */
function AddAction(
  props: { icon?: LucideIcon; label: string } & ({ to: string } | { onClick: () => void }),
) {
  const Icon = props.icon ?? Plus;
  const content = (
    <>
      <Icon aria-hidden="true" data-icon="inline-start" />
      {props.label}
    </>
  );

  if ("to" in props) {
    return (
      <Link className={buttonVariants({ size: "console" })} to={props.to}>
        {content}
      </Link>
    );
  }

  return (
    <Button data-panel-navigation="" onClick={props.onClick} size="console" type="button">
      {content}
    </Button>
  );
}

export { AddAction };
