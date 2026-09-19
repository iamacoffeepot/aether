//! Const-assembled `aether.bloomery.reactors` records and a `no_std` decoder.
//!
//! Each reactor a bundle exports declares itself in one record: the reactor
//! `NAMESPACE` plus, per `#[rule]`, the rule name and the trigger/output
//! [`KindId`]s. `#[reactor]` assembles its record in const context with
//! [`write_reactor_record`]; `bundle_reactors` pins one record per selected
//! reactor into the section. wasm-ld concatenates same-named custom sections,
//! so [`reactor_declarations`] walks concatenated records:
//!
//! ```text
//! version:      u8  = 1
//! name_len:     u16 little-endian
//! name:         name_len UTF-8 bytes (ReactorName, the reactor's NAMESPACE)
//! rule_count:   u16 little-endian, >= 1
//! rule_count times:
//!   rule_len:   u16 little-endian
//!   rule:       rule_len UTF-8 bytes (RuleName, the #[rule] method ident)
//!   trigger:    u64 little-endian KindId of the rule's trigger type
//!   output:     u64 little-endian KindId of the rule's return type
//! ```

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;
use core::str;

use aether_data::KindId;

use crate::{ReactorName, RuleName};

/// Record version byte written at the start of every reactor declaration.
pub const SECTION_VERSION: u8 = 1;

/// Custom-section name carrying reactor declarations in a reactor bundle.
pub const REACTORS_SECTION: &str = "aether.bloomery.reactors";

/// One rule's declared name and trigger/output kinds for the const writer.
#[derive(Debug, Clone, Copy)]
pub struct RuleRecord<'a> {
    name: &'a str,
    trigger: KindId,
    output: KindId,
}

impl<'a> RuleRecord<'a> {
    /// Describe one rule's record fields.
    #[must_use]
    pub const fn new(name: &'a str, trigger: KindId, output: KindId) -> Self {
        Self { name, trigger, output }
    }
}

/// Byte length of one declaration record for `name` with `rules`.
#[must_use]
pub const fn reactor_record_len(name: &str, rules: &[RuleRecord<'_>]) -> usize {
    1 + 2 + name.len() + 2 + rules_record_len(rules)
}

const fn rules_record_len(rules: &[RuleRecord<'_>]) -> usize {
    let mut len = 0;
    let mut index = 0;
    while index < rules.len() {
        len += 2 + rules[index].name.len() + 8 + 8;
        index += 1;
    }
    len
}

/// Const-assemble one version-prefixed declaration record.
///
/// # Panics
///
/// Panics when `N` is not [`reactor_record_len`] for the same `name` and
/// `rules`, when `rules` is empty, when the name or a rule name is not a
/// valid [`ReactorName`] / [`RuleName`], or when a length exceeds `u16::MAX`.
#[must_use]
pub const fn write_reactor_record<const N: usize>(name: &str, rules: &[RuleRecord<'_>]) -> [u8; N] {
    assert!(N == reactor_record_len(name, rules), "aether-bloomery-kinds: reactor record length mismatch");
    assert!(!rules.is_empty(), "aether-bloomery-kinds: reactor record needs at least one rule");
    assert!(ReactorName::is_valid(name), "aether-bloomery-kinds: reactor name is not a valid ReactorName");
    check_rule_names(rules);
    let mut out = [0u8; N];
    let mut pos = 0;
    out[pos] = SECTION_VERSION;
    pos += 1;
    write_u16_le(&mut out, &mut pos, u16_len(name.as_bytes()));
    write_slice(&mut out, &mut pos, name.as_bytes());
    write_u16_le(&mut out, &mut pos, u16_count(rules.len()));
    let mut index = 0;
    while index < rules.len() {
        write_u16_le(&mut out, &mut pos, u16_len(rules[index].name.as_bytes()));
        write_slice(&mut out, &mut pos, rules[index].name.as_bytes());
        write_u64_le(&mut out, &mut pos, rules[index].trigger.0);
        write_u64_le(&mut out, &mut pos, rules[index].output.0);
        index += 1;
    }
    let _ = pos;
    out
}

const fn check_rule_names(rules: &[RuleRecord<'_>]) {
    let mut index = 0;
    while index < rules.len() {
        assert!(
            RuleName::is_valid(rules[index].name),
            "aether-bloomery-kinds: reactor rule name is not a valid RuleName"
        );
        index += 1;
    }
}

const fn u16_len(bytes: &[u8]) -> u16 {
    let mut len = 0u16;
    let mut index = 0;
    while index < bytes.len() {
        len = match len.checked_add(1) {
            Some(next) => next,
            None => panic!("aether-bloomery-kinds: reactor name or rule exceeds u16::MAX"),
        };
        index += 1;
    }
    len
}

const fn u16_count(count: usize) -> u16 {
    let mut len = 0u16;
    let mut index = 0;
    while index < count {
        len = match len.checked_add(1) {
            Some(next) => next,
            None => panic!("aether-bloomery-kinds: reactor rule count exceeds u16::MAX"),
        };
        index += 1;
    }
    len
}

const fn write_u16_le(out: &mut [u8], pos: &mut usize, value: u16) {
    let bytes = value.to_le_bytes();
    out[*pos] = bytes[0];
    out[*pos + 1] = bytes[1];
    *pos += 2;
}

const fn write_u64_le(out: &mut [u8], pos: &mut usize, value: u64) {
    let bytes = value.to_le_bytes();
    let mut index = 0;
    while index < 8 {
        out[*pos] = bytes[index];
        *pos += 1;
        index += 1;
    }
}

const fn write_slice(out: &mut [u8], pos: &mut usize, bytes: &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        out[*pos] = bytes[index];
        *pos += 1;
        index += 1;
    }
}

/// One rule's declared name and trigger/output kinds, decoded from a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleDeclaration {
    name: RuleName,
    trigger: KindId,
    output: KindId,
}

impl RuleDeclaration {
    /// Assemble one decoded rule from its already-validated name and kinds.
    #[must_use]
    pub fn new(name: RuleName, trigger: KindId, output: KindId) -> Self {
        Self { name, trigger, output }
    }

    /// The `#[rule]` method name.
    #[must_use]
    pub fn name(&self) -> &RuleName {
        &self.name
    }

    /// Trigger kind of the rule.
    #[must_use]
    pub const fn trigger(&self) -> KindId {
        self.trigger
    }

    /// Output kind of the rule.
    #[must_use]
    pub const fn output(&self) -> KindId {
        self.output
    }
}

/// Why a reactor declaration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactorDeclarationError {
    /// The record declared no rules.
    NoRules,
    /// Two rules in one record share a name.
    DuplicateRule,
}

impl fmt::Display for ReactorDeclarationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRules => f.write_str("reactor declaration declares no rules"),
            Self::DuplicateRule => f.write_str("reactor declaration repeats a rule name"),
        }
    }
}

