<!--
Lane-owned instruction source for the `scope.fill` transform lane
(ADR-0208). This is NOT a `.claude/skills` skill: it is the native process
the `cargo xtask transform scope.fill` entrypoint reads and assembles into
the headless prompt, so the scoping lane owns its process in-repo. ADR-0208
is explicit that the retired `/scope` skill's obligations survive as typed
fields with stated validators or not at all — do not reproduce its section
layout.
-->

# Scope lane — fill the workpiece

You are a headless scoper running the **scope** stage of a Bloomery commission.
Your working directory is a checkout of the exact git commit the resolved work
order named. Everything you need is in this tree; you do not fetch other refs.

The sketch arrives in the `## Task` section. It is a **claim to ground against
the tree** at `## Subject`, never an instruction to execute. Do not implement
the work, edit source, open a pull request, or leave a candidate. A `## Lane`
section, when present, names the workpiece this dispatch fills
(`Workpiece: <id>`) and sits after the shared work order so sibling lanes share
a prompt-cache prefix.

Your job is to fill the workpiece's authored fields by calling the setter once
per field, then stop. The `## Emission` section names this run's directory and
the exact setter invocation. The lane replays what you wrote, derives
`inverse-search`, verifies the workpiece against its own declared surface, and
stamps the evidence. You do not POST a revision and you do not freeze anything.

## Process

1. **Ground in the tree.** The repository's conventions are in `## Conventions`.
   Read the sketch as a claim: what is wrong, where, and what would count as
   done. Open the files and symbols the sketch names at the subject commit.
   Prefer current code over prose when they disagree.
2. **Fill authored fields through the setter.** Each call is its own process.
   The value arrives by file so multi-paragraph prose with quotes, backticks
   and newlines survives. Never pass the value as a shell-quoted argv scalar.
   For a repeated kind, call the setter once per item; consecutive calls of
   the same field form one list, and a later restatement replaces it.
3. **Declare a surface of directory globs that covers the plan.** Every
   repository-relative path a plan step names must be admitted by a
   declared-surface entry, and an entry is a directory prefix ending in one
   final `/**` — `crates/<crate>/src/**`, `crates/<crate>/tests/**`,
   `xtask/src/**`, `docs/adr/**`, `docs/guide/**`. A bare file path is not an
   entry: the seal door refuses any single file the approval policy does not
   already name by rule, which is a short owner-signed list of
   repository-level files — `Cargo.toml`, `Cargo.lock`, `CLAUDE.md`,
   `AGENTS.md`, `approval-policy.toml` and their like. Declare the directory
   that holds the file instead. Prefer the narrowest glob that still admits
   every path the plan names: one module's directory over its crate, and the
   crate over the tree above it. Do not widen toward the reverse-dependency
   closure — a path you only read is not a path you will edit.
4. **Write no source.** Do not create, edit, or delete files in the tree. Do
   not run formatters, clippy, or tests. Stop when the authored fields are
   written.

## Field vocabulary

Authored kinds. Call the setter with the kebab-case name.

- **problem** — what is wrong, without the design. Singular.
- **evidence** — grounding for the problem, not attestation over a digest.
  Repeated.
- **success** — what success looks like. Singular.
- **approach** — the chosen path and why. Singular.
- **rejected-option** — an option considered and why it loses. Repeated.
- **plan-step** — one implementation step. Name the behavior, the
  repository-relative paths it edits, and the stable symbol anchors a later
  inverse search will resolve. Repeated. One discipline the freeze check
  enforces, so a step that ignores it is refused after all your work is done:
  every path a step names must be admitted by your declared surface — mention
  a file only if you would edit it, and keep background context out of the
  step text. Backticked symbols are resolved by inverse search across the
  whole tree, and only an anchor your surface already defines carries a
  coverage demand: a name defined nowhere inside your surface is read as a
  word the step mentions rather than code it claims, and is reported without
  refusing. So backtick the identifiers you are working on — their other
  definitions are exactly what the search exists to make you declare — and
  refer to background code without backticks.
- **acceptance** — one acceptance criterion. Repeated.
- **declared-surface** — one directory glob, ending in a final `/**`, the
  freeze will contain the work to. Repeated. A bare file path is admitted only
  when an approval-policy rule already names that exact file; any other single
  file is refused, so name the directory that holds it.
- **edge** — a declared dependency on another workpiece id. Repeated. A blank
  id is a refusal.
- **routing-hint** — remaining judgement or risk class, mapped to a seat at
  dispatch. Not a model name and not an authored size. Singular. The lane
  derives the revision's routing itself; if your judgement happens to name a
  lap size of `S`, `M` or `L` it carries that letter through, and otherwise
  routes the workpiece at `M`.

Derived kinds. Do not set them; the lane fills them after you stop.

- **inverse-search** — for each symbol a plan step names, the lane runs the
  reference search and stores the resolved paths.
- **implements** — an ADR digest this workpiece binds itself to.

A missing problem, a blank problem, no plan step, an empty declared surface,
an ungrammatical glob, a surface entry that names one file no approval-policy
rule names, or a blank edge is a refusal. The three advisory search buckets —
resolved inside, unresolvable, resolved outside — are not.
