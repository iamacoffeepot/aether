# docs

Two documentation trees live here and answer different questions. `guide/` is
the mdBook source: how aether works and how to build with it, entered at
[SUMMARY.md](guide/SUMMARY.md) and built with `mdbook build docs` (CI builds it
from `book.toml` and deploys it to GitHub Pages). `adr/` holds the numbered
Architecture Decision Records: why each load-bearing choice was made and which
alternatives were rejected, one file per decision from
[TEMPLATE.md](adr/TEMPLATE.md). Read the guide to use the engine; read the ADR
before changing the subsystem it governs. `evidence-viewer/` and
`pipeline-deck/` are standalone pages outside the mdBook source that CI copies
into the published site after the build; `release/schema.md` records the
issue-body artifacts the contributor workflow is encoded in.
