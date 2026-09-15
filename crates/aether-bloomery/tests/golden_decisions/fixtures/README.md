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

`pre-coalesce-decisions.bin` and `pre-coalesce-event.bin` are the
`decisions.bin` and `containment-refused-event.bin` of
`a12fe3fc4951a2b2890fa29b4a22b5e219383776`, the last commit before
`CoordinationPolicy::coalesce_millis`. Raw sha256
`0dbfcab9761e8887a14fcf76702286c635db1381a0f577ac0950e202f44d2914` and
`28a3ba3a8bd3477f805b52cedb7cadd5a575cadaf050f1ae61cb62fddc444a2a`; they
carry the stamps `decisions a6311d65…` and `event 9e55e67b…`.

`pre-red-verify-decisions.bin` and `pre-red-verify-event.bin` are the
`decisions.bin` and `containment-refused-event.bin` of
`9dd10de5cbb8bc2b36d41bfe570732f6519664d5`, the last commit before
`CoordinationPolicy::red_verify` and `Fact::VerifyFailed::findings`. Raw
sha256 `dd824e31a3e762e070e44ffa56f2aef566763a4963d36defdc71030b652c9d02` and
`28a3ba3a8bd3477f805b52cedb7cadd5a575cadaf050f1ae61cb62fddc444a2a`; they
carry the stamps `decisions b67ebb69…` and `event 37e4b124…`. The event row
is the first whose upcast is not the identity: `findings` is appended inside
`Fact::VerifyFailed` rather than past every discriminant.

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
