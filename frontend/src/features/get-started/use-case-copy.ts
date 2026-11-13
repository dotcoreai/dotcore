import { Code, ImageIcon, type LucideIcon } from "lucide-react";

import { ChatSectionIcon } from "@/components/ui/section-icons";
import { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";

/** How Get started names and describes each use case. */
export const useCaseCopy: Record<
  UseCase,
  { description: string; icon: LucideIcon; label: string }
> = {
  [UseCase.UNSPECIFIED]: { description: "", icon: ChatSectionIcon, label: "Unspecified" },
  [UseCase.CHAT_ASSISTANT]: {
    description: "Chat, writing, questions and documents.",
    icon: ChatSectionIcon,
    label: "Chat assistant",
  },
  [UseCase.CODING]: {
    description: "Coding agents in your repositories.",
    icon: Code,
    label: "Coding",
  },
  [UseCase.IMAGE_GENERATION]: {
    description: "Image and video workflows.",
    icon: ImageIcon,
    label: "Image generation",
  },
};
