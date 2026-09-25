#!/usr/bin/env bash
# Q2 archive endpoints + Q4 timings, against the daemon socket directly.
set -uo pipefail
S=/var/run/docker.sock; D=${SPIKE_SCRATCH:-${TMPDIR:-/tmp}/spike-ws0}; mkdir -p $D; cd $D
IMG=spike-ws0-env:v1
ENVJ='["PATH=/usr/local/cargo/bin:/usr/bin:/bin","RUSTUP_HOME=/usr/local/rustup","CARGO_HOME=/tmp/cargo-home","CARGO_NET_OFFLINE=true","SOURCE_DATE_EPOCH=0"]'
ms() { echo $(( $(date +%s%N) / 1000000 )); }
call() { local m=$1 p=$2; shift 2; curl -sS --unix-socket $S -o $D/body -w '%{http_code}' -X "$m" "http://d$p" "$@"; }
create() { local c; c=$(call POST /containers/create -H 'Content-Type: application/json' -d "$1"); [ "$c" = 201 ] || { echo "CREATE $c $(cat $D/body)" >&2; echo none; return; }; jq -r .Id $D/body; }
put() { call PUT "/containers/$1/archive?path=$2" -H 'Content-Type: application/x-tar' --data-binary @$3; }
get() { call GET "/containers/$1/archive?path=$2" > $D/getcode; cp $D/body $D/get.tar; echo "$(cat $D/getcode)"; }
start() { call POST /containers/$1/start; }
wait_() { call POST /containers/$1/wait >/dev/null; jq -c .StatusCode $D/body; }
rm_() { call DELETE "/containers/$1?force=1&v=1" >/dev/null; }
listtar() { tar -tvf $D/get.tar 2>&1 | awk '{print $1, $2, $6}' | tr '\n' ';'; }

docker info --format 'daemon: driver={{.Driver}} store={{.DriverStatus}}' 2>/dev/null | head -c 300; echo

# the input tree: a small crate with one clippy finding, as a canonical tar
rm -rf $D/crate && mkdir -p $D/crate/src
printf '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n' > $D/crate/Cargo.toml
printf 'fn main() {\n    let v: Vec<i32> = Vec::new();\n    if v.len() == 0 {\n        println!("empty");\n    }\n}\n' > $D/crate/src/main.rs
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 -cf $D/in.tar -C $D/crate .
echo "input tar: $(tar -tf $D/in.tar | tr '\n' ' ')"

echo "== Q2a: PUT /work before start =="
for variant in rw ro ro+volume ro+tmpfs-work; do
  case $variant in
    rw) HC='{"ReadonlyRootfs":false}';;
    ro) HC='{"ReadonlyRootfs":true}';;
    ro+volume) HC='{"ReadonlyRootfs":true,"Mounts":[{"Type":"volume","Target":"/work"}]}';;
    ro+tmpfs-work) HC='{"ReadonlyRootfs":true,"Tmpfs":{"/work":"rw,exec"}}';;
  esac
  id=$(create "{\"Image\":\"$IMG\",\"WorkingDir\":\"/work\",\"Cmd\":[\"sh\",\"-c\",\"ls /work | tr \\\"\\\\n\\\" \\\" \\\"; echo made > /work/made.txt\"],\"Env\":$ENVJ,\"HostConfig\":$HC}")
  [ "$id" = none ] && continue
  pc=$(put $id /work $D/in.tar); pbody=$(head -c 160 $D/body)
  sc=$(start $id); ex=$(wait_ $id); out=$(docker logs $id 2>&1 | head -c 200 | tr '\n' ' ')
  gc=$(get $id /work)
  echo "[$variant] put=$pc ${pbody:+body=$pbody} start=$sc exit=$ex out='$out' get_after_exit=$gc tar=$(listtar)"
  rm_ $id
done

