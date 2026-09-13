These two immutable rows were encoded by the previous Bloomery source at
`d04707893456077046715e189d2739e80c97646c` (materialized in issue-5899;
its later reconcile-session-only changes do not touch the view or codec).
The writer sealed two members, alpha and beta, through the public reducer
and encoded `view_of` through `encode_row`, once with the storage kind name
and once with the positional identity. They are history, not regenerable
current fixtures.

SHA-256:
- storage: `0af5463862aa10e571c4cedb2050bac346c11fe97255bbfa6ac1be4d27ede66d`
- positional: `074ca624419dced9a03e247f9d8956749e0cba87e355892e5223e43d81648cb1`
