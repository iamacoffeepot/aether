//! A bounded, duplicate-rejecting JSON reader for the request edge.
//!
//! The envelope rules need three things a general JSON reader does not offer,
//! and each of them is a refusal the protocol must make rather than a
//! preference:
//!
//! 1. **A duplicate member name at any object depth is invalid.** Left to
//!    last-writer-wins, two consumers could disagree about a duplicated
//!    `method`, `name`, or argument field — one reading the first value and
//!    one the second. The envelope is ambiguous, so the request is refused.
//! 2. **Nesting and value-count ceilings must trip before the excess node is
//!    built.** A ceiling enforced after construction has already paid for the
//!    allocation it exists to prevent.
//! 3. **A number's source token must survive verbatim.** A JSON-RPC
//!    identifier is echoed into the response unchanged, and an exponent, a
//!    fraction, a large integer, or a negative zero does not survive a
//!    round trip through a binary float. The reader records the source span
//!    of each root member so the envelope layer can copy the identifier's
//!    original text.
//!
//! The walk is iterative over an explicit stack of partially built
//! containers, so a deeply nested body cannot exhaust the call stack before
//! the depth ceiling has a chance to speak.

use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::mem;
use std::ops::Range;

/// Default JSON nesting levels accepted in a request body.
pub const DEFAULT_MAXIMUM_DEPTH: usize = 128;
/// Default JSON value nodes accepted in a request body.
pub const DEFAULT_MAXIMUM_VALUES: usize = 262_144;

/// What the reader will accept before refusing a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseLimits {
    pub maximum_depth: usize,
    pub maximum_values: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self { maximum_depth: DEFAULT_MAXIMUM_DEPTH, maximum_values: DEFAULT_MAXIMUM_VALUES }
    }
}

/// A parsed body, plus the source spans of its root object's members.
#[derive(Debug, Clone)]
pub struct Document {
    /// The parsed value.
    pub value: Value,
    /// Where each root-object member's value sits in the source text. Empty
    /// when the root is not an object.
    pub member_spans: BTreeMap<String, Range<usize>>,
}

impl Document {
    /// The verbatim source text of a root member, if the root was an object
    /// and carried that member.
    #[must_use]
    pub fn member_source<'a>(&self, source: &'a str, name: &str) -> Option<&'a str> {
        self.member_spans.get(name).and_then(|span| source.get(span.clone()))
    }
}

/// Why a body is not acceptable JSON for this boundary.
///
/// [`JsonError::Duplicate`], [`JsonError::DepthExceeded`], and
/// [`JsonError::ValuesExceeded`] are *invalid request* refusals rather than
/// parse failures: the text was legal JSON and the boundary declined it. The
/// envelope layer maps them accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonError {
    /// The text is not JSON.
    Malformed { offset: usize, reason: &'static str },
    /// One object carries the same member name twice.
    Duplicate { name: String },
    /// The body nests deeper than the limit allows.
    DepthExceeded { maximum: usize },
    /// The body holds more values than the limit allows.
    ValuesExceeded { maximum: usize },
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { offset, reason } => write!(f, "malformed JSON at byte {offset}: {reason}"),
            Self::Duplicate { name } => write!(f, "duplicate member `{name}`"),
            Self::DepthExceeded { maximum } => write!(f, "nesting deeper than {maximum} levels"),
            Self::ValuesExceeded { maximum } => write!(f, "more than {maximum} values"),
        }
    }
}

impl Error for JsonError {}

/// Read one JSON document under the given limits.
pub fn parse(source: &str, limits: ParseLimits) -> Result<Document, JsonError> {
    Reader { cursor: Cursor::new(source), limits, values: 0, root_member_start: 0, member_spans: BTreeMap::new() }.run()
}

/// A container being built, and for an object the member name whose value is
/// currently being read.
enum Partial {
    Array(Vec<Value>),
    Object { members: Map<String, Value>, name: String },
}

struct Reader<'a> {
    cursor: Cursor<'a>,
    limits: ParseLimits,
    values: usize,
    /// Where the root object's member value currently being read began.
    root_member_start: usize,
    member_spans: BTreeMap<String, Range<usize>>,
}

/// Whether the value about to be read, or just read, belongs to the root
/// object. Only those values get their source spans recorded.
fn at_root_object(stack: &[Partial]) -> bool {
    matches!(stack.first(), Some(Partial::Object { .. })) && stack.len() == 1
}

