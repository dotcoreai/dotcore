# Architecture document agentic instructions

- Keep `architecture.md` as the single source of truth for target architecture facts. Use the root `../../AGENTS.md` glossary for canonical terminology; describe relationships and constraints here without maintaining another glossary.
- Follow the shared content and writing rules in `../AGENTS.md`.
- Organize top-down: purpose/context, system overview, components, major flows, cross-cutting constraints, deployment shapes, open decisions.
- Include component purposes, responsibilities, owned state, major connections, and durable constraints that affect design choices.
- Keep meaningful distinctions explicit: logical ownership versus physical placement, desired state versus observations, control versus live traffic, authority versus storage.
- Introduce terms where readers first need them; do not front-load a detailed resource glossary.
- Exclude API methods and fields, configuration inventories, algorithms, operation state machines, implementation walkthroughs, POC exceptions, and delivery history.
- Keep milestones in `../product/product-spec.md` and implementation status in project/phase documents; do not maintain a second roadmap or current-capability ledger.
- Update architecture when target structure, ownership, trust, data boundaries, major flows, or deployment shapes change—not whenever code changes.
- Keep diagrams consistent with the text and limited to major relationships. Treat them as orientation rather than additional normative contracts.
- Preserve unresolved architecture choices as brief questions; do not silently turn them into decisions.
