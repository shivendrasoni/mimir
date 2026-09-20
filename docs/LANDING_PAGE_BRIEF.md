# Mimir enterprise landing page brief

This document defines the positioning, narrative, page sections, copy, and brand direction for Mimir's enterprise landing page. It is a content brief, not a page implementation.

## The positioning decision

Mimir should not enter the market as another coding agent. Models and coding agents will keep changing. The durable enterprise problem is operating them consistently across people, teams, repositories, and security boundaries.

**Category:** Enterprise agent operations

**Plain-language explanation:** A governed, codebase-aware harness for AI coding agents.

**Positioning statement:** Mimir gives enterprises one operational layer for deploying, governing, measuring, and improving AI coding agents across every codebase.

**Brand promise:** Every agent starts with the right context, stays inside enterprise rules, and improves from evidence without putting the organization at risk.

**Tagline:** One harness. Every repository. Your rules.

**Core idea:** Scale agent capability without scaling chaos.

### Who the page is for

- CTOs and VPs of Engineering deciding how agents should operate across the company.
- Platform, Developer Experience, and AI Platform teams responsible for the paved road.
- Security leaders who need control over tools, extensions, data routes, and execution.
- Engineering leaders who need to know whether the chosen setup actually improves delivery.

### The buyer's problem

AI agents can increase output, but they introduce a new operational cold start:

- Every developer assembles a different setup.
- Every repository needs its own context, commands, policies, and workflows.
- Teams install unreviewed skills, MCP servers, extensions, and scripts.
- Prompt-level instructions are mistaken for enforceable policy.
- Useful practices stay local instead of compounding across the organization.
- Leaders cannot tell which configurations improve outcomes and which only add cost or risk.

The page should make this feel like an enterprise systems problem, not a developer-preference problem.

## Message hierarchy

1. **Outcome:** Make AI agents operable across the enterprise.
2. **Immediate value:** Give every developer a ready, codebase-aware setup from day one.
3. **Control:** Apply company policy while allowing teams to keep the workflows they need.
4. **Trust:** Admit only approved capabilities and keep dangerous actions bounded.
5. **Evidence:** Measure whether each setup actually works, then improve it safely.
6. **Technical proof:** Mimir already has a serious Rust runtime, project isolation, policy boundaries, privacy-safe diagnostics, reversible learning, durable delegation, and multi-provider support.

## Page copy

### 1. Navigation

**Wordmark:** Mimir

**Links:** Platform · Governance · Learning · Benchmarks · Security

**Primary action:** Request a pilot

The navigation should stay compact. Documentation and GitHub can live behind a secondary menu or the footer so the main page remains a product story rather than an open-source project index.

### 2. Hero

**Eyebrow:** ENTERPRISE AGENT OPERATIONS

**Headline:** AI agents your enterprise can actually operate.

**Body:** Mimir turns every codebase into a governed agent environment—ready for developers on day one, aligned to each team, and measurable across the organization.

**Supporting line:** Use the models you choose. Keep the standards you trust. Improve from evidence without surrendering control.

**Primary CTA:** Request an enterprise pilot

**Secondary CTA:** See how Mimir works

**Trust line:** Model-agnostic · Repository-aware · Policy-enforced

**Suggested hero visual:** A living organizational map flowing from company policy to team configurations, repository blueprints, active agent runs, and verified learning. The visual should show control and inheritance, not a chatbot window.

### 3. Proof strip

**Intro:** Built for real code, not controlled demos.

- **100% solve rate** — Mimir and Claude Code both solved every task in the recorded six-task comparison.
- **84.5% fewer reported tokens** — Mean total tokens versus Claude Code in that comparison.
- **70.9% lower median runtime** — Versus Codex with both harnesses using the matched GPT-5.6 Terra model.
- **0 scope violations** — Across both recorded comparison suites.

**Footnote:** Results are from fixed six-task suites with three repeats per task. Harness-level benchmarks are not universal model-quality claims. Link to the complete reports and methodology.

Do not compress these into one vague “faster and cheaper” claim. The specific methodology is part of the credibility.

### 4. Problem

**Eyebrow:** THE AGENT COLD START

