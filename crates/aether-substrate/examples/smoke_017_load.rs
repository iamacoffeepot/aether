// One-shot helper for the ADR-0017 component-to-component smoke
// test. Builds a LoadComponentPayload for the echoer or caller
// example shipped with `aether-hello-component`, declaring the
// three demo kinds (Pod { seq: u32 }) inline so the substrate
// registers them at load time.
//
// Usage:
//   cargo build -p aether-hello-component --target wasm32-unknown-unknown \
//       --release --examples
//   smoke_017_load echoer target/wasm32-unknown-unknown/release/examples/echoer.wasm echoer
//   smoke_017_load caller target/wasm32-unknown-unknown/release/examples/caller.wasm caller

use aether_hub_protocol::{KindDescriptor, KindEncoding, PodField, PodFieldType, PodPrimitive};
use aether_substrate::LoadComponentPayload;

fn seq_pod(name: &str) -> KindDescriptor {
    KindDescriptor {
        name: name.to_string(),
        encoding: KindEncoding::Pod {
            fields: vec![PodField {
                name: "seq".to_string(),
                ty: PodFieldType::Scalar(PodPrimitive::U32),
            }],
        },
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, role, wasm_path, name] = args.as_slice() else {
        eprintln!("usage: smoke_017_load <echoer|caller> <wasm-path> <name>");
        std::process::exit(1);
    };

    let wasm = std::fs::read(wasm_path).expect("read wasm");
    let kinds = match role.as_str() {
        "echoer" => vec![seq_pod("demo.request"), seq_pod("demo.response")],
        "caller" => vec![
            seq_pod("demo.request"),
            seq_pod("demo.response"),
            seq_pod("demo.observation"),
        ],
        other => panic!("unknown role {other:?} (expected echoer or caller)"),
    };

    let payload = LoadComponentPayload {
        wasm,
        kinds,
        name: Some(name.clone()),
    };
    let bytes = postcard::to_allocvec(&payload).expect("encode");
    let json: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    println!("[{}]", json.join(","));
}
