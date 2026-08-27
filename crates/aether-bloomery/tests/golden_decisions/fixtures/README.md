The four `.bin` files pin a *current encoding*. A wire append on the
journal's decisions or event column is expected to move those bytes, and
`cargo xtask fixtures regen` rewrites them in place. Tests only compare;
`cargo xtask fixtures check` reports staleness without writing.

`schema-digests.txt` pins a *history*: one kind-and-digest line per
persisted shape, oldest first. A shape change appends a line and
registers an upcast. Regenerating this file only appends a newly current
digest; it never drops a prior line. The remedy for a failing digest
test is never a regen command.