**Headline:** More capable agents do not remove the setup problem. They multiply it.

**Body:** Whether AI improves a team's output by 10%, 100%, or 1,000%, every new developer, repository, and agent still has to answer the same questions.

**Questions:**

- What should this agent know about this codebase?
- Which models, tools, and data sources may it use?
- Which skills and extensions can the team trust?
- What is it allowed to change or execute?
- How will we know whether this setup is working?

**Close:** Without a shared operational layer, every answer becomes a local convention. Onboarding slows down. Configurations drift. Unreviewed capabilities enter the environment. The organization gains powerful agents but loses control of the system around them.

### 5. Platform introduction

**Eyebrow:** THE MIMIR CONTROL PLANE

**Headline:** One operating layer between your developers, agents, and code.

**Body:** Mimir maps each repository to its approved context, capabilities, policies, and learning. Platform teams set the company baseline. Product teams extend it within guardrails. Developers enter a codebase with an agent already configured for the work.

**System line:** Company standard → Team policy → Repository blueprint → Agent run → Evidence → Verified improvement

The page should present this as a closed operational loop, not a collection of unrelated features.

### 6. Four outcome pillars

#### Ready on day one

**Body:** Replace tribal setup guides with repository blueprints. Give every developer the right commands, context, quality gates, tools, and workflows as soon as they open the project.

**Supporting capability:** Repository-to-team mapping, inherited configuration, project-isolated state, and reproducible setup.

#### One company. Many team workflows.

**Body:** Define the non-negotiables once, then let each team add the domain knowledge and workflows its code requires. Teams stay autonomous without rebuilding the harness from scratch.

**Supporting capability:** Organization baselines with team and repository overlays, deterministic precedence, and scoped memory.

#### Only approved capabilities

**Body:** Turn skills, extensions, MCP servers, and tools from shadow infrastructure into a governed supply chain. Review provenance and permissions, pin versions, approve use by scope, and revoke unsafe capabilities centrally.

**Supporting capability:** Trusted registry, capability manifests, signed packages, policy enforcement, version pinning, and revocation.

#### Know what is working

**Body:** Measure completion, reliability, latency, token use, recoveries, and policy failures without collecting raw prompts or source code. Compare configurations, canary improvements, and roll back what does not help.

**Supporting capability:** Privacy-safe diagnostics, benchmarks, outcome evidence, verified canaries, and reversible learning.

### 7. Organizational model

**Eyebrow:** CENTRAL CONTROL. LOCAL FIT.

**Headline:** A shared standard without a one-size-fits-all agent.

**Body:** Security and platform teams control the enterprise boundary. Engineering teams control how work gets done inside it. Repositories carry the final, local layer of truth.

**Organization sets:**

- Approved model providers and data routes.
- Identity, permissions, budgets, and execution policy.
- Trusted skill, extension, and MCP catalogs.
- Required audit, reliability, and quality controls.

**Teams add:**

- Domain-specific workflows and task templates.
- Team-approved skills and integrations.
- Shared conventions across a group of repositories.
- Team-level performance and reliability goals.

**Repositories define:**

- Build, test, validation, and release commands.
- Codebase context, ownership, and architectural boundaries.
- Local memories, quality gates, and task-specific constraints.
- The exact capabilities permitted for that project.

**Close:** Policy inherits downward. Evidence rolls upward. Raw code and prompts do not have to.

### 8. How it works

**Eyebrow:** FROM FIRST REPOSITORY TO FLEET

**Headline:** Connect once. Improve continuously.

1. **Connect the organization.** Link identity, code hosts, model providers, and the systems agents are allowed to use.
2. **Map each codebase.** Assign repositories to teams and attach the right blueprint, owners, commands, context, and quality gates.
3. **Deploy the guardrails.** Apply approved models, tools, skills, permissions, budgets, and data policies at the correct scope.
4. **Observe outcomes.** Collect bounded operational evidence about what completed, failed, recovered, or violated policy—without storing raw work.
5. **Promote what works.** Canary improvements, verify them against real outcomes, sign trusted updates, and roll them out by team or fleet. Roll back immediately when the evidence turns.

### 9. Mimir technology

