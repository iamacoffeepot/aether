The four `.bin` files pin a *current encoding*. A wire append on the
journal's decisions or event column is expected to move those bytes, and
`cargo xtask fixtures regen` rewrites them in place. Tests only compare;
`cargo xtask fixtures check` reports staleness without writing.

The `pre-*.bin` files are the opposite: rows a previous binary actually
wrote, kept so each registered upcast is exercised against real bytes.
They have no regen command and are never rewritten. An era's pair is the
current-encoding `decisions.bin` and event fixture as they stood at the
last commit of that era, copied before the shape moved — the frozen copy
is what keeps them, because `regen` rewrites the originals.

`pre-coordination-decisions.bin` and `pre-coordination-event.bin` are the
`decisions.bin` and `containment-refused-event.bin` of
`7e623b642dc0d5040eec704ce78853132045c891`, the last commit before shared
verification and eager integration. Raw sha256
`6b2b8f25025b1b7689c5176f82f25ab8d7c181171a1d84e5e87547fceb19b9a4` and
`28a3ba3a8bd3477f805b52cedb7cadd5a575cadaf050f1ae61cb62fddc444a2a`; they
carry the stamps `decisions cd1234d2…` and `event 55af52a6…`.

`schema-digests.txt` pins a *history*: one kind-and-digest line per
persisted shape, oldest first. A shape change appends a line and
registers an upcast. Regenerating this file only appends a newly current
digest; it never drops a prior line. The remedy for a failing digest
test is never a regen command.

The first 10 lines are an independent historical baseline from
`449d0f894c533a6a354270544becd8efb18a3753` (raw sha256
`f5f2be01f6bfa39e41ffb51480f994fa0dd61e639a9bddec3867e36ca2ace86f`).
The digest test pins that prefix in source; it does not compare the
fixture to itself, to git, or to the on-disk path at runtime. Do not
update existing pins to bless rewritten history. Later history may add
independent checkpoints; it must not erase earlier ones.
