# Contributing to Aether

Thanks for your interest in contributing. A couple of things to know before you open a PR.

## Licensing of contributions

Aether is dual-licensed under [MIT](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE), at the recipient's option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## Developer Certificate of Origin (DCO)

Every commit must be signed off. The sign-off is your certification, under the
[Developer Certificate of Origin](https://developercertificate.org/) (v1.1),
that you wrote the contribution or otherwise have the right to submit it under
the project's license. It is a lightweight attestation, not a copyright
assignment — you keep the copyright in your work.

Add the sign-off with `-s`:

```
git commit -s -m "your message"
```

That appends a trailer to the commit message:

```
Signed-off-by: Your Name <your@email.example>
```

If you wrote the contribution on an employer's time or equipment, confirm you
actually have the right to contribute it before you sign off — and when in
doubt, get your employer's sign-off too. The DCO is only as good as the
representation behind it.

## Before you push

The repository's pre-flight mirrors CI:

```
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

See `CLAUDE.md` and `scripts/preflight.sh` for the full local pre-flight (it
also stamps the commit so the pre-push hook short-circuits).
