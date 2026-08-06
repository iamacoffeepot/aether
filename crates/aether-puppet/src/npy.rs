//! The bounded `NumPy` v1.0 framing shared by the puppet's asset readers.

const MAGIC: &[u8; 6] = b"\x93NUMPY";
const PREAMBLE_BYTES: usize = 10;

pub struct Array<'a> {
    pub descr: &'a str,
    pub fortran_order: bool,
    pub shape: Vec<usize>,
    pub payload: &'a [u8],
}

/// Read the scalar-array subset of `NumPy` 1.0 that the puppet's authored
/// assets use.
///
/// The v1 header length is a `u16`, which bounds both the cursor's work and
/// the only allocation (`shape`). The returned payload and descriptor stay
/// borrowed from the mail bytes.
pub fn parse(bytes: &[u8]) -> Result<Array<'_>, String> {
    let preamble = bytes.get(..PREAMBLE_BYTES).ok_or_else(|| "NumPy preamble is truncated".to_owned())?;
    if &preamble[..6] != MAGIC {
        return Err("NumPy magic is not \\x93NUMPY".to_owned());
    }
    if preamble[6..8] != [1, 0] {
        return Err(format!("NumPy version is {}.{}, expected 1.0", preamble[6], preamble[7]));
    }

    let header_len = usize::from(u16::from_le_bytes([preamble[8], preamble[9]]));
    let header_end = PREAMBLE_BYTES
        .checked_add(header_len)
        .ok_or_else(|| "NumPy header length overflows the input address space".to_owned())?;
    let header = bytes
        .get(PREAMBLE_BYTES..header_end)
        .ok_or_else(|| format!("NumPy header declares {header_len} bytes but is truncated"))?;
    if header.last() != Some(&b'\n') {
        return Err("NumPy header does not end with a newline".to_owned());
    }
    if header_end % 16 != 0 {
        return Err("NumPy 1.0 header is not padded to a 16-byte boundary".to_owned());
    }
    if !header.is_ascii() {
        return Err("NumPy header is not ASCII".to_owned());
    }

    let mut cursor = Cursor::new(&header[..header.len() - 1]);
    cursor.skip_space();
    cursor.expect(b'{', "header dictionary")?;

    let (mut descr, mut fortran_order, mut shape) = (None, None, None);
    loop {
        cursor.skip_space();
        if cursor.take(b'}') {
            break;
        }

        let key = cursor.string("header field name")?;
        cursor.skip_space();
        cursor.expect(b':', "after header field name")?;
        cursor.skip_space();

        match key {
            "descr" => {
                if descr.is_some() {
                    return Err("NumPy header repeats 'descr'".to_owned());
                }
                descr = Some(cursor.string("'descr' value")?);
            }
            "fortran_order" => {
                if fortran_order.is_some() {
                    return Err("NumPy header repeats 'fortran_order'".to_owned());
                }
                fortran_order = Some(cursor.boolean()?);
            }
            "shape" => {
                if shape.is_some() {
                    return Err("NumPy header repeats 'shape'".to_owned());
                }
                shape = Some(cursor.shape()?);
            }
            _ => return Err(format!("NumPy header has unsupported field '{key}'")),
        }

        cursor.skip_space();
        if cursor.take(b',') {
            continue;
        }
        cursor.expect(b'}', "after header field")?;
        break;
    }

    cursor.skip_space();
    if !cursor.done() {
        return Err("NumPy header has bytes after its dictionary".to_owned());
    }

    let descr = descr.ok_or_else(|| "NumPy header is missing 'descr'".to_owned())?;
    let fortran_order = fortran_order.ok_or_else(|| "NumPy header is missing 'fortran_order'".to_owned())?;
    let shape = shape.ok_or_else(|| "NumPy header is missing 'shape'".to_owned())?;
    let elements = shape.iter().try_fold(1usize, |product, &dimension| {
        product.checked_mul(dimension).ok_or_else(|| "NumPy shape product overflows usize".to_owned())
    })?;
    let expected =
        elements.checked_mul(element_bytes(descr)?).ok_or_else(|| "NumPy payload length overflows usize".to_owned())?;
    let payload = &bytes[header_end..];
    if payload.len() != expected {
        return Err(format!("NumPy payload is {} bytes, expected {expected} from shape and dtype", payload.len()));
    }

    Ok(Array { descr, fortran_order, shape, payload })
}

