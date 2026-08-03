//! A deliberately small OBJ reader.
//!
//! The subject is a 75 MB generated reconstruction, so this parses bytes
//! directly rather than splitting strings, and it keeps only what the
//! renderer needs: positions and triangles. Bytes rather than a path
//! because a wasm guest has no filesystem — they arrive by mail, from
//! `aether.fs`. Normals in the file are
//! per-face and get recomputed anyway; texture coordinates are unused.

use core::str;

use aether_math::Vec3;

pub struct Raw {
    pub positions: Vec<Vec3>,
    pub faces: Vec<[u32; 3]>,
}

/// Reads the leading `f32` from `bytes`, returning it and the rest.
fn take_f32(bytes: &[u8]) -> Option<(f32, &[u8])> {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace())?;
    let rest = &bytes[start..];
    let end = rest.iter().position(u8::is_ascii_whitespace).unwrap_or(rest.len());

    let value = str::from_utf8(&rest[..end]).ok()?.parse().ok()?;
    Some((value, &rest[end..]))
}

/// Reads one face vertex — `12`, `12/3`, `12//7`, `12/3/7` — keeping only
/// the position index and normalising OBJ's 1-based indexing.
fn take_index(bytes: &[u8], vertex_count: usize) -> Option<(u32, &[u8])> {
    let start = bytes.iter().position(|b| !b.is_ascii_whitespace())?;
    let rest = &bytes[start..];
    let end = rest.iter().position(u8::is_ascii_whitespace).unwrap_or(rest.len());

    let token = &rest[..end];
    let position_end = token.iter().position(|&b| b == b'/').unwrap_or(token.len());
    let raw: i64 = str::from_utf8(&token[..position_end]).ok()?.parse().ok()?;

    // Negative indices count back from the end of the vertex list so far.
    let index = if raw < 0 {
        vertex_count as i64 + raw
    } else {
        raw - 1
    };
    (index >= 0).then(|| (index as u32, &rest[end..]))
}

pub fn parse(bytes: &[u8]) -> Raw {
    let mut positions = Vec::new();
    let mut faces = Vec::new();
    let mut corners: Vec<u32> = Vec::with_capacity(4);

    for line in bytes.split(|&b| b == b'\n') {
        match line {
            [b'v', rest @ ..] if rest.first().is_some_and(u8::is_ascii_whitespace) => {
                let Some((x, rest)) = take_f32(rest) else {
                    continue;
                };
                let Some((y, rest)) = take_f32(rest) else {
                    continue;
                };
                let Some((z, _)) = take_f32(rest) else {
                    continue;
                };
                positions.push(Vec3::new(x, y, z));
            }
            [b'f', tail @ ..] if tail.first().is_some_and(u8::is_ascii_whitespace) => {
                corners.clear();
                let mut rest: &[u8] = tail;
                while let Some((index, remainder)) = take_index(rest, positions.len()) {
                    corners.push(index);
                    rest = remainder;
                }
                // Fan-triangulate; the subject is already triangles, but a
                // quad in some other export should not silently vanish.
                for i in 1..corners.len().saturating_sub(1) {
                    faces.push([corners[0], corners[i], corners[i + 1]]);
                }
            }
            _ => {}
        }
    }

    Raw { positions, faces }
}
