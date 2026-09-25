# The Rust toolchain every Bloomery environment builds with (ADR-0237
# decision 3). `publish.sh` builds it with a context holding only the
# repository's `rust-toolchain.toml`.
#
# The digest is the pin; the tag only names it for the reader. Its channel must
# be the one `rust-toolchain.toml` names, which the build below enforces. The
# environment merge program selects `usr/local/rustup/toolchains/<channel>-<triple>`
# from this image's tree, so no rustup proxy enters an environment.
FROM rust:1.97.1-slim-trixie@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c

# `rustup toolchain install` with no argument installs the toolchain the
# override file names, with exactly its components and targets. A pinned image
# of another channel would install a second toolchain beside its own, so more
# than one installed toolchain fails the build. `--no-self-update` keeps the
# image's own rustup, so the pin decides it and the build day does not.
RUN --mount=type=bind,source=rust-toolchain.toml,target=/toolchain/rust-toolchain.toml \
    cd /toolchain \
 && rustup toolchain install --no-self-update \
 && if [ "$(rustup toolchain list | wc -l)" -ne 1 ]; then \
      echo "the pinned image's channel is not the one rust-toolchain.toml names; repin FROM" >&2; \
      rustup toolchain list >&2; \
      exit 1; \
    fi
