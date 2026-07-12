# Spec-Fidelity Lens

Judge only the delta between the verified issue scope and the complete change.

## Keepable categories

- `over-delivery`: the change adds generality, API, configuration, abstraction, error surface, or behavior the scope did not ask for.
- `under-delivery`: a named acceptance criterion or scoped error case remains missing, stubbed, or incomplete.
- `scope-leakage`: a changed file or symbol is unrelated drive-by work.
- `silent-deviation`: the change implements a materially different approach without returning the decision to scope.

## Bar

Keep a finding only when it names what the scope asked for, what the change actually does, and the concrete mismatch. A refactor implied by the requested implementation is not leakage. In-scope ugliness belongs to economy, an in-scope bug to correctness, and an in-scope repository-rule violation to convention.

List truly unrelated files in `outOfScope` as absolute paths. Also report each as a `scope-leakage` finding. Do not prune a file merely because the issue failed to name every mechanical call site.