impl StdError for ReactorDeclarationError {}

/// One reactor's decoded declaration: its name and every rule's kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactorDeclaration {
    name: ReactorName,
    rules: Vec<RuleDeclaration>,
}

impl ReactorDeclaration {
    /// Assemble a declaration from validated parts.
    ///
    /// # Errors
    ///
    /// [`ReactorDeclarationError::NoRules`] when `rules` is empty,
    /// [`ReactorDeclarationError::DuplicateRule`] when two rules share a name.
    pub fn new(name: ReactorName, rules: Vec<RuleDeclaration>) -> Result<Self, ReactorDeclarationError> {
        if rules.is_empty() {
            return Err(ReactorDeclarationError::NoRules);
        }
        let mut seen = BTreeSet::new();
        for rule in &rules {
            if !seen.insert(rule.name()) {
                return Err(ReactorDeclarationError::DuplicateRule);
            }
        }
        Ok(Self { name, rules })
    }

    /// The reactor's `NAMESPACE`.
    #[must_use]
    pub fn name(&self) -> &ReactorName {
        &self.name
    }

    /// Every declared rule in record order.
    #[must_use]
    pub fn rules(&self) -> &[RuleDeclaration] {
        &self.rules
    }
}

/// Why [`reactor_declarations`] refused a custom-section payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactorDeclarationsError {
    /// The remaining bytes were shorter than a record header or declared field.
    Truncated,
    /// The record version byte is not 1.
    UnsupportedVersion(u8),
    /// A name field was not UTF-8.
    InvalidUtf8,
    /// The name field is not a valid [`ReactorName`].
    InvalidReactorName,
    /// A rule field is not a valid [`RuleName`].
    InvalidRuleName,
    /// The record's rules fail [`ReactorDeclaration::new`].
    Declaration(ReactorDeclarationError),
    /// Two records share a reactor name.
    DuplicateReactor(ReactorName),
}

impl fmt::Display for ReactorDeclarationsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated reactor declaration record"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported reactor declaration version {version}"),
            Self::InvalidUtf8 => f.write_str("reactor declaration name is not UTF-8"),
            Self::InvalidReactorName => f.write_str("reactor declaration name is not a ReactorName"),
            Self::InvalidRuleName => f.write_str("reactor declaration rule is not a RuleName"),
            Self::Declaration(error) => write!(f, "reactor declaration invalid: {error}"),
            Self::DuplicateReactor(name) => write!(f, "reactor declaration repeats reactor {}", name.as_str()),
        }
    }
}

impl StdError for ReactorDeclarationsError {}

