import { mergeProps } from "@base-ui/react/merge-props";
import { useRender } from "@base-ui/react/use-render";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

const badgeVariants = cva(
  // Geometry matches the console demo's .badge base class exactly: a pill regardless of theme
  // radius, 3px/9px padding, 11px/600 type, width hugging its content.
  "group/badge inline-flex w-fit shrink-0 items-center justify-center gap-1 overflow-hidden rounded-full border border-transparent px-[9px] py-[3px] text-[11px] font-semibold whitespace-nowrap transition-all focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 aria-invalid:border-destructive aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 [&>svg]:pointer-events-none [&>svg]:size-3!",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground [a]:hover:bg-primary/80",
        secondary: "bg-secondary text-secondary-foreground [a]:hover:bg-secondary/80",
        // Demo .b-danger: the local status-scale --danger chip with the already-owned
        // --destructive ink as its text, rather than an opacity tint of --destructive alone.
        destructive:
          "bg-danger text-destructive focus-visible:ring-destructive/20 dark:focus-visible:ring-destructive/40 [a]:hover:bg-danger/80",
        outline: "border-border text-foreground [a]:hover:bg-muted [a]:hover:text-muted-foreground",
        ghost: "hover:bg-muted hover:text-muted-foreground dark:hover:bg-muted/50",
        link: "text-primary underline-offset-4 hover:underline",
        // Demo .b-ok, .b-info, .b-warn, .b-neutral: local extensions of the generated variant map,
        // since shadcn ships only default/secondary/destructive/outline/ghost/link.
        success: "bg-success text-success-foreground [a]:hover:bg-success/80",
        info: "bg-info text-primary [a]:hover:bg-info/80",
        warning: "bg-warning text-warning-foreground [a]:hover:bg-warning/80",
        neutral: "bg-muted text-muted-foreground [a]:hover:bg-muted/80",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

function Badge({
  className,
  variant = "default",
  render,
  ...props
}: useRender.ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return useRender({
    defaultTagName: "span",
    props: mergeProps<"span">(
      {
        className: cn(badgeVariants({ variant }), className),
      },
      props,
    ),
    render,
    state: {
      slot: "badge",
      variant,
    },
  });
}

export { Badge, badgeVariants };
