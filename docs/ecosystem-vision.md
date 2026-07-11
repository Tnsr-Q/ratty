# The ecosystem vision

Design context for the M3 organ designs ([wayfinder map #10](https://github.com/Tnsr-Q/ratty/issues/10)).
This is a curated distillation of the advisory notes committed raw under
[`agent-extensions/`](../agent-extensions/) — analyses of
[The-comune](https://github.com/Tnsr-Q/The-comune), SparrowDB, AirLLM,
llama.cpp/Vulkan/Mojo, and a monitoring-dashboard concept (July 2026).
Nothing here is a commitment; it is the picture the organ designs must not
foreclose.

## The system ratty sits in

Ratty is slated to become the **embodied visual front-end of a multi-agent
memory ecosystem**. The layers divide cleanly:

| Layer | Responsibility |
| --- | --- |
| The-comune / PCKP | Signed knowledge exchange, deterministic convergence, provenance, equivocation evidence |
| SparrowDB | Durable local graph storage, Cypher queries, WAL recovery, indexing |
| Memory coordinator | Salience, consolidation, contradiction detection, promotion, forgetting |
| Dashboard | Monitoring and administrative control |
| **Ratty / Bevy** | **Spatial embodiment and real-time visual projection** |
| Renderers (DOT/SVG/PNG) | Static snapshots, lineage maps, reports |
| Proof/chain layer (optional) | External certification, dispute resolution |

Detail: [`agent-extensions/the-comune.md`](../agent-extensions/the-comune.md).

## Principles worth carrying into every design

- **Replication and belief are separate.** A valid packet replicates
  deterministically; local trust decides whether it surfaces in recall or
  reasoning. "What was asserted" ≠ "what I believe."
- **The log is canonical; views are rebuildable.** The PCKP packet log is the
  event history; SparrowDB (and any ratty projection) is a materialized view
  that can be replayed from it.
- **Contradiction is structure, not noise.** Conflicting claims are retained
  as separate facts linked by a `CONFLICTS_WITH` assertion with its own
  provenance — never flattened into one "current truth."
- **The dashboard observes; the memory service governs; agents propose
  memories but do not unilaterally establish truth.**
- **VRAM is not agent memory.** Compute memory (weights, KV cache, tensors)
  and cognitive memory (facts, experiences, procedures) get separate names
  and separate metrics.
- **Multimodal artifacts are content-addressed.** Image bytes live in a CAS;
  the graph stores identity and provenance; ratty loads textures from the
  store.

## Memory classes

Memory is not one merged list. Five classes, each with its own lifecycle:
**working** (task-scoped, expires fast), **episodic** (append-only what
happened), **semantic** (believed facts with confidence + provenance +
contradiction handling), **procedural** (skills, policies, prompts), and
**shared/social** (workspace state, capabilities, trust scores, spatial
positions — the layer that lets agents perceive each other). The `Remember`
phase is a gated consolidation pipeline (novelty/salience scoring), not an
unconditional save. Detail:
[`agent-extensions/draft-monitor.md`](../agent-extensions/draft-monitor.md).

## Inference temperatures

Three tiers, almost opposite in their optimization targets:

| Tier | Runtime | Latency | Role |
| --- | --- | --- | --- |
| Hot | llama.cpp / Ollama, resident models | ms–seconds | Conversation, tool selection, vision descriptions, embeddings — many concurrent agents |
| Warm | Larger quantized resident model | seconds–minute | Planning, reconciliation, critic agents — selective |
| Cold | AirLLM layer-streaming | minutes+ | Oversized-model batch oracle: overnight consolidation, adjudicating disagreements, deep review |

AirLLM is a **deep, slow, local oracle — not the nervous system**. Its results
enter the system as proposed assessments with provenance, not as truth.
Backend selection: Metal is the default on macOS; Vulkan matters when
distributing to AMD/Intel hardware; Mojo is an experimental kernel laboratory,
used only where a measured gain exists. Ratty's render path (Bevy → wgpu)
already sits on the same cross-platform GPU strategy. Detail:
[`agent-extensions/updated-recomendation.md`](../agent-extensions/updated-recomendation.md),
[`agent-extensions/research.md`](../agent-extensions/research.md).

## Ratty's role: the embodied front-end

Each visible terminal corresponds to one real agent. Orbital position, mood,
confidence, and attention target are state derived from a shared structured
event stream — inference results become components the renderer projects
physically:

| Internal state | Ratty representation |
| --- | --- |
| High confidence | Stronger aura |
| Context nearly full | Pulsing border / compression effect |
| Memory consolidation | Particles flowing into the terminal |
| Attention on another agent | Visible line-of-sight or beam |
| Coordination request | Terminal leans or moves toward peer |
| Contradiction detected | Red split/glitch effect |
| Successful shared memory | Synchronized flash between agents |
| Agent overload | Dimmed display / slower orbital motion |

The graphical behavior gets a semantic basis instead of being decorative.
The [`agent-extensions/monitor.html`](../agent-extensions/monitor.html)
mockup sketches the observability panels (skill registry, agent loop phase,
consolidation jobs, memory channels, execution trace); its notes are explicit
that agents and memory services own the state — any dashboard, ratty
included, **observes, queries, and controls; it is not the store**.

## What this implies for the M3 organs

- **Collaboration** — don't invent a parallel networking layer; the
  replication/provenance substrate is The-comune/PCKP (currently an M0/M1
  scaffold). The first primitive should be designed *against* that trajectory,
  and multi-user trust ("who may speak OSC-777") inherits its
  replication-vs-belief split.
- **Data-viz** — monito-style telemetry (worker progress, queue depth, layer
  streaming, retrieval hit rates) is the target workload, not toy charts.
- **Reactive** — sysinfo is the first sensor set of a larger observability
  role; keep the mapping (sensor → effect) declarative so event-stream
  sources can replace local sensors later.
- **Query channel (OSC 778)** — the closed loop: agents reading ratty state
  as structured components is the inverse of the event stream above; schema
  design should anticipate both directions.
- **Avatar / presence** — embodiment of internal state (the table above) is
  the value; a glTF character is one possible skin over it, not the point.
- **Panes** — many agents means many terminals in one space; pane design
  should not assume a single PTY-owner worldview.
- **Sound** — same principle as visuals: semantic basis (state transitions,
  coordination events), not decoration.
