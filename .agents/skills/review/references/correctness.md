# Correctness Lens

Require a concrete bad input, state, or execution path. Reject speculative hazards prevented by Rust's type or borrow system and observations already decided by a mechanical gate.

## Keepable categories

- `swallowed-error`: a fallible result is discarded, collapsed, or made less informative on a reachable runtime path.
- `missing-bounds-cap`: user-derived recursion, iteration, growth, or arithmetic can exceed a required budget or overflow without a controlled error.
- `silent-incompleteness`: a required branch, match arm, stub, TODO, or no-op leaves behavior incomplete.
- `invariant-violation`: code can construct or transition a type into a state its contract forbids.
- `resource-leak`: a handle, subscription, lane, texture, task, or child lacks release on a success, early-return, or error path.
- `concurrency`: a concrete race, lost update, or lock-order hazard in native substrate or chassis code. Do not apply this category to actor state protected by the run-token model.

## Verification bar

Verify every candidate independently, even at high finder confidence. Read existing tests and the complete path. A passing test that directly pins the allegedly broken behavior refutes the claim. Keep subtle claims uncertain when a read-only review cannot establish them; do not write scratch tests or mutate the worktree during review.