/// Decode concatenated `aether.bloomery.reactors` custom-section records.
///
/// # Errors
///
/// [`ReactorDeclarationsError`] when a record is truncated, versioned
/// incorrectly, or carries an invalid name, UTF-8 field, rule set, or a
/// repeated reactor name.
pub fn reactor_declarations(section: &[u8]) -> Result<Vec<ReactorDeclaration>, ReactorDeclarationsError> {
    let mut rest = section;
    let mut out: Vec<ReactorDeclaration> = Vec::new();
    while !rest.is_empty() {
        let declaration = read_record(&mut rest)?;
        if out.iter().any(|existing| existing.name() == declaration.name()) {
            return Err(ReactorDeclarationsError::DuplicateReactor(declaration.name().clone()));
        }
        out.push(declaration);
    }
    Ok(out)
}

fn read_record(rest: &mut &[u8]) -> Result<ReactorDeclaration, ReactorDeclarationsError> {
    let version = read_u8(rest)?;
    if version != SECTION_VERSION {
        return Err(ReactorDeclarationsError::UnsupportedVersion(version));
    }
    let name = read_len_prefixed_string(rest)?;
    let name = ReactorName::new(name).map_err(|_| ReactorDeclarationsError::InvalidReactorName)?;
    let rule_count = usize::from(read_u16(rest)?);
    let mut rules = Vec::new();
    let mut index = 0;
    while index < rule_count {
        rules.push(read_rule(rest)?);
        index += 1;
    }
    ReactorDeclaration::new(name, rules).map_err(ReactorDeclarationsError::Declaration)
}

fn read_rule(rest: &mut &[u8]) -> Result<RuleDeclaration, ReactorDeclarationsError> {
    let name = read_len_prefixed_string(rest)?;
    let name = RuleName::new(name).map_err(|_| ReactorDeclarationsError::InvalidRuleName)?;
    let trigger = KindId(read_u64(rest)?);
    let output = KindId(read_u64(rest)?);
    Ok(RuleDeclaration::new(name, trigger, output))
}

fn read_u8(rest: &mut &[u8]) -> Result<u8, ReactorDeclarationsError> {
    let (byte, tail) = rest.split_first().ok_or(ReactorDeclarationsError::Truncated)?;
    *rest = tail;
    Ok(*byte)
}