impl Reader<'_> {
    fn run(mut self) -> Result<Document, JsonError> {
        let mut stack: Vec<Partial> = Vec::new();
        let root = self.read_value(&mut stack)?;

        self.cursor.skip_whitespace();
        if !self.cursor.is_at_end() {
            return Err(self.cursor.malformed("trailing content after the document"));
        }

        Ok(Document { value: root, member_spans: self.member_spans })
    }

    /// Read one complete value, expanding containers on the stack rather than
    /// on the call stack.
    fn read_value(&mut self, stack: &mut Vec<Partial>) -> Result<Value, JsonError> {
        loop {
            self.cursor.skip_whitespace();
            if at_root_object(stack) {
                self.root_member_start = self.cursor.offset();
            }
            self.charge_value()?;

            // Opening a container suspends this value and continues with its
            // first element; anything else completes immediately.
            let Some(mut value) = self.open_container(stack)? else {
                continue;
            };
            let mut end = self.cursor.offset();

            // Close every container this value completed, innermost first.
            loop {
                let root_member = at_root_object(stack);
                let Some(mut top) = stack.pop() else {
                    return Ok(value);
                };

                match &mut top {
                    Partial::Array(items) => {
                        items.push(value);
                        self.cursor.skip_whitespace();
                        if self.cursor.eat(b',') {
                            stack.push(top);
                            break;
                        }
                        self.cursor.expect(b']', "expected `,` or `]`")?;
                    }
                    Partial::Object { members, name } => {
                        if members.contains_key(name.as_str()) {
                            return Err(JsonError::Duplicate { name: mem::take(name) });
                        }
                        if root_member {
                            self.member_spans.insert(name.clone(), self.root_member_start..end);
                        }
                        members.insert(mem::take(name), value);

                        self.cursor.skip_whitespace();
                        if self.cursor.eat(b',') {
                            *name = self.read_member_name()?;
                            stack.push(top);
                            break;
                        }
                        self.cursor.expect(b'}', "expected `,` or `}`")?;
                    }
                }

                value = match top {
                    Partial::Array(items) => Value::Array(items),
                    Partial::Object { members, .. } => Value::Object(members),
                };
                end = self.cursor.offset();
            }
        }
    }

    /// Begin a value. Returns the finished value, or `None` when a container
    /// was opened and its first element must be read next.
    fn open_container(&mut self, stack: &mut Vec<Partial>) -> Result<Option<Value>, JsonError> {
        match self.cursor.peek() {
            Some(b'{') => {
                self.cursor.bump();
                self.enter(stack)?;
                self.cursor.skip_whitespace();
                if self.cursor.eat(b'}') {
                    return Ok(Some(Value::Object(Map::new())));
                }
                let name = self.read_member_name()?;
                stack.push(Partial::Object { members: Map::new(), name });
                Ok(None)
            }
            Some(b'[') => {
                self.cursor.bump();
                self.enter(stack)?;
                self.cursor.skip_whitespace();
                if self.cursor.eat(b']') {
                    return Ok(Some(Value::Array(Vec::new())));
                }
                stack.push(Partial::Array(Vec::new()));
                Ok(None)
            }
            _ => self.cursor.read_scalar().map(Some),
        }
    }

    /// Charge one nesting level, refusing before the container is pushed.
    fn enter(&self, stack: &[Partial]) -> Result<(), JsonError> {
        if stack.len() + 1 > self.limits.maximum_depth {
            return Err(JsonError::DepthExceeded { maximum: self.limits.maximum_depth });
        }
        Ok(())
    }

    /// Charge one value node, refusing before it is constructed.
    fn charge_value(&mut self) -> Result<(), JsonError> {
        self.values += 1;
        if self.values > self.limits.maximum_values {
            return Err(JsonError::ValuesExceeded { maximum: self.limits.maximum_values });
        }
        Ok(())
    }

    /// Read `"name":` and leave the cursor on the member's value.
    fn read_member_name(&mut self) -> Result<String, JsonError> {
        self.cursor.skip_whitespace();
        let name = self.cursor.read_string()?;
        self.cursor.skip_whitespace();
        self.cursor.expect(b':', "expected `:` after a member name")?;
        Ok(name)
    }
}