echo "== Q2c/d: tmpfs at /work/target, GET while running and after exit =="
HC='{"ReadonlyRootfs":true,"Mounts":[{"Type":"volume","Target":"/work"}],"Tmpfs":{"/work/target":"rw,exec","/tmp":"rw,exec"}}'
id=$(create "{\"Image\":\"$IMG\",\"WorkingDir\":\"/work\",\"Cmd\":[\"sh\",\"-c\",\"echo t > /work/target/t.txt; echo w > /work/w.txt; sleep 4\"],\"Env\":$ENVJ,\"HostConfig\":$HC}")
echo "put=$(put $id /work $D/in.tar) start=$(start $id)"; sleep 2
echo "running: get=$(get $id /work) tar=$(listtar)"
echo "running: get target=$(get $id /work/target) tar=$(listtar)"
echo "exit=$(wait_ $id)"
echo "exited: get=$(get $id /work) tar=$(listtar)"
echo "exited: get target=$(get $id /work/target) tar=$(listtar)"
rm_ $id
echo "-- same with writable rootfs (no volume) --"
HC='{"ReadonlyRootfs":false,"Tmpfs":{"/work/target":"rw,exec"}}'
id=$(create "{\"Image\":\"$IMG\",\"WorkingDir\":\"/work\",\"Cmd\":[\"sh\",\"-c\",\"echo t > /work/target/t.txt; echo w > /work/w.txt; sleep 4\"],\"Env\":$ENVJ,\"HostConfig\":$HC}")
echo "put=$(put $id /work $D/in.tar) start=$(start $id)"; sleep 2
echo "running: get=$(get $id /work) tar=$(listtar)"
echo "exit=$(wait_ $id)"
echo "exited: get=$(get $id /work) tar=$(listtar)"
rm_ $id

echo "== Q4a: create+start+wait+remove overhead, Cmd true, 10 runs (API) =="
tc=0; ts=0; tw=0; tr=0
for i in $(seq 10); do
  a=$(ms); id=$(create "{\"Image\":\"$IMG\",\"Cmd\":[\"true\"],\"HostConfig\":{\"ReadonlyRootfs\":true,\"NetworkMode\":\"none\"}}"); b=$(ms)
  start $id >/dev/null; c=$(ms); wait_ $id >/dev/null; d=$(ms); rm_ $id; e=$(ms)
  tc=$((tc+b-a)); ts=$((ts+c-b)); tw=$((tw+d-c)); tr=$((tr+e-d))
done
echo "avg millis: create=$((tc/10)) start=$((ts/10)) wait=$((tw/10)) remove=$((tr/10)) total=$(((tc+ts+tw+tr)/10))"
a=$(ms); for i in $(seq 5); do docker run --rm --network none $IMG true; done; echo "docker run --rm true avg millis=$(( ($(ms)-a)/5 ))"

echo "== Q4b: cold clippy in container through the archive loop (ro root, volume /work, tmpfs target+tmp, net none) =="
for n in 1 2 3; do
  HC='{"ReadonlyRootfs":true,"NetworkMode":"none","Mounts":[{"Type":"volume","Target":"/work"}],"Tmpfs":{"/work/target":"rw,exec","/tmp":"rw,exec"},"NanoCpus":4000000000,"Memory":4294967296,"MemorySwap":4294967296,"PidsLimit":512}'
  a=$(ms); id=$(create "{\"Image\":\"$IMG\",\"WorkingDir\":\"/work\",\"Cmd\":[\"cargo\",\"clippy\",\"--offline\",\"--\",\"-D\",\"warnings\"],\"Env\":$ENVJ,\"HostConfig\":$HC}"); b=$(ms)
  put $id /work $D/in.tar >/dev/null; c=$(ms); start $id >/dev/null; ex=$(wait_ $id); d=$(ms)
  docker logs $id > $D/clippy$n.log 2>&1; gc=$(get $id /work); e=$(ms); rm_ $id; f=$(ms)
  echo "run$n exit=$ex create=$((b-a)) put=$((c-b)) start+wait=$((d-c)) logs+get=$((e-d)) remove=$((f-e)) total=$((f-a)) get=$gc out_tar=$(listtar)"
done
echo "clippy log (run1): $(grep -E 'error|warning|Finished|Checking' $D/clippy1.log | head -5 | tr '\n' '|')"
