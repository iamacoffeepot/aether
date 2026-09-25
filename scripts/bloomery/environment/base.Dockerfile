# The distro userland every Bloomery environment starts from (ADR-0237
# decision 3). `publish.sh` builds it and `check.sh` proves the package list.
#
# The digest is the pin; the tag only names it for the reader. Trixie is the
# release the pinned `rust:<channel>-slim-trixie` image in
# `toolchain.Dockerfile` is built on, so both trees share one libc.
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

# The packages the workspace's build scripts need, and nothing a fetch needs:
# `check.sh` fetches crates in the toolchain image and checks offline here.
#
#   build-essential  cc, c++, make, and libc headers for the `cc` crate builds
#   pkg-config       the `pkg-config` crate's probe for system libraries
#   libasound2-dev   alsa-sys links libasound through pkg-config (cpal audio)
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential \
      pkg-config \
      libasound2-dev \
 && rm -rf /var/lib/apt/lists/*
