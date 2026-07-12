# Plain and Under Modes

Use this reference for a normal wish pass and for drilling one persisted chosen or alternative branch. Work inline in the main Codex thread; these modes do not need subagents.

## Plain pass

1. Parse the theme and optional role. With only `--as <role>`, survey that role's grounded Aether adversity. If neither a theme nor a role can be inferred, ask one concise question in the final response and stop until the user answers. If the theme is too broad to ground or produce a coherent tree, ask for a narrower outcome instead of generating generic roots.
2. Resolve the shared output root from the common contract and create a new dated theme directory. If that exact slug already exists, do not overwrite it silently; extend it only when the user's request clearly targets that pass, otherwise choose a disambiguating slug and report it.
3. Scan the available adversity sources selectively. Use REST issue reads rather than GraphQL convenience commands. Record a compact source digest for the index.
4. Generate one to three outcome-level roots. Record data versus empathy grounding and drop ungrounded candidates.
5. Recursively drill each root:
   - articulate the satisfying shape at the current level of detail;
   - verify every claimed existing engine surface;
   - decide producibility;
   - name alternatives with all path-cost dimensions;
   - state doors opened and closed;
   - verify that children compose upward;
   - recurse through genuine absences until every retained leaf is producible or explicitly resource-bound.
6. Filter the completed tree against existing work using the common contract. Do not silently drop interior nodes.
7. Write each `wish.md` and the navigation `index.md` with `apply_patch`. Re-read the resulting tree and verify frontmatter, parent links, leaf counts, and cited paths.
8. Report the theme, tree path, root/node/interior/leaf counts, depth range, alternative counts, adversity-source counts, considered-and-dropped count, and any resource-bound nodes.

End with Codex-native next actions, for example:

```text
Read the index and the branches that interest you.
Drill a branch or alternative: $wish --under <wish-path>
File a chosen leaf: $sketch --from-wish <leaf-path>, then use $scope <issue-number>.
```

Do not invoke those follow-up skills automatically.

## `$wish --under <wish-path>`

1. Resolve the path beneath the shared checkout's `wishes/` corpus. Refuse traversal outside that corpus.
2. If the target exists, read its `wish.md`, its ancestor chain, and its existing descendants before designing additions. Recheck its cited surfaces first. If grounding drifted, mark it through the refresh contract and stop so the user can choose whether to redesign stale work.
3. If the target is a not-yet-materialized `alternatives/<slug>` path, read the parent `wish.md` and match `<slug>` to a prose-named alternative. Materialize it only when the match is unambiguous. If neither an existing node nor a named alternative matches, refuse and list valid nearby paths or alternatives.
4. Treat the target shape as the chosen path for this pass. Drill its own alternatives and children recursively to producibility using the common contract.
5. Preserve all unrelated nodes and prior index history. Update the tree's navigation counts, summaries, depth, materialized-alternative count, considered-and-dropped list, and notes rather than replacing the whole index.
6. Re-read the changed subtree and parent linkages, then report the target, new nodes and leaves, remaining absences, drift findings, and index path.

`--under` never files an issue or changes a prior `filed:` value.
