A wire append on the journal's decisions or event column is expected to
move these bytes — they pin the current encoding, and
`cargo xtask fixtures regen` is how they move. Tests only compare;
`cargo xtask fixtures check` reports staleness without writing.