struct Cursor<'a> {
    source: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(source: &'a str) -> Self {
        Self { source: source.as_bytes(), offset: 0 }
    }

    fn offset(&self) -> usize {
        self.offset
    }

    fn is_at_end(&self) -> bool {
        self.offset >= self.source.len()
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.offset).copied()
    }

    fn bump(&mut self) {
        self.offset += 1;
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.bump();
            return true;
        }
        false
    }

    fn expect(&mut self, byte: u8, reason: &'static str) -> Result<(), JsonError> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.malformed(reason))
        }
    }

    fn malformed(&self, reason: &'static str) -> JsonError {
        JsonError::Malformed { offset: self.offset, reason }
    }

    /// JSON whitespace is exactly these four bytes; no comments, no other
    /// separators.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    fn read_scalar(&mut self) -> Result<Value, JsonError> {
        match self.peek() {
            Some(b'"') => self.read_string().map(Value::String),
            Some(b't') => self.read_literal("true", Value::Bool(true)),
            Some(b'f') => self.read_literal("false", Value::Bool(false)),
            Some(b'n') => self.read_literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.read_number(),
            _ => Err(self.malformed("expected a value")),
        }
    }

    fn read_literal(&mut self, literal: &str, value: Value) -> Result<Value, JsonError> {
        if self.source[self.offset..].starts_with(literal.as_bytes()) {
            self.offset += literal.len();
            return Ok(value);
        }
        Err(self.malformed("expected `true`, `false`, or `null`"))
    }

    /// Read a JSON number and keep its exact source token.
    ///
    /// The token is the truth the response echoes; the parsed `f64` or
    /// integer is only what the value tree carries. A token whose magnitude
    /// has no finite binary representation is a parse failure rather than a
    /// silent infinity.
    fn read_number(&mut self) -> Result<Value, JsonError> {
        let start = self.offset;
        self.eat(b'-');

        if self.eat(b'0') {
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.malformed("a leading zero is not a valid number"));
            }
        } else if !self.take_digits() {
            return Err(self.malformed("expected a digit"));
        }

        if self.eat(b'.') && !self.take_digits() {
            return Err(self.malformed("expected a digit after `.`"));
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.bump();
            let _ = self.eat(b'+') || self.eat(b'-');
            if !self.take_digits() {
                return Err(self.malformed("expected a digit in the exponent"));
            }
        }

        let token =
            str::from_utf8(&self.source[start..self.offset]).map_err(|_| self.malformed("expected a number"))?;
        parse_number_token(token).map(Value::Number).ok_or_else(|| self.malformed("number out of range"))
    }

    fn take_digits(&mut self) -> bool {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
        }
        self.offset > start
    }

    fn read_string(&mut self) -> Result<String, JsonError> {
        self.expect(b'"', "expected a string")?;
        let mut out = String::new();

        loop {
            let start = self.offset;
            while !matches!(self.peek(), None | Some(b'"' | b'\\' | 0x00..=0x1f)) {
                self.bump();
            }
            out.push_str(
                str::from_utf8(&self.source[start..self.offset])
                    .map_err(|_| self.malformed("invalid UTF-8 in a string"))?,
            );

            match self.peek() {
                Some(b'"') => {
                    self.bump();
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.bump();
                    out.push(self.read_escape()?);
                }
                Some(_) => return Err(self.malformed("a control character must be escaped")),
                None => return Err(self.malformed("unterminated string")),
            }
        }
    }

    fn read_escape(&mut self) -> Result<char, JsonError> {
        let escape = self.peek().ok_or_else(|| self.malformed("unterminated escape"))?;
        self.bump();

        Ok(match escape {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{0008}',
            b'f' => '\u{000c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.read_unicode_escape(),
            _ => return Err(self.malformed("unknown escape")),
        })
    }

    /// A `\u` escape, joining a surrogate pair when one is present.
    fn read_unicode_escape(&mut self) -> Result<char, JsonError> {
        let high = self.read_hex4()?;
        if !(0xd800..0xdc00).contains(&high) {
            return char::from_u32(u32::from(high)).ok_or_else(|| self.malformed("invalid escape codepoint"));
        }

        if !(self.eat(b'\\') && self.eat(b'u')) {
            return Err(self.malformed("expected the low half of a surrogate pair"));
        }
        let low = self.read_hex4()?;
        if !(0xdc00..0xe000).contains(&low) {
            return Err(self.malformed("expected the low half of a surrogate pair"));
        }

        let combined = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
        char::from_u32(combined).ok_or_else(|| self.malformed("invalid escape codepoint"))
    }

    fn read_hex4(&mut self) -> Result<u16, JsonError> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|byte| char::from(byte).to_digit(16))
                .ok_or_else(|| self.malformed("expected four hexadecimal digits"))?;
            self.bump();
            value = value * 16 + u16::try_from(digit).unwrap_or_default();
        }
        Ok(value)
    }
}

/// Turn a validated JSON number token into a `serde_json::Number`.
///
/// Integer tokens keep their exact integer value; everything else becomes the
/// nearest `f64`, and a token whose magnitude that cannot represent finitely
/// yields `None`.
pub fn parse_number_token(token: &str) -> Option<Number> {
    if !token.contains(['.', 'e', 'E']) {
        if let Ok(signed) = token.parse::<i64>() {
            return Some(Number::from(signed));
        }
        if let Ok(unsigned) = token.parse::<u64>() {
            return Some(Number::from(unsigned));
        }
    }
    token.parse::<f64>().ok().filter(|float| float.is_finite()).and_then(Number::from_f64)
}
