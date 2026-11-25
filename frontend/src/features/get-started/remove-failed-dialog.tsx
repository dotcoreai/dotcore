import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

/**
 * The confirmation for removing a use case's failed deployments. It names what is removed, says
 * that deployments which didn't fail stay, and warns that the logs go with the removed ones.
 */
export function RemoveFailedDialog({
  failedNames,
  keptNames,
  onConfirm,
  onOpenChange,
  open,
  pending,
}: {
  /** The failed deployments the removal tears down. */
  failedNames: readonly string[];
  /** The followed deployments that stay. */
  keptNames: readonly string[];
  onConfirm: () => void;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  pending: boolean;
}) {
  return (
    <Dialog disablePointerDismissal modal onOpenChange={onOpenChange} open={open}>
      <DialogContent showCloseButton={false}>
        <DialogHeader>
          <DialogTitle>Remove the failed deployment?</DialogTitle>
          <DialogDescription>
            This removes {failedNames.join(" and ")} and its logs. It stays listed as Tearing down
            until cleanup completes, then Get started proposes the deployment again.
            {keptNames.length ? ` ${keptNames.join(" and ")} stays.` : ""}
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <DialogClose
            render={
              <Button type="button" variant="outline">
                Cancel
              </Button>
            }
          />
          <Button disabled={pending} onClick={onConfirm} type="button" variant="destructive">
            {pending ? "Removing…" : "Remove and start over"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
