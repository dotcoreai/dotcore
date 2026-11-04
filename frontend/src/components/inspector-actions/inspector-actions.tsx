import { Ellipsis, LoaderCircle, type LucideIcon } from "lucide-react";
import { type ReactNode, type RefObject, useId } from "react";

import { Button, buttonVariants } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";

/**
 * One inspector action: a label with its icon that either runs a handler or opens an external
 * link. Destructive actions ask for confirmation before acting, so their label gains a trailing
 * ellipsis. While pending, the pending label replaces the label.
 */
export type InspectorAction = {
  destructive?: boolean;
  disabled?: boolean;
  /**
   * Why the action is disabled. A disabled button opens it in a popup on hover, focus or tap; a
   * disabled menu item shows it under its label.
   */
  disabledReason?: string;
  /**
   * Receives the element that keeps focus for this action: its own button, or the more-actions
   * trigger when the action sits in the menu. Confirmation dialogs return focus there.
   */
  focusRef?: RefObject<HTMLElement | null>;
  icon: LucideIcon;
  label: string;
  pending?: boolean;
  pendingLabel?: string;
} & ({ href: string; onSelect?: never } | { href?: never; onSelect: () => void });

/**
 * The action row of an inspector footer: at most one filled primary action, visible outline
 * secondary actions, and a "More actions" menu for the less used ones. A menu never holds a single
 * action, which then stays a button, and destructive menu actions go last behind a divider.
 */
export function InspectorActions({
  more = [],
  primary,
  secondary = [],
}: {
  more?: InspectorAction[];
  primary?: InspectorAction;
  secondary?: InspectorAction[];
}) {
  const reasonIdPrefix = useId();
  const menu = more.length > 1 ? more : [];
  const buttons: { action: InspectorAction; primary: boolean }[] = [
    ...(primary ? [{ action: primary, primary: true }] : []),
    ...[...secondary, ...(more.length === 1 ? more : [])].map((action) => ({
      action,
      primary: false,
    })),
  ];

  return (
    <>
      {buttons.map(({ action, primary }, index) => (
        <ActionButton
          action={action}
          key={action.label}
          primary={primary}
          reasonId={`${reasonIdPrefix}-${index}`}
        />
      ))}
      {menu.length > 0 ? <MoreActionsMenu actions={menu} /> : null}
    </>
  );
}

function ActionButton({
  action,
  primary,
  reasonId,
}: {
  action: InspectorAction;
  primary: boolean;
  reasonId: string;
}) {
  const variant = action.destructive ? "destructive" : primary ? "default" : "outline";
  const content = <ActionContent action={action} />;

  if (action.href !== undefined) {
    return (
      <a
        className={buttonVariants({ size: "console", variant })}
        href={action.href}
        ref={(node) => assignFocusRefs([action], node)}
        rel="noreferrer"
        target="_blank"
      >
        {content}
      </a>
    );
  }

  if (action.disabled && action.disabledReason && !action.pending) {
    return (
      <BlockedActionButton action={action} reasonId={reasonId} variant={variant}>
        {content}
      </BlockedActionButton>
    );
  }

  return (
    <Button
      disabled={action.disabled || action.pending}
      onClick={action.onSelect}
      ref={(node) => assignFocusRefs([action], node)}
      size="console"
      type="button"
      variant={variant}
    >
      {content}
    </Button>
  );
}

/**
 * A disabled button that explains itself. It stays focusable and tappable, unlike a natively
 * disabled button, so hovering, focusing or tapping it opens a popup with the reason, which touch
 * and keyboard operators cannot get from a tooltip.
 */
function BlockedActionButton({
  action,
  children,
  reasonId,
  variant,
}: {
  action: InspectorAction;
  children: ReactNode;
  reasonId: string;
  variant: "default" | "destructive" | "outline";
}) {
  return (
    <Popover>
      <PopoverTrigger
        aria-describedby={reasonId}
        aria-disabled="true"
        openOnHover
        ref={(node) => assignFocusRefs([action], node)}
        render={
          <Button
            className="cursor-not-allowed opacity-50"
            size="console"
            type="button"
            variant={variant}
          />
        }
      >
        {children}
      </PopoverTrigger>
      <PopoverContent className="w-auto max-w-xs text-xs" side="top">
        {action.disabledReason}
      </PopoverContent>
      <span className="sr-only" id={reasonId}>
        {action.disabledReason}
      </span>
    </Popover>
  );
}

function MoreActionsMenu({ actions }: { actions: InspectorAction[] }) {
  const regular = actions.filter((action) => !action.destructive);
  const destructive = actions.filter((action) => action.destructive);
  const pending = actions.some((action) => action.pending);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-busy={pending || undefined}
        aria-label="More actions"
        disabled={pending}
        ref={(node) => assignFocusRefs(actions, node)}
        render={<Button className="px-2" size="console" type="button" variant="outline" />}
      >
        {pending ? (
          <LoaderCircle aria-hidden="true" className="animate-spin motion-reduce:animate-none" />
        ) : (
          <Ellipsis aria-hidden="true" />
        )}
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-auto min-w-40" side="top">
        {regular.map((action) => (
          <ActionMenuItem action={action} key={action.label} />
        ))}
        {regular.length > 0 && destructive.length > 0 ? <DropdownMenuSeparator /> : null}
        {destructive.map((action) => (
          <ActionMenuItem action={action} key={action.label} />
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function ActionMenuItem({ action }: { action: InspectorAction }) {
  const reasonId = useId();
  const reason = action.disabled ? action.disabledReason : undefined;
  const label = actionLabel(action);

  return (
    // The label names the item on its own; a disabled reason describes it instead of joining it.
    <DropdownMenuItem
      aria-describedby={reason ? reasonId : undefined}
      aria-label={label}
      disabled={action.disabled || action.pending}
      onClick={action.onSelect}
      render={
        action.href !== undefined ? (
          // The menu item renders its content inside the anchor.
          // eslint-disable-next-line jsx-a11y/anchor-has-content
          <a href={action.href} rel="noreferrer" target="_blank" />
        ) : undefined
      }
      variant={action.destructive ? "destructive" : "default"}
    >
      {/* The item centers this group, so the icon tracks the label's first line at any row height. */}
      <span className="flex min-w-0 items-start gap-1.5">
        <action.icon aria-hidden="true" className="mt-0.5" />
        <span className="flex min-w-0 flex-col">
          <span>{label}</span>
          {reason ? (
            <span className="text-xs text-muted-foreground" id={reasonId}>
              {reason}
            </span>
          ) : null}
        </span>
      </span>
    </DropdownMenuItem>
  );
}

function ActionContent({ action }: { action: InspectorAction }) {
  return (
    <>
      <action.icon aria-hidden="true" data-icon="inline-start" />
      {actionLabel(action)}
    </>
  );
}

function actionLabel(action: InspectorAction): string {
  if (action.pending && action.pendingLabel) return action.pendingLabel;
  return action.destructive ? `${action.label}…` : action.label;
}

function assignFocusRefs(actions: InspectorAction[], node: HTMLElement | null) {
  for (const action of actions) {
    if (action.focusRef) action.focusRef.current = node;
  }
}
