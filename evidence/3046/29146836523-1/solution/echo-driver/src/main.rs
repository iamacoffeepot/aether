//! Raw framed TCP client that drives the echo component.
//!
//! For each message it writes a length-prefix frame ([4-byte LE len][body])
//! but *splits the frame across multiple TCP writes* to prove the server-side
//! reassembly. It then reads the echoed bytes back and reassembles them with
//! `aether_codec::frame::pop_frame` (the reassembler), confirming every message
//! round-trips intact.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use aether_codec::frame::pop_frame;

fn frame_bytes(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn main() {
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:7777".to_string());
    let messages: Vec<&[u8]> = vec![
        b"hello",
        b"world",
        b"the quick brown fox jumps over the lazy dog",
        b"",
        b"\x00\x01\x02\x03 binary-ish payload \xff\xfe",
    ];

    let mut stream = TcpStream::connect(&addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // Write every frame, splitting each frame across several TCP writes so the
    // 4-byte prefix and the body arrive in separate chunks.
    for msg in &messages {
        let framed = frame_bytes(msg);
        // Split points: after 2 prefix bytes, after the full prefix, then mid-body.
        let mut cuts = vec![2usize, 4];
        if framed.len() > 5 {
            cuts.push(4 + (framed.len() - 4) / 2);
        }
        let mut prev = 0;
        for &cut in cuts.iter().chain(std::iter::once(&framed.len())) {
            let cut = cut.min(framed.len());
            if cut > prev {
                stream.write_all(&framed[prev..cut]).expect("write");
                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(15));
                prev = cut;
            }
        }
    }

    // Read echoed bytes and reassemble frames with pop_frame.
    let mut buf: Vec<u8> = Vec::new();
    let mut got: Vec<Vec<u8>> = Vec::new();
    let mut chunk = [0u8; 256];
    while got.len() < messages.len() {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                while let Some(body) = pop_frame(&mut buf).expect("pop_frame") {
                    got.push(body);
                }
            }
            Err(e) => {
                eprintln!("read error after {} frames: {e}", got.len());
                break;
            }
        }
    }

    let mut ok = true;
    for (i, msg) in messages.iter().enumerate() {
        match got.get(i) {
            Some(body) if body.as_slice() == *msg => {
                println!("frame {i}: OK ({} bytes) round-tripped", msg.len());
            }
            Some(body) => {
                ok = false;
                println!("frame {i}: MISMATCH sent={:?} got={:?}", msg, body);
            }
            None => {
                ok = false;
                println!("frame {i}: MISSING (no echo received)");
            }
        }
    }

    if ok && got.len() == messages.len() {
        println!("ALL {} FRAMES ROUND-TRIPPED INTACT", messages.len());
    } else {
        println!("FAILED: {}/{} frames matched", got.iter().zip(&messages).filter(|(g, m)| g.as_slice() == **m).count(), messages.len());
        std::process::exit(1);
    }
}
