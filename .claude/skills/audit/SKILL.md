---
name: audit
description: Run a code-quality scan on a single workspace crate and write a narrative audit report under `/audits/`. Invoke as `/audit <type> <crate>` where <type> ∈ {dup, unused, concurrency, stale} and <crate> is a workspace member (e.g. `/audit concurrency aether-substrate`). After the report writes, prompts which findings to file as GitHub issues. Use when a crate has grown enough to accrete smells worth a thoughtful read.
---

# Audit skill

Category-driven code-quality scan over a single workspace crate. Each type targets a different class of smell — duplicated code, unused symbols, concurrency primitive misuse, stale comments + magic-string duplication — and produces a narrative audit report under `/audits/`. After the report lands, the user picks which findings warrant GitHub issues.

The report shape matches the prior hand-written audits already in `/audits/`: executive summary, architecture observed for the area, then detailed findings with stable IDs that issue-filing can reference.

**Symbolic only — no regex pattern matching.** Findings come from RustRover's symbol resolver, not text grep. The IDE knows when `Slot<T>` aliases to `Arc<Mutex<Option<T>>>`; a regex doesn't. Every category's procedure leans on `mcp__rustrover__*` symbol/PSI tools as the primary path; textual search appears only when the question is genuinely textual (e.g. "is this name mentioned in a comment?").

## Arguments

`<type>` — one of: `dup` | `unused` | `concurrency` | `stale`.
`<crate>` — a workspace member name (e.g. `aether-substrate`, `aether-capabilities`); must resolve via `cargo metadata --no-deps`.

Example: `/audit concurrency aether-capabilities`

If `<type>` or `<crate>` is missing or unknown, stop and surface the gap — don't guess.

## Procedure

1. **Validate inputs.** Check `<type>` against the four known categories. Resolve `<crate>` via `cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "<crate>") | .manifest_path'`. If either fails, surface what's wrong and stop. Verify `mcp__rustrover__*` tools are available in the session; if not, stop with "RustRover MCP plugin must be enabled — bring up RustRover with the project open and retry." The skill cannot degrade to grep-only; the symbolic engine is load-bearing.

2. **Resolve crate root.** Strip `/Cargo.toml` from the manifest path. The scan walks `src/` for every type, plus `tests/` for `dup` and `unused` (dead test code matters).

3. **Run the type-specific scan** (see per-type procedures below). Each scan emits a list of findings with stable per-type IDs — `D<n>` (dup), `U<n>` (unused), `C<n>` (concurrency), `S<n>` (stale) — assigned in file:line order so re-running the scan produces stable references unless the source moved.

4. **Write the narrative report.** Target path: `audits/<CRATE_UPPER>_<TYPE_UPPER>_AUDIT_<YYYYMMDD_HHMMSS>.md`. The `<CRATE_UPPER>` is the crate name uppercased with hyphens → underscores (`aether-substrate` → `AETHER_SUBSTRATE`). `<TYPE_UPPER>` is one of `DUP` / `UNUSED` / `CONCURRENCY` / `STALE`. The timestamp is local time.

   Section layout (mirrors the existing `/audits/CODE_*_AUDIT_*.md` reports):

   ```markdown
   # <Type human name> Audit — <crate>

   Audit timestamp: <YYYY-MM-DD HH:MM:SS Zone>
   Repository: `aether`
   Crate scope: `crates/<crate>` (or `demos/<crate>`)
   Git head: `<short-sha>` (or `<short-sha>-dirty` if working tree is dirty)
   Tooling: RustRover MCP — list of `mcp__rustrover__*` tools actually called this run.

   ## Scope

   Two or three sentences describing what this audit type catches and what it does NOT cover. Explicit about FP risk for the `stale` type.

   ## Executive Summary

   <n> findings; <n_high> high-severity, <n_medium> medium, <n_low> needs-review.

   One paragraph naming the dominant pattern across findings (e.g. "Three of four findings cluster around `Arc<Mutex<...>>` cap state — a retrofit pattern from issue 629 that ADR-0038 was supposed to retire").

   ## Architecture Observed

   Two to four short subsections walking the crate's relevant structure for the audit type. For `concurrency`: synchronization primitives in use, thread model, channel boundaries (built from `search_symbol` enumerations). For `dup`: module layout + which areas grew copy-paste fastest. For `unused`: pub surface inventory (from `search_symbol` filtered to `pub`). For `stale`: any ADR-superseded module names still present. Cite files inline (`file:line`).

   ## Findings

   ### <ID> — <one-line summary>

   - **Severity**: high | medium | low
   - **Location**: `<file>:<line>` (clickable file path)
   - **How surfaced**: which RustRover tool + which inspection / query returned this finding (e.g. "`get_file_problems` → `RsDeadCode`", or "`search_symbol` for `Mutex` filtered by `get_symbol_info` resolving to `std::sync::Mutex<Option<…>>`").

   Two or three paragraphs of context: what the code does, what the pattern catches, why it's flagged. Quote 3–8 lines of source when the shape isn't obvious from a one-line summary.

   **Suggested action**: one paragraph; either a concrete fix or "needs human read because <reason>".

   ### <ID> — ...
   ```

   `audits/` is gitignored — reports live on disk locally. Issues filed from them link to source files (not the report path).

