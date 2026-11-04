import type { DecisionForm } from "@/features/chat/decisions/decision-draft";

/** A built-in decision form the operator can load to try a decision route. */
export interface DecisionExample {
  name: string;
  form: DecisionForm;
}

/** The built-in examples, one per common use: triage, document checks and alerts. */
export const decisionExamples: readonly DecisionExample[] = [
  {
    name: "Support triage",
    form: {
      state:
        "Customer writes: My invoice was charged twice this month and I want my money back today.",
      questions: [
        {
          name: "topic",
          type: "choice",
          instructions: "What is the main topic of the message?",
          options: [
            { name: "billing", description: "Charges, invoices, refunds" },
            { name: "technical", description: "Product malfunction" },
            { name: "other", description: "" },
          ],
          levels: [],
        },
        {
          name: "urgency",
          type: "score",
          instructions: "How urgent is the request?",
          options: [],
          levels: ["not urgent", "somewhat urgent", "urgent"],
        },
        {
          name: "refund",
          type: "noul",
          instructions: "Does the customer ask for a refund?",
          options: [],
          levels: [],
        },
      ],
    },
  },
  {
    name: "Invoice check",
    form: {
      state:
        "Invoice #4471 from Acme Corp, total $12,400, due in 7 days, line items: 40h consulting, 1 hardware kit. PO number missing.",
      questions: [
        {
          name: "complete",
          type: "noul",
          instructions: "Does the invoice include everything needed for payment?",
          options: [],
          levels: [],
        },
        {
          name: "category",
          type: "choice",
          instructions: "Which spending category fits best?",
          options: [
            { name: "services", description: "" },
            { name: "hardware", description: "" },
            { name: "mixed", description: "" },
          ],
          levels: [],
        },
      ],
    },
  },
  {
    name: "Security alert",
    form: {
      state:
        "Alert: 37 failed SSH logins for root from 203.0.113.9 within 2 minutes, followed by one success from the same address.",
      questions: [
        {
          name: "severity",
          type: "score",
          instructions: "How severe is the incident?",
          options: [],
          levels: ["informational", "low", "medium", "high", "critical"],
        },
        {
          name: "escalate",
          type: "noul",
          instructions: "Should the on-call engineer be paged now?",
          options: [],
          levels: [],
        },
      ],
    },
  },
];