**Eyebrow:** BUILT ON MIMIR

**Headline:** A serious runtime beneath the control plane.

**Body:** Enterprise control only matters if the runtime can enforce it. Mimir is a fast, memory-efficient agentic runtime written in safe Rust and designed for long-running, inspectable work.

**Capability cards:**

- **Durable delegation.** Parent agents can start, observe, cancel, and remove bounded child agents while work remains scoped to the session.
- **Provider independence.** Run Anthropic, OpenAI/Codex, Bedrock, Vertex/Google, Mistral, compatible providers, or extension-provided transports through one typed runtime.
- **Runtime-enforced policy.** Workspace boundaries, exact process allowlists, tool budgets, permissions, timeouts, and cancellation are enforced beneath the model.
- **Continual harness learning.** Turn evidence into scoped prompt, memory, skill, or subagent improvements without changing model weights or the immutable base prompt.
- **Private operational evidence.** Inspect and replay bounded run metadata without storing prompt text, model output, tool payloads, credentials, or absolute host paths.
- **Extensible by design.** Add tools, providers, resources, lifecycle hooks, and interfaces through capability-scoped extension surfaces.

**Brag line:** No Node.js runtime required. No unsafe Rust. No prompt masquerading as policy.

### 10. Security

**Eyebrow:** GOVERNANCE BELOW THE MODEL

**Headline:** Control is an architecture, not an admin screen.

**Body:** Models can propose actions. Mimir decides what the runtime will permit. The security boundary remains in deterministic code, where it can be inspected, tested, and enforced.

**Controls:**

- Process execution is disabled by default; the narrower process tool requires an exact allowlist.
- File tools are rooted to the canonical workspace and reject traversal and escaping symlinks.
- Embedded extensions receive no filesystem, network, shell, or Node.js access implicitly.
- Credentials are redacted and excluded from diagnostics.
- Diagnostic evidence excludes raw prompts, model output, tool arguments, tool output, environment values, and absolute host paths.
- Fleet learning packs are versioned, signed, bounded, revocable, and reversible.
- Provider activity, tool output, context, child concurrency, and execution time remain bounded.

**CTA:** Read the security model

### 11. Before and after

**Eyebrow:** FROM AGENT SPRAWL TO AGENT OPERATIONS

| Without Mimir | With Mimir |
| --- | --- |
| Every developer assembles a different setup | Every repository opens with an inherited, team-approved blueprint |
| Unreviewed marketplace packages enter production workflows | Skills and extensions move through a trusted, revocable supply chain |
| Prompt instructions stand in for controls | Runtime policy enforces what agents may access and execute |
| Context and state leak between unrelated work | Sessions, state, and learning stay scoped to the correct project |
| Teams debate productivity through anecdotes | Privacy-safe evidence shows completion, reliability, cost, and failure modes |
| Good practices stay on one laptop | Verified improvements can move safely from project to team to fleet |

### 12. Closing CTA

**Headline:** Scale the agents. Keep the standards.

**Body:** Start with one engineering team, prove the setup against real work, and expand across the organization without rebuilding the harness for every repository.

**Primary CTA:** Request an enterprise pilot

**Secondary CTA:** Explore the technical foundation

**Final line:** One harness. Every repository. Your rules.

### 13. Footer

**Product:** Platform · Governance · Learning · Benchmarks · Security

**Developers:** Documentation · GitHub · Releases · Architecture

**Company:** Contact · Enterprise pilot

**Legal:** Privacy · Security reporting · License

## Brand direction

### Brand idea

Mimir should feel like calm command over a complex system. It is not a robot, a copilot, or a magical oracle. It is the layer that preserves institutional knowledge, applies boundaries, and makes agent behavior legible.

The name can quietly evoke memory and wisdom, but the visual language should avoid Viking, helmet, rune, fantasy, and “all-seeing eye” clichés.

### Voice

- Calm, exact, and operational.
- Confident enough to make a clear claim, disciplined enough to show the evidence.
- Outcome-led in headlines; technically concrete in supporting copy.
- Enterprise without sounding bureaucratic.
- Ambitious without words such as “revolutionary,” “limitless,” “magic,” or “fully autonomous.”

