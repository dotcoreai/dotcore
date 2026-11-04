import { CodeCopyControl } from "@/components/ui/copy-control";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";

const nodeAddCommand = "dotc node add";

/** Shows the steps to add a machine to the site with the command-line tool. */
export function AddMachineDialog({
  onOpenChange,
  open,
}: {
  onOpenChange: (open: boolean) => void;
  open: boolean;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="gap-0 p-0 sm:max-w-md"
        // Centres the close button on the title row and aligns its icon with the header gutter.
        closeButtonClassName="top-3.5 right-3.5 max-sm:top-1.5"
      >
        <DialogHeader className="border-b px-5 py-4 pr-16 sm:pr-12">
          <DialogTitle className="text-[17px] leading-6 font-semibold">Add a machine</DialogTitle>
        </DialogHeader>
        <ol className="flex list-decimal flex-col gap-3 py-4 pr-5 pl-10 text-sm">
          <li>
            Stop the manager, then run on its machine:
            <div className="mt-2 flex items-center justify-between gap-2 rounded-md border border-border bg-muted/50 py-1.5 pr-1.5 pl-3">
              <code className="font-mono">{nodeAddCommand}</code>
              <CodeCopyControl label={`${nodeAddCommand} command`} value={nodeAddCommand} />
            </div>
          </li>
          <li>
            On the new machine, run the <code className="font-mono">dotc node join</code> command it
            prints.
          </li>
          <li>Allow the machine on the manager, then start the manager and the new machine.</li>
        </ol>
      </DialogContent>
    </Dialog>
  );
}