fn element_bytes(descr: &str) -> Result<usize, String> {
    let bytes = descr.as_bytes();
    if bytes.len() < 3 || !matches!(bytes[0], b'<' | b'>' | b'=' | b'|') || !bytes[1].is_ascii_alphabetic() {
        return Err(format!("NumPy dtype descriptor '{descr}' is not a scalar dtype"));
    }

    let size = bytes[2..].iter().try_fold(0usize, |size, byte| {
        if byte.is_ascii_digit() {
            size.checked_mul(10)
                .and_then(|size| size.checked_add(usize::from(*byte - b'0')))
                .ok_or_else(|| format!("NumPy dtype descriptor '{descr}' has an overflowing item size"))
        } else {
            Err(format!("NumPy dtype descriptor '{descr}' is not a scalar dtype"))
        }
    })?;
    if size == 0 {
        return Err(format!("NumPy dtype descriptor '{descr}' has a zero item size"));
    }

    Ok(size)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn skip_space(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn take(&mut self, wanted: u8) -> bool {
        if self.bytes.get(self.at) == Some(&wanted) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, wanted: u8, context: &str) -> Result<(), String> {
        if self.take(wanted) {
            Ok(())
        } else {
            Err(format!("NumPy header expected '{}' {context}", char::from(wanted)))
        }
    }

    fn string(&mut self, context: &str) -> Result<&'a str, String> {
        let quote = *self.bytes.get(self.at).ok_or_else(|| format!("NumPy header is truncated at {context}"))?;
        if !matches!(quote, b'\'' | b'"') {
            return Err(format!("NumPy header expected a quoted string for {context}"));
        }
        self.at += 1;
        let start = self.at;
        while let Some(&byte) = self.bytes.get(self.at) {
            if byte == quote {
                let value = str::from_utf8(&self.bytes[start..self.at]).expect("ASCII was checked above");
                self.at += 1;
                return Ok(value);
            }
            if byte == b'\\' {
                return Err(format!("NumPy header does not accept escapes in {context}"));
            }
            self.at += 1;
        }

        Err(format!("NumPy header has an unterminated string for {context}"))
    }

    fn boolean(&mut self) -> Result<bool, String> {
        if self.bytes.get(self.at..self.at + 4) == Some(b"True") {
            self.at += 4;
            Ok(true)
        } else if self.bytes.get(self.at..self.at + 5) == Some(b"False") {
            self.at += 5;
            Ok(false)
        } else {
            Err("NumPy header 'fortran_order' is not True or False".to_owned())
        }
    }

    fn shape(&mut self) -> Result<Vec<usize>, String> {
        self.expect(b'(', "at the start of 'shape'")?;
        self.skip_space();
        if self.take(b')') {
            return Ok(Vec::new());
        }

        let mut dimensions = Vec::new();
        loop {
            let start = self.at;
            let mut dimension = 0usize;
            while let Some(byte) = self.bytes.get(self.at).filter(|byte| byte.is_ascii_digit()) {
                dimension = dimension
                    .checked_mul(10)
                    .and_then(|dimension| dimension.checked_add(usize::from(*byte - b'0')))
                    .ok_or_else(|| "NumPy shape dimension overflows usize".to_owned())?;
                self.at += 1;
            }
            if self.at == start {
                return Err("NumPy header 'shape' contains a non-integer dimension".to_owned());
            }
            dimensions.push(dimension);
            self.skip_space();
            if self.take(b',') {
                self.skip_space();
                if self.take(b')') {
                    break;
                }
            } else if dimensions.len() > 1 && self.take(b')') {
                break;
            } else {
                return Err("NumPy header 'shape' is not a tuple".to_owned());
            }
        }

        Ok(dimensions)
    }
}

#[cfg(test)]
mod tests {
    use core::iter;

    use super::*;

    fn array(header: &str, payload: &[u8]) -> Vec<u8> {
        let padding = (16 - ((PREAMBLE_BYTES + header.len() + 1) % 16)) % 16;
        let mut header = header.to_owned();
        header.extend(iter::repeat_n(' ', padding));
        header.push('\n');

        let mut bytes = b"\x93NUMPY\x01\x00".to_vec();
        bytes.extend(u16::try_from(header.len()).expect("a short test header").to_le_bytes());
        bytes.extend(header.as_bytes());
        bytes.extend(payload);
        bytes
    }

    #[test]
    fn a_valid_v1_array_returns_borrowed_metadata_and_payload() {
        let bytes = array("{'descr': '<f4', 'fortran_order': False, 'shape': (2, 1), }", &[0; 8]);
        let parsed = parse(&bytes).expect("valid NumPy 1.0");

        assert_eq!(parsed.descr, "<f4");
        assert!(!parsed.fortran_order);
        assert_eq!(parsed.shape, [2, 1]);
        assert_eq!(parsed.payload, &[0; 8]);
    }

    #[test]
    fn framing_and_payload_mismatches_are_diagnostic() {
        let mut bad_magic = array("{'descr': '|u1', 'fortran_order': False, 'shape': (2,), }", &[0; 2]);
        bad_magic[0] = 0;
        assert_eq!(parse(&bad_magic).err().as_deref(), Some("NumPy magic is not \\x93NUMPY"));

        let mut bad_version = array("{'descr': '|u1', 'fortran_order': False, 'shape': (2,), }", &[0; 2]);
        bad_version[6] = 2;
        assert_eq!(parse(&bad_version).err().as_deref(), Some("NumPy version is 2.0, expected 1.0"));

        let truncated = &array("{'descr': '|u1', 'fortran_order': False, 'shape': (2,), }", &[0; 2])[..15];
        assert!(parse(truncated).err().is_some_and(|error| error.contains("header declares")));

        let short = array("{'descr': '|u1', 'fortran_order': False, 'shape': (3,), }", &[0; 2]);
        assert_eq!(parse(&short).err().as_deref(), Some("NumPy payload is 2 bytes, expected 3 from shape and dtype"),);
    }

    #[test]
    fn required_duplicate_and_malformed_fields_are_refused() {
        let missing = array("{'descr': '|u1', 'fortran_order': False, }", &[]);
        assert_eq!(parse(&missing).err().as_deref(), Some("NumPy header is missing 'shape'"));

        let duplicate = array("{'descr': '|u1', 'descr': '|u1', 'fortran_order': False, 'shape': (0,), }", &[]);
        assert_eq!(parse(&duplicate).err().as_deref(), Some("NumPy header repeats 'descr'"));

        let malformed = array("{'descr': '|u1', 'fortran_order': false, 'shape': (0,), }", &[]);
        assert_eq!(parse(&malformed).err().as_deref(), Some("NumPy header 'fortran_order' is not True or False"),);
    }

    #[test]
    fn shape_overflow_is_refused_before_payload_arithmetic() {
        let overflowing =
            array(&format!("{{'descr': '|u1', 'fortran_order': False, 'shape': ({}, 2), }}", usize::MAX), &[]);
        assert_eq!(parse(&overflowing).err().as_deref(), Some("NumPy shape product overflows usize"));
    }
}