**Preferred verbs:** map, govern, bound, observe, verify, promote, revoke, roll back, improve.

**Avoid:** unleash, supercharge, transform everything, autonomous workforce, military metaphors, fear-heavy malware copy, and generic AI-gradient language.

### Visual system

**Concept:** The governed graph.

Use a network of precise lines and nodes to show inheritance and feedback across organization, team, repository, agent, and evidence. Some paths should visibly stop at policy boundaries. Verified paths can brighten or resolve into a stronger line.

**Palette:**

- Ink — `#0A0D12`
- Carbon — `#151A22`
- Warm paper — `#F4F1EA`
- Mimir signal — `#63E6BE`
- Evidence blue — `#74C0FC`
- Controlled warning — `#F6C76A`
- Muted text — `#98A2B3`

The primary experience should be dark, restrained, and high-contrast. Mint is the “approved / active” signal; blue denotes evidence and observability; amber should be reserved for risk or human review.

**Typography:** Use a rigorous grotesk or neo-grotesk for headlines and body copy, paired with a restrained monospace for system labels, benchmarks, policy states, and repository identifiers. Geist with Geist Mono is a strong open-source baseline.

**Imagery:** Prefer product diagrams, policy inheritance, repository maps, signed capability cards, benchmark evidence, and sparse terminal details. Avoid stock developers, humanoid robots, glowing brains, chat bubbles, and decorative code walls.

**Motion:** Lines inherit configuration downward; bounded evidence returns upward; unsafe or unapproved paths stop at a visible policy boundary. Motion should explain the operating model, not provide ambient spectacle.

## Naming system for enterprise capabilities

Use outcome language in the main copy and capability names only as secondary labels:

- **Repository Blueprints** — the approved setup attached to a repository or repository class.
- **Policy Graph** — inherited organization, team, and repository controls.
- **Trust Registry** — approved skills, extensions, MCP servers, versions, permissions, and provenance.
- **Evidence Loop** — privacy-safe operational measurement, comparison, canaries, and rollback.
- **Fleet Learning** — signed, versioned, reversible improvements distributed across teams.

These names should not become five separate mini-products. They are parts of one enterprise agent operating layer.

## Claims and product boundary

### Grounded in the current Mimir runtime

- Safe-Rust runtime with no Node.js requirement.
- Multi-provider adaptation.
- Workspace-rooted file controls and runtime tool policy.
- Default-off process execution and exact allowlists for the narrower process tool.
- Project-isolated sessions and state namespaces.
- Deterministic resource and skill precedence.
- Capability-scoped extensions and explicit unrestricted-native handling.
- Privacy-safe diagnostics and offline verification replay.
- Scoped, reversible continual harness learning.
- Verified canaries and signed fleet learning packs.
- Durable recursive child-agent execution.
- Recorded benchmark reports against Claude Code and Codex.

### Enterprise product direction—do not imply these are shipping until implemented

- A hosted organization control plane and admin UI.
- Automatic repository inventory and ownership mapping.
- Organization and team policy distribution.
- A centrally managed skill, extension, and MCP registry with review workflows.
- Enterprise identity, SSO, SCIM, and role-based access control.
- Central fleet analytics, audit export, and retention policy.
- Remote revocation and staged rollout across managed developer machines.
- Turnkey integrations with source hosts, ticketing, internal knowledge, and security systems.

The landing page can sell this integrated product vision once the enterprise layer is available or clearly announced. Until then, use future-facing language around the control plane and present the current runtime as the technical foundation.

## Metadata

**Page title:** Mimir — Enterprise control for AI coding agents

**Meta description:** Give every engineering team a codebase-aware AI agent setup with centralized governance, trusted capabilities, privacy-safe evidence, and safe, reversible learning.

**Open Graph headline:** Scale agent capability without scaling chaos.

**Open Graph description:** Mimir is the enterprise operating layer for governed, measurable AI coding agents across every team and repository.

## The one sentence to preserve

If the eventual template forces the page to become much shorter, preserve this sentence:

> Mimir gives enterprises one operational layer for deploying, governing, measuring, and improving AI coding agents across every codebase.
