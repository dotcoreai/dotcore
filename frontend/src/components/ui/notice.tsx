import { cva, type VariantProps } from "class-variance-authority";
import type * as React from "react";

import { cn } from "@/lib/utils";

const noticeVariants = cva("rounded-lg border px-[15px] py-[13px] text-[12.5px]", {
  variants: {
    tone: {
      info: "border-primary/30 bg-info text-primary",
      destructive: "border-destructive/30 bg-destructive/10 text-destructive",
      warning: "border-warning-foreground/30 bg-warning text-warning-foreground",
    },
  },
  defaultVariants: {
    tone: "info",
  },
});

function Notice({
  className,
  tone,
  ...props
}: React.ComponentProps<"p"> & VariantProps<typeof noticeVariants>) {
  return <p data-slot="notice" className={cn(noticeVariants({ tone }), className)} {...props} />;
}

export { Notice, noticeVariants };
