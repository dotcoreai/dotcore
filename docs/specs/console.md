# Console

The console is Dotcore's web client, for graphical management and observation of a site. It is
also an inference client for trying routes.

Code: `frontend/` (its `AGENTS.md` holds the code map).

## Capabilities

- Manage and observe the site, its machines, deployments and routes, through the management API.
- See live updates as the site changes.
- Start with a guided Get started page that proposes, for each use case, a model, an app or both that can start now on your hardware.
- Check fit and preview memory before deploying, from the server's capacity answers.
- Explore metric history across machines, devices, deployments and routes.
- Try inference routes: conversations with chat routes, and typed questions to decision routes
  through a form or the raw request.

## Architecture

```mermaid
flowchart LR
  CON["Console"] -->|"changes, reads, live watches"| API["Management API"]
  OWN["Owning components"] -->|"owner reads, recommendations, verdicts, previews"| API
  CAP["Capacity component"] -->|"node memory projection"| API
  CON -->|"try a route"| INF["Inference API"]
  CON -->|"decision drafts, question sets"| BS["Browser storage"]
```

### Watching the site

```mermaid
sequenceDiagram
  participant CON as Console
  participant API as Management API
  CON->>API: Subscribe to live watches
  loop Each change
    API->>CON: Change, stamped with its aligned interval
  end
```

- The console subscribes to the management API's live watches
  ([README](README.md#managing-the-site)).

### Trying a route

```mermaid
flowchart LR
  CHAT["Console chat: browser session"] -->|"chat or decision request"| INF["Inference API"]
  INF -->|"answer"| CHAT
  CHAT -->|"decision drafts, saved question sets"| BS["Browser storage"]
```

- The chat is an ordinary inference client of the inference API.
- Conversations last for the browser session. Decision drafts and saved question sets are kept in
  browser storage.

## Components

### Console

#### Responsibilities

- Draw what the server answers: owner reads, capacity verdicts, previews, recommendations and
  metrics.
- Hide Get started for the whole site once a ready step shows or the operator dismisses it. The
  profile menu still opens the page without changing the choice. The choice is a site-wide console
  setting, read from and updated through the site manager ([sites](sites.md#site-manager)).
- Explain the impact of actions, confirm destructive ones, and show actionable failures
  ([README](README.md#principles)).

#### Boundaries

- Owns no product state. Site-wide console settings belong to the site manager, not the console.
- Browser storage holds only per-browser preferences and drafts, never a setting the whole site
  shares.
- Holds no fit, headroom, sizing or starting-value rule ([machines](machines.md)). Each figure has
  one source, so two screens never disagree.
- Never shows representative or fixture data as infrastructure-backed.
- The chat has no assistant tools, console actions, attachments or saved history
  ([README](README.md#non-goals)).
- Not used for site initialization, node removal, destructive recovery, physical provisioning or
  installation ([cli](cli.md)).
