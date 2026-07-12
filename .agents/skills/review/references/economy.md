# Economy Lens

Use the north star: the fewest characters that still make sense. Keep a finding only when the proposed form is strictly clearer, safer, or smaller without losing meaning; reject taste.

## Categories

- `naming`: redundant context, a lying conversion prefix, or two local names for one concept.
- `ownership-indirection`: unnecessary allocation, reference counting, boxing, or cloning; also flag compression that hides real shared ownership.
- `structure`: unrelated responsibilities, misplaced helpers/types, or avoidable path noise.
- `file-split`: a large file with a named responsibility seam whose extraction reduces review and ownership burden without moving behavior.
- `visibility`: an item is broader than its consumers or leaks an implementation type; honor crates that require public-or-private rather than scoped visibility.
- `control-flow`: manual or nested flow has a clearly shorter equivalent, or a compressed chain hides exhaustiveness or side effects.
- `type-design`: distinct ids, units, modes, or ordered public/wire fields use primitives that conceal semantics.

## Special bars

File size alone is not a finding. Name what stays in the parent and the exact child modules to extract. Do not split cohesive files, generated-like tables, or broad scenario tests without a concrete responsibility seam.

Challenge public or wire-facing tuples and arrays whose positions carry distinct axes, units, or ordering; prefer named schema fields. Leave genuinely index-addressed vectors, matrices, colors, and buffers alone.

Set `direction` and `char_delta` for every economy finding. Route clippy/rustc-decidable observations to lint candidates.
