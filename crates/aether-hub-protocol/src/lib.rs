// aether-hub-protocol: engine ↔ hub wire types and framing per ADR-0006.
//
// Uni-directional mail flow for V0: frames go Claude → hub → engine.
// Engines send only lifecycle frames (Hello, Heartbeat, Goodbye) —
// engine-originated mail and replies are parked.
//
// Framing: each frame on the TCP stream is a 4-byte little-endian
// length prefix followed by the postcard-encoded message. Two enum
// types (`EngineToHub`, `HubToEngine`) enforce direction at the type
// level.

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Serialize, de::DeserializeOwned};

pub use uuid::Uuid;

mod types;
pub use types::*;

/// Maximum accepted frame body size. Bounded so a malformed length
/// prefix cannot drive a reader into an OOM. 16 MiB is comfortably
/// larger than any expected mail payload on the hub wire (vertex
/// streams travel through the render sink, not the hub).
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Errors from the framing helpers. Wraps I/O and postcard decode
/// errors; adds its own variant for an oversize length prefix.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Postcard(postcard::Error),
    FrameTooLarge { size: usize, max: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "hub protocol io: {e}"),
            FrameError::Postcard(e) => write!(f, "hub protocol decode: {e}"),
            FrameError::FrameTooLarge { size, max } => {
                write!(f, "hub protocol frame too large: {size} > {max}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

impl From<postcard::Error> for FrameError {
    fn from(e: postcard::Error) -> Self {
        FrameError::Postcard(e)
    }
}

/// Encode a message into its framed wire representation (4-byte LE
/// length prefix + postcard body). Infallible — postcard encoding of
/// `alloc::Vec` is infallible for the types in this crate.
pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_allocvec(msg).expect("postcard encode to Vec is infallible");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Synchronous read of one framed message. Blocks until a complete
/// frame is consumed from `r`. Async callers should inline the
/// length+body reads on their own async stream rather than calling
/// this on a blocking wrapper.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(postcard::from_bytes(&buf)?)
}

/// Synchronous write of one framed message.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), FrameError> {
    let bytes = encode_frame(msg);
    w.write_all(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn hello_roundtrip() {
        let hello = EngineToHub::Hello(Hello {
            name: "hello-triangle".into(),
            pid: 8910,
            started_unix: 1_712_345_678,
            version: "0.1.0".into(),
            kinds: vec![],
        });

        let mut buf = Vec::new();
        write_frame(&mut buf, &hello).unwrap();

        let mut r = Cursor::new(buf);
        let back: EngineToHub = read_frame(&mut r).unwrap();
        match back {
            EngineToHub::Hello(h) => {
                assert_eq!(h.name, "hello-triangle");
                assert_eq!(h.pid, 8910);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hello_with_kinds_roundtrip() {
        let hello = EngineToHub::Hello(Hello {
            name: "hello-triangle".into(),
            pid: 1,
            started_unix: 0,
            version: "0".into(),
            kinds: vec![
                KindDescriptor {
                    name: "aether.tick".into(),
                    encoding: KindEncoding::Signal,
                },
                KindDescriptor {
                    name: "aether.key".into(),
                    encoding: KindEncoding::Pod {
                        fields: vec![PodField {
                            name: "code".into(),
                            ty: PodFieldType::Scalar(PodPrimitive::U32),
                        }],
                    },
                },
                KindDescriptor {
                    name: "aether.draw_triangle".into(),
                    encoding: KindEncoding::Opaque,
                },
            ],
        });

        let mut buf = Vec::new();
        write_frame(&mut buf, &hello).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        let EngineToHub::Hello(h) = back else {
            panic!("wrong variant")
        };
        assert_eq!(h.kinds.len(), 3);
        assert_eq!(h.kinds[0].encoding, KindEncoding::Signal);
        let KindEncoding::Pod { fields } = &h.kinds[1].encoding else {
            panic!("expected Pod")
        };
        assert_eq!(fields[0].name, "code");
        assert_eq!(fields[0].ty, PodFieldType::Scalar(PodPrimitive::U32));
        assert_eq!(h.kinds[2].encoding, KindEncoding::Opaque);
    }

    #[test]
    fn pod_field_array_roundtrip() {
        let desc = KindDescriptor {
            name: "aether.mouse_move".into(),
            encoding: KindEncoding::Pod {
                fields: vec![
                    PodField {
                        name: "x".into(),
                        ty: PodFieldType::Scalar(PodPrimitive::F32),
                    },
                    PodField {
                        name: "y".into(),
                        ty: PodFieldType::Scalar(PodPrimitive::F32),
                    },
                    PodField {
                        name: "pad".into(),
                        ty: PodFieldType::Array {
                            element: PodPrimitive::U8,
                            len: 4,
                        },
                    },
                ],
            },
        };
        let bytes = postcard::to_allocvec(&desc).unwrap();
        let back: KindDescriptor = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, desc);
    }

    #[test]
    fn welcome_roundtrip() {
        let id = EngineId(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0));
        let msg = HubToEngine::Welcome(Welcome { engine_id: id });

        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: HubToEngine = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            HubToEngine::Welcome(w) => assert_eq!(w.engine_id, id),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mail_frame_roundtrip() {
        let sender = SessionToken(Uuid::from_u128(0xa_b_c_d));
        let msg = HubToEngine::Mail(MailFrame {
            recipient_name: "hello".into(),
            kind_name: "aether.tick".into(),
            payload: vec![],
            count: 1,
            sender,
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: HubToEngine = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            HubToEngine::Mail(m) => {
                assert_eq!(m.recipient_name, "hello");
                assert_eq!(m.kind_name, "aether.tick");
                assert_eq!(m.count, 1);
                assert_eq!(m.sender, sender);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn engine_mail_frame_session_roundtrip() {
        let token = SessionToken(Uuid::from_u128(0x1));
        let msg = EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(token),
            kind_name: "aether.observation.ping".into(),
            payload: vec![1, 2, 3],
            origin: Some("physics".into()),
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::Mail(m) => {
                assert_eq!(m.address, ClaudeAddress::Session(token));
                assert_eq!(m.kind_name, "aether.observation.ping");
                assert_eq!(m.payload, vec![1, 2, 3]);
                assert_eq!(m.origin.as_deref(), Some("physics"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn engine_mail_frame_broadcast_roundtrip() {
        let msg = EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Broadcast,
            kind_name: "aether.observation.world".into(),
            payload: vec![],
            origin: None,
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::Mail(m) => {
                assert_eq!(m.address, ClaudeAddress::Broadcast);
                assert!(m.origin.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kinds_changed_roundtrip() {
        let msg = EngineToHub::KindsChanged(vec![
            KindDescriptor {
                name: "aether.tick".into(),
                encoding: KindEncoding::Signal,
            },
            KindDescriptor {
                name: "physics.contact".into(),
                encoding: KindEncoding::Opaque,
            },
        ]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: EngineToHub = read_frame(&mut Cursor::new(buf)).unwrap();
        match back {
            EngineToHub::KindsChanged(k) => {
                assert_eq!(k.len(), 2);
                assert_eq!(k[0].name, "aether.tick");
                assert_eq!(k[1].name, "physics.contact");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn heartbeat_both_directions() {
        for buf in [
            encode_frame(&EngineToHub::Heartbeat),
            encode_frame(&HubToEngine::Heartbeat),
        ] {
            // Smallest possible frame: 4 byte prefix + 1 byte postcard tag.
            assert_eq!(buf.len(), 5);
        }
    }

    #[test]
    fn multiple_frames_back_to_back() {
        let a = EngineToHub::Hello(Hello {
            name: "a".into(),
            pid: 1,
            started_unix: 0,
            version: "0".into(),
            kinds: vec![],
        });
        let b = EngineToHub::Heartbeat;
        let c = EngineToHub::Goodbye(Goodbye {
            reason: "done".into(),
        });

        let mut buf = Vec::new();
        write_frame(&mut buf, &a).unwrap();
        write_frame(&mut buf, &b).unwrap();
        write_frame(&mut buf, &c).unwrap();

        let mut r = Cursor::new(buf);
        let _: EngineToHub = read_frame(&mut r).unwrap();
        let _: EngineToHub = read_frame(&mut r).unwrap();
        let _: EngineToHub = read_frame(&mut r).unwrap();
    }

    #[test]
    fn frame_too_large_rejected() {
        // Hand-craft a length prefix claiming 100 MiB.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(100 * 1024 * 1024u32).to_le_bytes());
        let err = read_frame::<_, EngineToHub>(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, FrameError::FrameTooLarge { .. }));
    }

    #[test]
    fn truncated_body_returns_io_error() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes()); // claim 100 bytes
        buf.extend_from_slice(&[0u8; 10]); // only 10 bytes
        let err = read_frame::<_, EngineToHub>(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, FrameError::Io(_)));
    }

    #[test]
    fn malformed_body_returns_postcard_error() {
        // Length=1, body byte that doesn't match any EngineToHub variant tag.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(0xff);
        let err = read_frame::<_, EngineToHub>(&mut Cursor::new(buf)).unwrap_err();
        assert!(matches!(err, FrameError::Postcard(_)));
    }
}
