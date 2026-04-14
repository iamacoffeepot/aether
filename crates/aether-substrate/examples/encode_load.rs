// One-shot helper for ADR-0010 smoke testing. Emits postcard-encoded
// control-plane payloads ready to drop into the MCP `send_mail` tool's
// `payload_bytes` field, and decodes a matching result when run with
// `--decode <kind> <bytes>`.
//
// Usage:
//   encode_load                         # encode LoadComponentPayload
//   encode_load drop <id>               # encode DropComponentPayload
//   encode_load replace <id> [name]     # encode ReplaceComponentPayload
//   encode_load --decode load  <bytes>  # decode LoadResultPayload
//   encode_load --decode drop  <bytes>  # decode DropResultPayload
//   encode_load --decode repl  <bytes>  # decode ReplaceResultPayload
//
// `<bytes>` is a comma-separated decimal byte list (what MCP
// `receive_mail` emits).
//
// Not a load-bearing utility — the control-plane kinds are `Opaque`
// today because ADR-0007's descriptor model doesn't cover structured
// variable-length payloads.

use aether_hub_protocol::{KindDescriptor, KindEncoding};
use aether_substrate::{
    DropComponentPayload, DropResultPayload, LoadComponentPayload, LoadResultPayload,
    ReplaceComponentPayload, ReplaceResultPayload,
};

const WAT: &str = r#"
    (module
        (memory (export "memory") 1)
        (func (export "receive") (param i32 i32 i32) (result i32)
            i32.const 0))
"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if let [_, flag, tag, hex] = args.as_slice()
        && flag == "--decode"
    {
        let bytes = parse_bytes(hex);
        match tag.as_str() {
            "load" => {
                let r: LoadResultPayload = postcard::from_bytes(&bytes).expect("decode");
                println!("{r:#?}");
            }
            "drop" => {
                let r: DropResultPayload = postcard::from_bytes(&bytes).expect("decode");
                println!("{r:#?}");
            }
            "repl" | "replace" => {
                let r: ReplaceResultPayload = postcard::from_bytes(&bytes).expect("decode");
                println!("{r:#?}");
            }
            other => panic!("unknown decode tag {other:?}"),
        }
        return;
    }

    if let [_, cmd, rest @ ..] = args.as_slice() {
        match cmd.as_str() {
            "drop" => {
                let id: u32 = rest[0].parse().expect("mailbox_id u32");
                let bytes =
                    postcard::to_allocvec(&DropComponentPayload { mailbox_id: id }).unwrap();
                print_bytes(&bytes);
                return;
            }
            "replace" => {
                let id: u32 = rest[0].parse().expect("mailbox_id u32");
                let wasm = wat::parse_str(WAT).unwrap();
                let bytes = postcard::to_allocvec(&ReplaceComponentPayload {
                    mailbox_id: id,
                    wasm,
                    kinds: vec![],
                })
                .unwrap();
                print_bytes(&bytes);
                return;
            }
            _ => {}
        }
    }

    // Default: encode LoadComponentPayload.
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
    print_bytes(&bytes);
}

fn print_bytes(bytes: &[u8]) {
    let json: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    println!("[{}]", json.join(","));
}

fn parse_bytes(s: &str) -> Vec<u8> {
    // Comma-separated decimal is the canonical form — `mcp__aether-hub__receive_mail`
    // emits a JSON array of u8 which looks like `0,1,2,...` once the
    // brackets are stripped. Single-byte payloads still come through
    // as a bare `"0"` with no delimiter; treat anything without a hex
    // character as decimal so `["0"]` decodes correctly.
    let hexish = s.chars().any(|c| matches!(c, 'a'..='f' | 'A'..='F'));
    if !hexish {
        s.split(',')
            .filter(|p| !p.is_empty())
            .map(|n| n.trim().parse::<u8>().expect("decimal byte"))
            .collect()
    } else {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex byte"))
            .collect()
    }
}
