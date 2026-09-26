# A source checkout as an image, so the `aether.workspace` actor can import it
# by digest (ADR-0237 decision 3). `publish.sh` builds it over a checkout
# directory; the image reference is the only thing that crosses into the
# engine, never a host path.
#
# `source.Dockerfile.dockerignore` beside this file decides what the context
# holds: a fail-closed allowlist of the roots cargo and the `cargo xtask` lanes
# read. The checkout lands under `/source`, apart from the placeholders Docker
# adds to every container, and the `source.select` program in
# `aether-bloomery-workspace-programs` takes that one entry from the imported
# tree. The two spell the same name, so rename both or neither.
FROM scratch
COPY . /source
