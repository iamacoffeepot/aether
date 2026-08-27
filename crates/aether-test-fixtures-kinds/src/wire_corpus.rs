//! Vocabulary-coverage types for the derived-codec corpus (ADR-0188).
//!
//! These are `Schema` types, not `Kind`s — they exist so the codec
//! conformance harness can instantiate a Rust value for every
//! `SchemaType` arm.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CorpusUnit;

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct CorpusScalars {
    pub u8_field: u8,
    pub u16_field: u16,
    pub u32_field: u32,
    pub u64_field: u64,
    pub i8_field: i8,
    pub i16_field: i16,
    pub i32_field: i32,
    pub i64_field: i64,
    pub f32_field: f32,
    pub f64_field: f64,
    pub flag: bool,
    pub label: String,
}

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CorpusCollections {
    pub tags: Vec<String>,
    pub maybe_some: Option<u64>,
    pub maybe_none: Option<u64>,
    pub triple: [u32; 3],
    pub blob: Vec<u8>,
    pub empty_vec: Vec<u32>,
    pub empty_string: String,
}

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum CorpusSum {
    Pending,
    Ok(u64),
    Pair(u32, i16),
    Err { reason: String },
}

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CorpusNested {
    pub items: Vec<Option<CorpusSum>>,
}

#[derive(aether_data::Schema, serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CorpusMaps {
    pub by_name: BTreeMap<String, u8>,
    pub by_u32: BTreeMap<u32, String>,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, aether_data::Schema, serde::Serialize, serde::Deserialize)]
pub struct CorpusCast {
    pub x: f32,
    pub y: f32,
}
