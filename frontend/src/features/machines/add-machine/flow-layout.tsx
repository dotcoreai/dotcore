import type { ReactNode } from "react";

import { DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";

import { StepList } from "./join-steps";

/**
 * The frame of a machine dialog: its title, an optional status next to the close button, the
 * numbered steps and the actions.
 */
export function FlowLayout({
  children,
  footer,
  status,
  title,
}: {
  children: ReactNode;
  footer?: ReactNode;
  status?: ReactNode;
  title: string;
}) {
  return (
    <>
      <DialogHeader className="flex-row items-center gap-3 border-b px-[1.6rem] py-[1.1rem] pr-16 max-sm:px-5 max-sm:pr-16 sm:pr-14">
        <DialogTitle className="mr-auto text-[17px] leading-6 font-semibold whitespace-nowrap">
          {title}
        </DialogTitle>
        {status}
      </DialogHeader>
      <div className="overflow-y-auto px-[1.6rem] py-6">
        <StepList>{children}</StepList>
      </div>
      {footer ? <DialogFooter className="m-0 px-[1.6rem] py-4">{footer}</DialogFooter> : null}
    </>
  );
}
