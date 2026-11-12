# Docs agentic instructions

`docs/` holds three kinds of documents, each for a different reader:

- `install.md` and `usage.md`: guides for people who run Dotcore.
- `development/`: guides for people who change Dotcore.
- `specs/`: the product and architecture specs. [`specs/AGENTS.md`](specs/AGENTS.md) owns their
  rules.

## Rules

- The root [`AGENTS.md`](../AGENTS.md) owns the writing rules, including progressive disclosure and
  keeping a document to what a reader needs to start.
- A fact goes in the one document whose reader needs it: how to run Dotcore in a guide, how to
  change it in the development guides, what it is and how it's built in the specs. Other documents
  link to it.
- `README.md` here only points to the documents above, with a few words each. Add a line when a
  document is added.
- A guide that leads into another ends with a horizontal rule and `Next: [Title](file.md).`
