# Historical view rows

`pre-precheck-view-storage.bin` and `pre-precheck-view-positional.bin` were
written through the original `ViewDocument` type before `BloomView.precheck`
existed. The writer used the core source at
`d04707893456077046715e189d2739e80c97646c`, available unchanged in the #5899
worktree. Both fixtures contain one sealed bloom with members `alpha` and
`beta`.

These are historical bytes, not current-schema goldens. Do not regenerate
them after changing the view type. They exercise the queued-row compatibility
decoder in `port/row.rs`, including the positional bloom elements inside a
storage container.

SHA-256:

- Storage: `0af5463862aa10e571c4cedb2050bac346c11fe97255bbfa6ac1be4d27ede66d`
- Positional: `074ca624419dced9a03e247f9d8956749e0cba87e355892e5223e43d81648cb1`
