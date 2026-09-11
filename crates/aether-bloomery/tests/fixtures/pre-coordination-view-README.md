# View rows before coordination

These frozen rows were generated using the retained pre-coordination
`aether-bloomery` library, before compiling issue #5903. The producer asserted
the Event and Decisions schema stamps below, matching main
`7e623b642dc0d5040eec704ce78853132045c891` (PR #5902). It decoded the existing
pre-precheck fixture, added a prepared precheck to its first bloom, and appended
a second bloom without a precheck. Both use the original `ViewDocument` type
and its real storage/positional encoders. They were not encoded through the
new compatibility structs.

- Event: `55af52a6acdc3e695b0da93f05a73b414485a6526a863e1ab4129a42be788052`
- Decisions: `cd1234d263eb61be6bacc167ac49bd83894c505b527726061d6758e3b19cbc24`
- Storage: 962 bytes, SHA-256 `de05b5a32fb7c21777ae66cdb5aff5e199139c51bb5cf5c82969a88e759232a9`
- Positional: 888 bytes, SHA-256 `01d9be173c067ee255f45b94cab87f1e1717e08dafd833df639a15310a2c972e`

Keep these bytes unchanged. New schema eras append new fixtures. The old
pre-precheck fixtures remain separate and unchanged.