5. **Prompt for issue filing.** Print the report path and a short summary (number of findings per severity), then use `AskUserQuestion` with a multiSelect listing each finding by `<ID> — <one-line summary>`. AskUserQuestion caps at 4 options per question; for >4 findings, batch via multiple AskUserQuestion calls or fall back to "reply with a space-separated ID list (e.g. `C1 C3 C7`)".

   For each chosen finding:
   - **Title**: `<commit-type>(<crate>): <finding summary>` where `<commit-type>` per category is `refactor` (dup, unused, concurrency, stale-magic-string) or `chore` (stale-comments, pure deletion).
   - **Body**: paste the finding's report section verbatim, then a footer:
     ```
     ---
     Surfaced by `/audit <type> <crate>`. Local report:
     `<audits/...>` (not committed; report lives in the gitignored `/audits/` dir).
     ```
   - **Labels**: `triage`, `crate:<crate-short-name>` (matches the CI auto-labeler's convention), and `audit:<type>`.
   - **Cross-issue refs**: use the cross-repo `iamacoffeepot/aether#NNN` form per the project's pr-body hook (memory `feedback_close_keyword_hook_strips_hash`).

## Per-type procedures

Each procedure starts with the RustRover tools it relies on and the symbolic queries it issues. None of them grep for type shapes — text search appears only where the question is literally about text (e.g. is a name mentioned in a comment).

### `unused` — unused fields, methods, items

**Tools**: `mcp__rustrover__get_file_problems`.

For each file under `<crate>/src` and `<crate>/tests`, call `get_file_problems` with `errorsOnly: false`. The tool returns problems as `{ severity, description, lineContent, line, column }` — there's no structured category tag, so filtering happens on the description string. Keep problems whose `description` matches a case-insensitive `Unused | Dead | Never used | does nothing | unreachable | redundant import | never read` substring. Each surviving problem becomes a `U<n>` finding. Severity mapping: `WARNING` and above → high, `WEAK WARNING` → medium, `INFORMATION` → low (the resolver is symbolic — false positives here are rare and almost always intentional `#[allow]` candidates).

The report's Architecture Observed section lists the crate's module + pub surface (just by reading `src/lib.rs` and `src/*.rs` headers — no symbolic enumeration needed) so the reader sees the denominator before reading findings.

**Deferred from v1 — cross-crate pub-but-unimported pass.** RustRover's built-in "Unused declaration" inspection conservatively skips `pub` items (since `pub` declares external API intent), so an item that's pub and only used within the same crate doesn't surface in `get_file_problems`. Properly finding cross-crate unused pubs requires either a workspace-PSI walk via `run_inspection_kts` (expensive per-name; needs verification that the API exposes reference lookup) or a textual `use <crate>::` reconciliation (drops the symbolic guarantee). Surfaced during the skill's first smoke against `aether-codec`; deferred to a future PR rather than shipped with a shaky implementation.

Commit type when filing: `refactor` (or `chore` for pure deletion of dead code with no behavioural surface).

### `concurrency` — concurrency primitive smells

**Tools**: `mcp__rustrover__run_inspection_kts` (primary — PSI + resolver walks per pattern), `mcp__rustrover__generate_inspection_kts_api` + `generate_inspection_kts_examples` (consulted when writing or revising a pattern's kts script).

The patterns in this category ask questions of the form "find every struct field whose resolved type tree matches X" — that's field-type enumeration over PSI, not name-fragment lookup. `search_symbol` returns hits by name; it can't be inverted to "give me all fields of resolved type Y". So the primary path is a per-pattern kts script the audit runs via `run_inspection_kts` against each file in `<crate>/src`.

The pattern catalogue lives in `patterns/concurrency.md` (sibling of this file); load it and apply each pattern in order. Each pattern entry specifies:
- The **symbolic question** it asks (e.g. "find every field whose resolved type is `Arc<Mutex<Option<T>>>` for any `T`").
- The **kts script** path under `patterns/kts/<pattern-id>.kts` — the script does the PSI walk + resolver query.
- An **expected output shape** so the aggregator can convert per-file results into clustered findings.

Each surviving hit becomes a `C<n>` finding; per-pattern severity + suggested-fix text live in the catalogue.

Architecture Observed for `concurrency` is itself generated by a small inventory kts script (`patterns/kts/inventory.kts`) that walks struct fields + function calls and emits a per-crate primitive census (Mutex count, RwLock count, channel-creation sites with bounds, thread-spawn sites). The findings paragraph then references the inventory.

**Honest limitation**: the kts scripts haven't been verified against Rust PSI yet. The first run of `/audit concurrency <crate>` is the verification — `run_inspection_kts` returns compilation errors if a script can't reach the Rust PSI types it needs. If a pattern's script fails to compile, that pattern is flagged `status: deferred — kts unsupported` in the catalogue rather than degrading to a regex fallback; better to under-report than to silently switch off the symbolic guarantee.

Until a `/audit concurrency <crate>` run successfully populates findings end-to-end, treat the concurrency category as v1-pending and skip it for routine use. The simpler `unused` and `dup` categories are unblocked today via `get_file_problems`.

Commit type when filing: `refactor`.

### `stale` — stale comments + magic-string duplication

**Tools**: `mcp__rustrover__search_symbol` (resolves whether a name is still a live symbol), `mcp__rustrover__search_in_files_by_text` (only for the genuinely textual question "does this name appear in a comment").

Two passes:

1. **Retired-symbol references.** Pattern list at `patterns/stale-symbols.md` — symbols that retired or renamed, with their retirement PR/issue and successor. For each entry:
   - Call `search_symbol` for `<symbol>` scoped to `<crate>/**`. If the symbolic resolver returns a hit, the symbol is still alive in this crate — that's a `S<n>` finding (someone reintroduced or never removed an identifier the catalogue marks as retired). Severity: high.
   - If the symbolic resolver returns no hit, fall through to `search_in_files_by_text` for the name. Hits that aren't inside an annotated history marker (`// historical:`, `// pre-#NNN`, `// ADR-NNNN superseded by`) become `S<n>` findings — the name only appears in comments/strings. Severity: medium.
   - The finding body cites the retirement PR + successor.

2. **Magic-string mailbox names.** For each `<Cap>` cap available in the crate (enumerated via `search_symbol` for symbols ending in `Capability`), call `get_symbol_info` on the cap to read its `NAMESPACE` const. Then `search_in_files_by_text` for that exact string literal under `<crate>/src`. Hits inside files that import the cap but use the literal instead of `mailbox_id_from_name(<Cap>::NAMESPACE)` → `S<n>` finding (severity: medium). Hits inside files that don't import the cap are surfaced as needs-review (severity: low — verify intentional literal).

The `stale` type has the highest FP rate among the four. The report's Scope section calls this out explicitly so the reader applies extra skepticism, and the AskUserQuestion prompt at the end defaults severity-low findings to unchecked.

Commit type when filing: `refactor` for magic-string findings, `chore` for stale-comment cleanup.

### `dup` — duplicated code

**Tools**: `mcp__rustrover__get_file_problems` (primary), `mcp__rustrover__get_symbol_info` (used when aggregating clusters across files).

Verified during the skill's first smoke (against `aether-codec`): RustRover's built-in Duplicates inspection runs as part of `get_file_problems` and returns symbolic findings of the form `"Duplicated code fragment (<N> lines long)"`. No custom kts script needed for v1 — the IDE's PSI-driven dup detector already does the work the audit was going to script by hand.

1. For each file under `<crate>/src` and `<crate>/tests`, call `get_file_problems` with `errorsOnly: false`. Filter problems whose `description` matches `Duplicated code fragment`. Each problem carries the file + the starting line of one duplicate; aggregate across files into clusters by parsing the `(<N> lines long)` length and locating the matching block in the second file via `search_symbol` for the leading identifier on the duplicated lines.
2. Each cluster of size ≥ 2 across distinct functions becomes a `D<n>` finding. Severity heuristic: the IDE inspection reports `WEAK WARNING` for every dup it finds — promote to medium if the cluster spans ≥ 15 lines, medium-low otherwise. (The dup-finding shape doesn't currently carry confidence — RustRover's inspection either matches or doesn't.)
3. Architecture Observed for `dup` lists the crate's function inventory (counts by module) so the reader sees the denominator.

For dup clusters that need richer detection — semantically-equivalent functions written with different variable names, or cross-crate dup — promote to a custom kts script under `patterns/kts/duplicates-deep.kts` and ride `run_inspection_kts`. Deferred until v1's built-in pass proves insufficient.

Commit type when filing: `refactor`.

## Constraints and notes

- The skill is per-crate by design. Workspace-wide scans cost too much per-run and lose narrative coherence; if a smell pattern truly needs cross-crate visibility, a follow-up issue can capture it without forcing this skill to grow that mode.
- RustRover MCP tools are required. If the session doesn't have `mcp__rustrover__*` available, the skill fails fast in step 1 — no silent grep fallback. The IDE inspections + symbol resolver are load-bearing; the whole skill's value over a `rg`/`cargo` script is the symbolic precision.
- `get_file_problems` default timeout is 60 seconds, which can time out on large files (e.g. `crates/aether-substrate/src/lib.rs`). The per-type procedures should pass an explicit `timeout: 120000` for any file ≥ 500 lines and retry once at `300000` before giving up. A timed-out file becomes a skipped-file note in the report's Tooling line, not a finding.
- Pattern files (`patterns/concurrency.md`, `patterns/stale-symbols.md`) and inspection scripts (`patterns/kts/*.kts`) are append-only — adding patterns or refining a kts script extends the skill without procedure changes. Mark deprecated patterns inline (`status: deprecated — <reason>`) rather than deleting; the history matters for re-running old reports.
- The report path lives under `/audits/` which is gitignored — reports are local artifacts. Issues filed from a report link to source files, not the report path.
- Don't auto-file findings. Every issue created from an audit goes through the user's explicit selection step, even when severity is high — false positives in any category cost more than the friction of confirming.
- For findings whose suggested action is design-bearing (touches an ADR-load-bearing primitive, breaks a public API), the filed issue body should say "design decision required — not `agent`-eligible" rather than presenting a mechanical fix. The `delegate` skill explicitly bails on design-bearing issues; the `audit` skill should not feed it any.
- After filing, the report stays on disk. If you re-run the same audit type against the same crate later, IDs reset (assignment is per-run) — issues filed from the prior run reference the prior report path in their footer, which still works to find the local file.
- Pattern catalogue maintenance is manual today. When retiring a symbol or shipping a new concurrency anti-pattern, append to the relevant `patterns/*.md` file in the same PR. A future `/audit-update-patterns` companion that scrapes recent merge commits is parked until the manual cadence hurts.
