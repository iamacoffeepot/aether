# Refresh and Survey Modes

These modes operate on persisted trees under the shared checkout's `wishes/` corpus. Refresh updates local grounding metadata. Survey reads GitHub issue state and updates local index history; it never mutates GitHub.

## `$wish --refresh <tree-path>`

1. Resolve the target to an existing tree or subtree beneath the shared `wishes/` root. Refuse traversal outside it. If it does not exist, list valid persisted tree paths.
2. Fetch `origin main`, capture one SHA for the refresh, and walk every `wish.md` below the target, including alternatives. Parse each `grounded_surfaces` entry in the required `` `identifier` — crates/aether-*/src/path.rs `` form.
3. Recheck each identifier against its cited path at that captured SHA with `git grep -F`. A missing path or empty match is drift. Do not guess a replacement. If the fetch or ref read fails, report a partial refresh and do not change grounding metadata.
4. For a node with any drift, use `apply_patch` to set `grounding_stale: true`, set `drifted_surfaces` to exactly the failed citations, and set `grounding_checked` to today's date.
5. When every citation still resolves, set `grounding_stale: false`, remove any old `drifted_surfaces`, and update `grounding_checked`.
6. A node with no `grounded_surfaces` is unverifiable. Report it without adding stale or checked fields. Drift is per node; a stale child does not automatically stale its ancestors.
7. Preserve the prose body and every unrelated frontmatter field. This mode detects drift; it never redesigns or redrills a node.

Report checked, current, drifted, and unverifiable counts, followed by each drifted node and failed citation. Recommend `$wish --under <path>` only as a user-chosen follow-up.

## `$wish --survey [<tree-path>]`

With no path, survey every `wishes/<date>-<theme>/` tree under the shared checkout. With a path, survey exactly that persisted tree. A missing or empty corpus is a clean no-op: report `no wish trees to survey` and create nothing.

### Identify leaves

A leaf is a `wish.md` whose frontmatter has `producible: true` and whose directory has no nested chosen-path wish directory. Exclude `alternatives/` from that child-directory check. A tree with zero leaves is valid.

Parse `filed` strictly:

- absent means `open` and requires no network call;
- a present value must be the quoted positive-decimal form `"#N"`;
- malformed values are invalid, not open, and must not be coerced.

### Read issue disposition

For each valid filed number, make one shaped REST read of `repos/iamacoffeepot/aether/issues/N` through `gh api` under the GitHub workflow contract, selecting `state`, `state_reason`, and `pull_request`. A response containing `pull_request` is an invalid `$sketch` filing. Derive:

- `filed`: issue is open;
- `landed`: issue is closed with `state_reason: completed`;
- `stale`: issue is closed with `state_reason: not_planned`.

A missing/inaccessible issue, unexpected closed reason, malformed response, or PR target is `unknown`/invalid for reporting and does not enter the four-way roll-up. Never infer stale from a failed lookup. An optional strict landed check may inspect the timeline, but the default survey does not spend that extra request.

If GitHub throttles or a read failure makes the survey incomplete, continue collecting what is safe, mark the tree partial, list unresolved leaves, and do not replace or append a dated index snapshot.

### Report and update `index.md`

For every complete tree, print a table with leaf slug-path, quoted filed value or `—`, and disposition, plus counts for open, filed, landed, and stale. Report invalid/unknown leaves separately.

Write the same result under `## Disposition survey (YYYY-MM-DD)` in the existing tree `index.md`:

- On the same date, replace every existing block for that date—from its H2 through the next H2 or end of file—with one current complete block.
- On a later date, preserve earlier survey history and append one block.
- Preserve every non-survey byte of the index.

A tree without `index.md` is malformed; report it and do not synthesize one. A complete zero-leaf tree still receives a dated zero-count survey. Survey never writes disposition into leaf frontmatter and never changes grounding fields or GitHub state.
