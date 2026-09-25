//! PAX extended-header records: `"<len> key=value\n"`, where `len` is the
//! decimal byte length of the whole record, its own digits included.

use crate::Refusal;

/// The name of every PAX `x` header the encoder writes.
pub const HEADER_NAME: &[u8] = b"././@PaxHeader";

/// Append one record. The encoder calls this in key order.
pub fn write_record(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    let body = key.len() + value.len() + 3;
    let mut len = body + decimal_digits(body);
    if decimal_digits(len) != decimal_digits(body) {
        len = body + decimal_digits(len);
    }
    out.extend_from_slice(format!("{len} {key}=").as_bytes());
    out.extend_from_slice(value);
    out.push(b'\n');
}

fn decimal_digits(value: usize) -> usize {
    value.to_string().len()
}

/// The records the codec reads; every other key is dropped with the fields
/// a tree does not carry.
#[derive(Debug, Default)]
pub struct Records {
    pub path: Option<Vec<u8>>,
    pub linkpath: Option<Vec<u8>>,
    pub size: Option<u64>,
}

/// Parse the body of one `x` header. A later record overrides an earlier one
/// for the same key, and an empty value unsets it.
///
/// # Errors
///
/// [`Refusal::Sparse`] for any `GNU.sparse.*` key, and
/// [`Refusal::ExtendedMalformed`] for a record whose length, separator,
/// terminator, or `size` value is wrong.
pub fn parse(mut data: &[u8]) -> Result<Records, Refusal> {
    let mut records = Records::default();
    while !data.is_empty() {
        let space = data.iter().position(|&byte| byte == b' ').ok_or(Refusal::ExtendedMalformed)?;
        let len = decimal(&data[..space])
            .and_then(|len| usize::try_from(len).ok())
            .filter(|&len| len > space + 2 && len <= data.len())
            .ok_or(Refusal::ExtendedMalformed)?;
        let (record, rest) = data.split_at(len);
        data = rest;

        let body = record[space + 1..].strip_suffix(b"\n").ok_or(Refusal::ExtendedMalformed)?;
        let equals = body.iter().position(|&byte| byte == b'=').ok_or(Refusal::ExtendedMalformed)?;
        let (key, value) = (&body[..equals], &body[equals + 1..]);
        if key.starts_with(b"GNU.sparse.") {
            return Err(Refusal::Sparse);
        }
        let value = (!value.is_empty()).then_some(value);
        match key {
            b"path" => records.path = value.map(<[u8]>::to_vec),
            b"linkpath" => records.linkpath = value.map(<[u8]>::to_vec),
            b"size" => {
                records.size = value.map(|value| decimal(value).ok_or(Refusal::ExtendedMalformed)).transpose()?;
            }
            _ => {}
        }
    }
    Ok(records)
}

fn decimal(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |value, &digit| {
        digit.is_ascii_digit().then_some(())?;
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}
