#!/usr/bin/env bash
# Q4b': cold clippy on a small crate with vendored deps (serde + serde_json, derive), through the archive loop.
set -uo pipefail
S=/var/run/docker.sock; D=${SPIKE_SCRATCH:-${TMPDIR:-/tmp}/spike-ws0}; mkdir -p $D; cd $D
IMG=spike-ws0-env:v1
ENVJ='["PATH=/usr/local/cargo/bin:/usr/bin:/bin","RUSTUP_HOME=/usr/local/rustup","CARGO_HOME=/tmp/cargo-home","SOURCE_DATE_EPOCH=0"]'
ms() { echo $(( $(date +%s%N) / 1000000 )); }
call() { local m=$1 p=$2; shift 2; curl -sS --unix-socket $S -o $D/body -w '%{http_code}' -X "$m" "http://d$p" "$@"; }
rm -rf $D/depcrate && mkdir -p $D/depcrate/src $D/depcrate/.cargo
printf '[package]\nname = "dep"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\n' > $D/depcrate/Cargo.toml
printf '#[derive(serde::Serialize, serde::Deserialize)]\nstruct P { x: i32 }\nfn main() {\n    let s = serde_json::to_string(&P { x: 1 }).unwrap();\n    let p: P = serde_json::from_str(&s).unwrap();\n    println!("{}", p.x);\n}\n' > $D/depcrate/src/main.rs
# fetch step: network on, writes vendor/ and Cargo.lock (the ADR's fetch run)
a=$(ms)
docker run --rm --user $(id -u):$(id -g) -e HOME=/tmp -e CARGO_HOME=/tmp/ch -v $D/depcrate:/work -w /work rust:1.97-slim \
  sh -c 'cargo vendor --locked 2>/dev/null || cargo vendor' > $D/vendor.out 2> $D/vendor.err; echo "vendor exit=$? millis=$(( $(ms)-a ))"
cat $D/vendor.out > $D/depcrate/.cargo/config.toml
echo "vendored crates: $(ls $D/depcrate/vendor | tr '\n' ' ')"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -cf $D/dep.tar -C $D/depcrate .
echo "input tar bytes=$(stat -c %s $D/dep.tar) entries=$(tar -tf $D/dep.tar | wc -l)"
for cpus in 4 32; do
  HC="{\"ReadonlyRootfs\":true,\"NetworkMode\":\"none\",\"Mounts\":[{\"Type\":\"volume\",\"Target\":\"/work\"}],\"Tmpfs\":{\"/work/target\":\"rw,exec\",\"/tmp\":\"rw,exec\"},\"NanoCpus\":${cpus}000000000,\"Memory\":8589934592,\"MemorySwap\":8589934592,\"PidsLimit\":1024}"
  a=$(ms); call POST /containers/create -H 'Content-Type: application/json' -d "{\"Image\":\"$IMG\",\"WorkingDir\":\"/work\",\"Cmd\":[\"cargo\",\"clippy\",\"--offline\",\"--locked\",\"--\",\"-D\",\"warnings\"],\"Env\":$ENVJ,\"HostConfig\":$HC}" >/dev/null; id=$(jq -r .Id $D/body); b=$(ms)
  pc=$(call PUT "/containers/$id/archive?path=/work" -H 'Content-Type: application/x-tar' --data-binary @$D/dep.tar); c=$(ms)
  call POST /containers/$id/start >/dev/null; call POST /containers/$id/wait >/dev/null; ex=$(jq -c .StatusCode $D/body); d=$(ms)
  docker logs $id > $D/dep$cpus.log 2>&1; gc=$(call GET "/containers/$id/archive?path=/work"); cp $D/body $D/depout.tar; e=$(ms)
  call DELETE "/containers/$id?force=1&v=1" >/dev/null; f=$(ms)
  echo "cpus=$cpus exit=$ex put=$pc create=$((b-a)) put_millis=$((c-b)) start+wait=$((d-c)) logs+get=$((e-d)) remove=$((f-e)) total=$((f-a)) out_entries=$(tar -tf $D/depout.tar | wc -l) target_entries=$(tar -tf $D/depout.tar | grep -c '^work/target/.')"
  echo "  log: $(tail -2 $D/dep$cpus.log | tr '\n' '|')"
done
