//! Minimal `.npy` v1/v2 reader for the one shape this spike cares about:
//! a C-order `uint8` cubic label grid.

use std::path::Path;

pub struct Grid {
    pub cells: Vec<u8>,
    pub n: usize,
}

impl Grid {
    /// Read a cubic C-order `|u1` array, parsing the header rather than
    /// assuming its layout — a Fortran-order or wider dtype file is an
    /// error here, not a silently transposed volume.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if bytes.len() < 12 || &bytes[0..6] != b"\x93NUMPY" {
            return Err("not a .npy file (bad magic)".into());
        }

        let (major, minor) = (bytes[6], bytes[7]);
        let (header_len, data_start) = match major {
            1 => (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10),
            2 => (u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize, 12),
            _ => return Err(format!("unsupported .npy version {major}.{minor}")),
        };

        let header = std::str::from_utf8(&bytes[data_start..data_start + header_len])
            .map_err(|e| format!("header is not utf-8: {e}"))?
            .to_string();

        let descr = dict_value(&header, "descr").ok_or("header has no 'descr'")?;
        if !matches!(descr.trim_matches('\''), "|u1" | "<u1" | "u1" | "B") {
            return Err(format!("expected a uint8 array, header says descr={descr}"));
        }
        let fortran = dict_value(&header, "fortran_order").ok_or("header has no 'fortran_order'")?;
        if fortran.trim() != "False" {
            return Err("expected C order, header says fortran_order=True".into());
        }

        let shape: Vec<usize> = dict_value(&header, "shape")
            .ok_or("header has no 'shape'")?
            .trim_matches(|c| c == '(' || c == ')')
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<usize>().map_err(|e| format!("bad shape entry {s}: {e}")))
            .collect::<Result<_, _>>()?;

        if shape.len() != 3 || shape[0] != shape[1] || shape[1] != shape[2] {
            return Err(format!("expected a cubic 3-d array, got shape {shape:?}"));
        }

        let n = shape[0];
        let cells = bytes[data_start + header_len..].to_vec();
        if cells.len() != n * n * n {
            return Err(format!("payload is {} bytes, shape {shape:?} wants {}", cells.len(), n * n * n));
        }

        println!("npy: version {major}.{minor}, descr={descr}, C order, shape {shape:?}");
        Ok(Self { cells, n })
    }

    #[inline]
    pub fn at(&self, i: usize, j: usize, k: usize) -> u8 {
        self.cells[(i * self.n + j) * self.n + k]
    }

    #[inline]
    pub fn index(&self, i: usize, j: usize, k: usize) -> usize {
        (i * self.n + j) * self.n + k
    }

    #[inline]
    pub fn coords(&self, index: usize) -> [usize; 3] {
        [index / (self.n * self.n), (index / self.n) % self.n, index % self.n]
    }
}

/// Pull one value out of the python-literal header dict. The values here are
/// flat scalars or a parenthesised tuple, so tracking paren depth is enough.
fn dict_value(header: &str, key: &str) -> Option<String> {
    let needle = format!("'{key}':");
    let rest = &header[header.find(&needle)? + needle.len()..];

    let mut depth = 0usize;
    let mut end = rest.len();
    for (offset, c) in rest.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                end = offset;
                break;
            }
            '}' if depth == 0 => {
                end = offset;
                break;
            }
            _ => {}
        }
    }

    Some(rest[..end].trim().to_string())
}
