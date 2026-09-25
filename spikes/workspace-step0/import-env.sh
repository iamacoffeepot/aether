#!/usr/bin/env bash
# Q3: build a clippy-capable rootfs, export it, stream it into POST /images/create?fromSrc=-.
set -uo pipefail
S=/var/run/docker.sock; D=${SPIKE_SCRATCH:-${TMPDIR:-/tmp}/spike-ws0}; mkdir -p $D; cd $D
ms() { echo $(( $(date +%s%N) / 1000000 )); }
docker rm -f ws0-prep >/dev/null 2>&1
docker run --name ws0-prep rust:1.97-slim rustup component add clippy > $D/prep.log 2>&1; echo "prep exit=$?"
t0=$(ms); docker export ws0-prep > $D/rootfs.tar; t1=$(ms)
echo "export_millis=$((t1-t0)) rootfs_bytes=$(stat -c %s $D/rootfs.tar)"
docker image rm -f spike-ws0-env:v1 >/dev/null 2>&1
t0=$(ms)
code=$(curl -sS --unix-socket $S -o $D/import.body -w '%{http_code}' -X POST -T $D/rootfs.tar \
  -H 'Content-Type: application/x-tar' \
  'http://d/images/create?fromSrc=-&repo=spike-ws0-env&tag=v1&changes=LABEL%20aether.environment.digest%3Dsha256-spike&changes=WORKDIR%20%2Fwork')
t1=$(ms)
echo "import_http=$code import_millis=$((t1-t0)) body_tail=$(tail -c 200 $D/import.body | tr -d '\n')"
# streamed variant: docker export piped straight into the endpoint, no file
docker image rm -f spike-ws0-env:v2 >/dev/null 2>&1
t0=$(ms)
docker export ws0-prep | curl -sS --unix-socket $S -o $D/import2.body -w 'stream_import_http=%{http_code} ' -X POST -T - \
  -H 'Content-Type: application/x-tar' 'http://d/images/create?fromSrc=-&repo=spike-ws0-env&tag=v2&changes=LABEL%20aether.environment.digest%3Dsha256-spike'
t1=$(ms); echo "stream_import_millis=$((t1-t0))"
docker image inspect spike-ws0-env:v1 --format 'image_size={{.Size}} labels={{json .Config.Labels}} env={{json .Config.Env}} workdir={{.Config.WorkingDir}} layers={{len .RootFS.Layers}}'
t0=$(ms)
docker run --rm -e PATH=/usr/local/cargo/bin:/usr/bin:/bin -e RUSTUP_HOME=/usr/local/rustup -e CARGO_HOME=/usr/local/cargo \
  spike-ws0-env:v1 cargo clippy --version; echo "run_from_imported exit=$? millis=$(( $(ms)-t0 ))"
docker rm ws0-prep >/dev/null
