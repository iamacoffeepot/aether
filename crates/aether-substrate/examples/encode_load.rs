// One-shot helper for ADR-0010 smoke testing. Emits a postcard-encoded
// `LoadComponentPayload` ready to drop into the MCP `send_mail` tool's
// `payload_bytes` field, and decodes a `LoadResultPayload` when run
// with the `--decode <hex>` arg.
//
// Not a load-bearing utility — the control-plane kinds are `Opaque`
// today because ADR-0007's descriptor model doesn't cover structured
// variable-length payloads. This example fills that gap by hand.

use aether_hub_protocol::{KindDescriptor, KindEncoding};
use aether_substrate::{LoadComponentPayload, LoadResultPayload};

const WAT: &str = r#"
    (module
        (memory (export "memory") 1)
        (func (export "receive") (param i32 i32 i32) (result i32)
            i32.const 0))
"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, hex] = args.as_slice()
        && flag == "--decode"
    {
        let bytes = hex_decode(hex);
        let r: LoadResultPayload = postcard::from_bytes(&bytes).expect("decode load_result");
        println!("{r:#?}");
        return;
    }

    let wasm = wat::parse_str(WAT).expect("compile WAT");
    let payload = LoadComponentPayload {
        wasm,
        kinds: vec![KindDescriptor {
            name: "smoke.ping".into(),
            encoding: KindEncoding::Signal,
        }],
        name: Some("smoke".into()),
    };
    let bytes = postcard::to_allocvec(&payload).expect("encode");
    let json: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    println!("[{}]", json.join(","));
}

fn hex_decode(s: &str) -> Vec<u8> {
    // Accepts either "0a,1b,..." (decimal comma-separated) or plain hex.
    if s.contains(',') {
        s.split(',')
            .map(|n| n.trim().parse::<u8>().expect("decimal byte"))
            .collect()
    } else {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex byte"))
            .collect()
    }
}
