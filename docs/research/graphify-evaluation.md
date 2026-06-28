# Graphify Evaluation — nobd-desktop

**Date:** 2026-06-27  **Status:** Research / paused — **recommended pilot repo**
**Tool:** [safishamsi/graphify](https://github.com/safishamsi/graphify) (MIT, by Safi Shamsi)

> One of 5 per-repo notes from the same research session. Siblings (same filename,
> `docs/research/graphify-evaluation.md`) live in: **GP2040-CE, maplecast-flycast,
> mvc2-oracle, mvc2-skin-studio**. The "Shared verdict" section is identical across all five.

---

## What graphify is (1-paragraph)

CLI that turns folders of **code + docs + PDFs + data** into a queryable knowledge
graph. Two extraction tiers:
- **AST tier** (tree-sitter, 36 langs) — runs **locally, zero API cost**, code structure.
- **Semantic tier** (LLM) — sends docs/markdown/PDFs to *your* API key, infers concept
  edges (costs tokens; where our value is).

Outputs: `graph.json` (committable), `graph.html`, `GRAPH_REPORT.md`, and an **MCP
server** (`python -m graphify.serve graph.json`). Edges tagged
`EXTRACTED`/`INFERRED`/`AMBIGUOUS`. Install: `uv tool install graphifyy` then `graphify install`.

---

## This repo's assessment

**nobd-desktop is the recommended first pilot** — it's the smallest repo (18 Rust files
~1.4k LOC, 3 docs ~538 lines), so a full `graphify .` end-to-end run is fast and
near-zero cost. Use it to validate whether the output (`GRAPH_REPORT.md`, `graphify
query`) is worth the token spend before pointing it at the big repos.

Part of the **NOBD input-timing constellation** — the cleanest graphify win, because no
knowledge graph exists in that constellation today and the sync-window algorithm is
**reimplemented across 3 languages**:
- C++ firmware: `GP2040-CE/src/gp2040.cpp` → `syncGpioGetAll()`
- **Rust DLL (here):** `src/sync_window.rs`
- **C++ driver scaffold (here):** `driver/nobd-hid-filter/SyncWindow.h`
- Accumulator design: `maplecast-flycast/docs/INPUT-LATCH.md`

These ports drift. The high-value query a graph unlocks: *"where is the 5ms default
defined, and does every port agree?"* and *"which nobd-website claim is backed by which
nobd-research finding?"*

Domain knowledge worth graphing (currently implicit in code/comments): three impl modes
(Defer/Block/Continuous), three filter milestones (v0–v3), three plug-and-play tiers
(HID filter / xusb22 / ViGEm), and the 20+ stat counters in `app/src/stats.rs` with
subtle interdependencies. Docs: `README.md`, `docs/USAGE.md`,
`driver/nobd-hid-filter/DESIGN.md`.

---

## Shared verdict (identical across all 5 repos)

| Constellation | Repos | Existing graph? | Verdict |
|---|---|---|---|
| **NOBD input-timing** | GP2040-CE, nobd-desktop, nobd-research, nobd-website, maplecast input-latch | **None** | **Strongest, cleanest win** |
| **MVC2 reverse-engineering** | mvc2-oracle, maplecast-flycast, mvc2-skin-studio, mvc2-skin-processor | **Yes — SurrealDB `re_kb`** (curated, provenance + confidence) | **Complement only — do NOT duplicate `re_kb`** |

**Most important finding:** the MVC2 repos already have a *better* graph than graphify
would build for RE facts (`re_kb` with `cites`/status edges). graphify's inferred edges
would be lower-confidence — so in MVC2 repos, graphify is **docs/onboarding only**.

**Risks:** scale blast radius (scope away from `target/`, vendored deps); yet-another-store
risk in MVC2; AST code-graph value modest (doc/concept graph is the draw); git-share
feature muted (solo).

**Overall:** Adopt for the **NOBD constellation**; **docs/onboarding + MCP** for MVC2;
never duplicate `re_kb`.

---

## Where we left off / next steps

1. **Pilot here first** — `graphify .` (exclude `target/`). Inspect `GRAPH_REPORT.md`, try `graphify query`. ~10 min.
2. GP2040-CE docs only: `graphify ./docs --backend claude`.
3. **Cross-repo NOBD graph** (GP2040-CE/docs + nobd-desktop + nobd-research + nobd-website), served as MCP ← the unique-value artifact.
4. MVC2 docs-only, scoped; don't replace re_kb.
5. Decide MCP-first after steps 1–2.

**Nothing installed/run yet.** Prereqs: `uv` + Python 3.10+.