fn read_u16(rest: &mut &[u8]) -> Result<u16, ReactorDeclarationsError> {
    if rest.len() < 2 {
        return Err(ReactorDeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(2);
    *rest = tail;
    Ok(u16::from_le_bytes([head[0], head[1]]))
}

fn read_u64(rest: &mut &[u8]) -> Result<u64, ReactorDeclarationsError> {
    if rest.len() < 8 {
        return Err(ReactorDeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(8);
    *rest = tail;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(head);
    Ok(u64::from_le_bytes(bytes))
}

fn read_len_prefixed_string(rest: &mut &[u8]) -> Result<String, ReactorDeclarationsError> {
    let len = usize::from(read_u16(rest)?);
    if rest.len() < len {
        return Err(ReactorDeclarationsError::Truncated);
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    str::from_utf8(head).map(String::from).map_err(|_| ReactorDeclarationsError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use aether_data::KindId;

    use super::{
        ReactorDeclarationError, ReactorDeclarationsError, RuleRecord, reactor_declarations, reactor_record_len,
        write_reactor_record,
    };
    use crate::ReactorName;

    // Tripwire: the writer's bytes for one reactor with two rules, computed by
    // hand from the layout. Catches layout drift between the const writer and
    // any reader already built against it (the driver's D9 reader).
    #[test]
    fn tripwire_two_rule_record_bytes_match_the_layout() {
        const RULES: &[RuleRecord<'static>] =
            &[RuleRecord::new("first", KindId(1), KindId(2)), RuleRecord::new("second", KindId(3), KindId(4))];
        const LEN: usize = reactor_record_len("test.reactor", RULES);
        const RECORD: [u8; LEN] = write_reactor_record::<LEN>("test.reactor", RULES);
        assert_eq!(
            RECORD,
            [
                1, // version
                12, 0, // name_len
                b't', b'e', b's', b't', b'.', b'r', b'e', b'a', b'c', b't', b'o', b'r', // name
                2, 0, // rule_count
                5, 0, // rule_len
                b'f', b'i', b'r', b's', b't', // rule
                1, 0, 0, 0, 0, 0, 0, 0, // trigger
                2, 0, 0, 0, 0, 0, 0, 0, // output
                6, 0, // rule_len
                b's', b'e', b'c', b'o', b'n', b'd', // rule
                3, 0, 0, 0, 0, 0, 0, 0, // trigger
                4, 0, 0, 0, 0, 0, 0, 0, // output
            ]
        );
    }

    // Catches a cursor that stops after the first record or mis-advances
    // across a multi-rule record.
    #[test]
    fn two_concatenated_records_decode_in_order_with_their_rules() {
        const FIRST_RULES: &[RuleRecord<'static>] = &[RuleRecord::new("one", KindId(1), KindId(2))];
        const SECOND_RULES: &[RuleRecord<'static>] =
            &[RuleRecord::new("two", KindId(3), KindId(4)), RuleRecord::new("three", KindId(5), KindId(6))];
        const FIRST_LEN: usize = reactor_record_len("test.first", FIRST_RULES);
        const SECOND_LEN: usize = reactor_record_len("test.second", SECOND_RULES);
        let first = write_reactor_record::<FIRST_LEN>("test.first", FIRST_RULES);
        let second = write_reactor_record::<SECOND_LEN>("test.second", SECOND_RULES);
        let mut section = Vec::new();
        section.extend_from_slice(&first);
        section.extend_from_slice(&second);

        let decoded = reactor_declarations(&section).expect("records decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].name().as_str(), "test.first");
        assert_eq!(decoded[0].rules().len(), 1);
        assert_eq!(decoded[0].rules()[0].name().as_str(), "one");
        assert_eq!(decoded[0].rules()[0].trigger(), KindId(1));
        assert_eq!(decoded[0].rules()[0].output(), KindId(2));
        assert_eq!(decoded[1].name().as_str(), "test.second");
        assert_eq!(decoded[1].rules().len(), 2);
        assert_eq!(decoded[1].rules()[0].name().as_str(), "two");
        assert_eq!(decoded[1].rules()[1].name().as_str(), "three");
        assert_eq!(decoded[1].rules()[1].trigger(), KindId(5));
        assert_eq!(decoded[1].rules()[1].output(), KindId(6));
    }

    // Catches a decoder that hands D9 a malformed declaration it would trust.
    #[test]
    fn malformed_sections_are_refused_with_typed_errors() {
        const RULES: &[RuleRecord<'static>] = &[RuleRecord::new("rule", KindId(1), KindId(2))];
        const LEN: usize = reactor_record_len("test.reactor", RULES);
        const DUP_RULES: &[RuleRecord<'static>] =
            &[RuleRecord::new("same", KindId(1), KindId(2)), RuleRecord::new("same", KindId(3), KindId(4))];
        const DUP_LEN: usize = reactor_record_len("test.dup", DUP_RULES);
        let record = write_reactor_record::<LEN>("test.reactor", RULES);
        let duplicate_rule = write_reactor_record::<DUP_LEN>("test.dup", DUP_RULES);

        let mut truncated = record.to_vec();
        truncated.truncate(truncated.len() - 1);
        let mut versioned = record.to_vec();
        versioned[0] = 2;
        let mut doubled = record.to_vec();
        doubled.extend_from_slice(&record);

        let cases: Vec<(&str, Vec<u8>, ReactorDeclarationsError)> = vec![
            ("truncated inside a rule", truncated, ReactorDeclarationsError::Truncated),
            ("version 2", versioned, ReactorDeclarationsError::UnsupportedVersion(2)),
            (
                "invalid reactor name",
                vec![
                    1, 3, 0, b'B', b'a', b'd', 1, 0, 4, 0, b'r', b'u', b'l', b'e', 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
                    0, 0, 0, 0,
                ],
                ReactorDeclarationsError::InvalidReactorName,
            ),
            (
                "invalid rule name",
                vec![
                    1, 12, 0, b't', b'e', b's', b't', b'.', b'r', b'e', b'a', b'c', b't', b'o', b'r', 1, 0, 7, 0, b'_',
                    b'h', b'i', b'd', b'd', b'e', b'n', 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0,
                ],
                ReactorDeclarationsError::InvalidRuleName,
            ),
            (
                "non-UTF-8 name",
                vec![1, 1, 0, 0xff, 1, 0, 4, 0, b'r', b'u', b'l', b'e', 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0],
                ReactorDeclarationsError::InvalidUtf8,
            ),
            (
                "rule_count 0",
                vec![1, 12, 0, b't', b'e', b's', b't', b'.', b'r', b'e', b'a', b'c', b't', b'o', b'r', 0, 0],
                ReactorDeclarationsError::Declaration(ReactorDeclarationError::NoRules),
            ),
            (
                "duplicate rule within a record",
                duplicate_rule.to_vec(),
                ReactorDeclarationsError::Declaration(ReactorDeclarationError::DuplicateRule),
            ),
            (
                "duplicate reactor across records",
                doubled,
                ReactorDeclarationsError::DuplicateReactor(ReactorName::new("test.reactor").expect("valid test name")),
            ),
        ];
        for (label, section, expected) in cases {
            assert_eq!(reactor_declarations(&section), Err(expected), "{label}");
        }
    }
}
