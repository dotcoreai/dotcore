# Product and architecture agentic instructions

These pages are the one authority for Dotcore's product intent and architecture: what each component
owns, what it never does, what it keeps, and the guarantees that hold between components. Delivery
milestones and commercial plans live outside this repository. Treat the pages as the scope.

## When to update

- A change to ownership, boundaries, identities, retained state, flows, failure behavior, trust or
  data boundaries updates its page in the same change.
- Routine code and UI changes that keep every contract need no page edit and no discussion.
- Leave out POC-only and scope-limited detail.
- Removing a fact is a design decision. A rewrite or move never drops one silently.

## Pages

- `README.md` lists the pages and the components each owns. Add a page only when a new component
  owns a new kind of state, or when a new client (a way people reach Dotcore) appears.
- Each page names the code it covers. Every component that has code appears in an area code map
  (`backend/AGENTS.md`, `frontend/AGENTS.md`), and every crate that holds a component appears on
  one page.

## Page skeleton

Use these sections, in this order, and leave out any that would be empty:

1. Title, a one or two sentence definition naming the owning components, and the code it covers.
2. `## Capabilities`: what the subject lets users and operators do. Outcomes, not screens or commands.
3. `## Architecture`: a component diagram, each component in one line, then one `###` per flow: a
   flow or sequence diagram, what moves between which components, then that flow's rules as
   bullets.
4. `## Resources`: one `###` per thing the page owns: its identity, whether it can change, what it
   keeps and how long it lives.
5. `## Components`: one `###` per component, with these `####` parts:
   - `Responsibilities`: the state and decisions it owns. Always present.
   - `Boundaries`: what it never owns or does, and who does instead. Always present.
   - `Interfaces`: what it receives, from whom, and what it hands back, in domain terms.
   - `State`: what it keeps, where and for how long.
   Variants of a component, such as the runtime backends, go in a short list under its heading.
6. `## Invariants`: guarantees that bind several components on the page, or that no change may break.
7. `## Future direction`: planned capabilities and ideas. No dates or milestones.
8. `## Open decisions`: undecided questions, worded as questions or topics. Never settle one through
   wording.

The definition, Capabilities and the opening of Architecture stay short, for people browsing. Flows,
Resources, Components and Invariants are as long as the contracts need.

## What goes in

Ask: could two correct implementations differ here and still be the same design? If they could, the
code owns it. If not, it goes in a page.

A statement goes in when it fixes one of these:

- Who owns, decides or writes something.
- What a component never does, and who does instead.
- A resource's identity, whether it can change, what it keeps by category, or how long it lives.
- How the system behaves on failure or under trust: advice never blocks, no silent fallback, fails
  closed.
- A component's inputs and outputs, in domain terms.

The code owns, and the pages leave out:

- Numbers: headroom, silence bounds, retention periods, intervals.
- Formulas, field names, API methods, flags, configuration inventories and paths.
- The tools a component uses internally.
- Step order inside one component, detailed state machines and implementation walkthroughs.
- Coding rules such as the backend clock and timing policy (`backend/AGENTS.md`).

Also out: milestones, delivery dates, pricing, commercial plans, unannounced products and services,
and implementation status or history.

## Where a fact goes

- On the page of the component that owns or enforces it, in one section. Other pages link to it and
  never repeat it.
- A flow's rules sit under the flow. A rule goes in Invariants only when it binds several components
  on the page.
- A component that owns no state goes on the page of the question it answers: the capacity component
  in `machines.md`, the router in `inference.md`.
- Ideas and open decisions that span several pages go in the README.

## Writing

- Plain English: short sentences, "you" for the operator, the same word for the same thing. Bullets
  are the default in flows, resources and components.
- Precision beats brevity. Reword a statement for clarity, but keep every qualifier and exception.
- Glossary terms from the root `AGENTS.md` are used as is. A contract term not in the glossary is
  defined once, on the page that owns it.
- Describe the target design, never how it was built or what changed.
- Diagrams are Mermaid and match the text. The text is the contract.
- Diagrams zoom in three levels:
  - README top level: clients, the site manager node and worker nodes, and the deployments they
    run. No components and no relation labels.
  - README zooms: the components on the site manager node, on every node, and in platform services,
    each linking to its pages.
  - Subject pages: the page's components with their neighbours and labeled relations, and one
    diagram per flow.
- Keep a diagram to about fifteen boxes; split a bigger one.
