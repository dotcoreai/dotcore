# Design documents agentic instructions

These pages are the one authority for Dotcore's product intent and architecture. Delivery
milestones and commercial plans live outside this repository. Treat the pages as the scope.

## Pages

The contents table in `README.md` says what each page covers. Each page covers one subject and the
component that owns it. Add a page only when a new component owns a new kind of state.

To find the page for some code, read the code map in that area's `AGENTS.md` (`backend/`,
`frontend/`) and match the component to the contents table.

## Where a fact goes

- On the page of the component that owns or enforces it. Other pages link to it and never repeat
  it.
- A component that owns no state goes on the page of the question it answers: the router in
  `inference.md`, the capacity component in `machines.md`.
- A term not in the root glossary is defined on the page that explains it.
- Ideas and open questions go in the Future section of the page they belong to. Ideas that span
  several pages go in the README's Future section.

## Page template

1. Title and a one-line definition.
2. What you can do: what the user gets, not screens.
3. How it works: one Mermaid diagram, then short paragraphs. Use bullets only for a real list of
   parallel items.
4. Rules: the constraints a change must keep.
5. Future: planned capabilities and ideas that aren't committed yet, then open questions starting
   with "Open:". No dates or milestones.

Keep a page around 300 to 550 words.

## Writing

- Write like an engineer explaining the system to a colleague. Plain words, short sentences, "you"
  rather than "the customer". Use the same word for the same thing every time.
- Describe the system as it's meant to be. Never write how it was built, what changed, or
  implementation steps.
- State each fact once.
- No bold labels at the start of bullets, no semicolons, no em dashes, and no slogan-style
  headings or principle names.
- Diagrams are Mermaid, show only the major relationships, and match the text.
- Update a page when ownership, trust, data boundaries, major flows, user outcomes or site shapes
  change, not for routine code or UI changes.
- Keep open questions as questions. Don't turn one into a decision through wording.

## What stays out

- Milestones, delivery dates, pricing and commercial plans.
- Unannounced products and services.
- Commands, flags, API fields, configuration lists, algorithms and detailed state machines.
- Detail the code owns: accounting formulas, heartbeat timing, snapshot fields, folder layouts,
  clock and timing policies.
