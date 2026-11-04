import { Popover as PopoverPrimitive } from "@base-ui/react/popover";

import { useInspectorFloatingLayerRegistration } from "@/components/ui/inspector-floating-layer";
import { cn } from "@/lib/utils";

function Popover({ defaultOpen, onOpenChange, open, ...props }: PopoverPrimitive.Root.Props) {
  const registerFloatingLayer = useInspectorFloatingLayerRegistration(open, defaultOpen);

  return (
    <PopoverPrimitive.Root
      data-slot="popover"
      defaultOpen={defaultOpen}
      onOpenChange={(nextOpen, details) => {
        if (open === undefined) registerFloatingLayer(nextOpen);
        onOpenChange?.(nextOpen, details);
      }}
      open={open}
      {...props}
    />
  );
}

function PopoverTrigger({ ...props }: PopoverPrimitive.Trigger.Props) {
  return <PopoverPrimitive.Trigger data-slot="popover-trigger" {...props} />;
}

function PopoverContent({
  align = "center",
  alignOffset = 0,
  anchor,
  className,
  side = "bottom",
  sideOffset = 4,
  ...props
}: PopoverPrimitive.Popup.Props &
  Pick<
    PopoverPrimitive.Positioner.Props,
    "align" | "alignOffset" | "anchor" | "side" | "sideOffset"
  >) {
  return (
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Positioner
        align={align}
        alignOffset={alignOffset}
        anchor={anchor}
        className="isolate z-popup"
        side={side}
        sideOffset={sideOffset}
      >
        <PopoverPrimitive.Popup
          data-slot="popover-content"
          className={cn(
            "flex max-h-(--available-height) w-72 max-w-(--available-width) origin-(--transform-origin) overflow-y-auto flex-col gap-2.5 rounded-lg bg-popover p-2.5 text-sm text-popover-foreground shadow-md ring-1 ring-foreground/10 outline-hidden duration-100 data-[side=bottom]:slide-in-from-top-2 data-[side=inline-end]:slide-in-from-left-2 data-[side=inline-start]:slide-in-from-right-2 data-[side=left]:slide-in-from-right-2 data-[side=right]:slide-in-from-left-2 data-[side=top]:slide-in-from-bottom-2 data-open:animate-in data-open:fade-in-0 data-open:zoom-in-95 data-closed:animate-out data-closed:fade-out-0 data-closed:zoom-out-95",
            className,
          )}
          {...props}
        />
      </PopoverPrimitive.Positioner>
    </PopoverPrimitive.Portal>
  );
}

export { Popover, PopoverContent, PopoverTrigger };
