//! Declarations query: every loaded bundle's programs, each with its input and result kinds' names and schemas.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{Digest, KindId};

use crate::{Mode, ProgramName};

/// Ask the driver for the programs of every bundle it holds decoded, read from
/// each bundle's `aether.bloomery.programs` records.
#[aether_data::kind(name = "aether.bloomery.driver.declarations", default, eq, no_serde)]
pub struct Declarations;

/// Reply to one [`Declarations`]: one entry per bundle the driver holds
/// decoded that declares programs.
#[aether_data::kind(name = "aether.bloomery.driver.declarations_result", eq, no_serde)]
pub struct DeclarationsResult {
    pub bundles: Vec<BundleDeclarations>,
}

/// One bundle's declared programs.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct BundleDeclarations {
    /// The bundle's artifact digest.
    pub bundle: Digest,
    pub programs: Vec<ProgramDeclaration>,
}

/// One program as its bundle record declares it.
///
/// Each schema travels as the wire bytes of its `SchemaType`, which is the
/// schema vocabulary rather than a value in it; decode it with
/// `aether_data::wire::from_bytes::<SchemaType>`.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Schema)]
pub struct ProgramDeclaration {
    pub name: ProgramName,
    pub mode: Mode,
    /// One sentence of meaning for a planner.
    pub intent: String,
    /// The input kind's id.
    pub input: KindId,
    /// The input kind's `Kind::NAME`.
    pub input_name: String,
    /// The input kind's named schema, as `SchemaType` wire bytes.
    pub input_schema: Vec<u8>,
    /// The result kind's id.
    pub result: KindId,
    /// The result kind's `Kind::NAME`.
    pub result_name: String,
    /// The result kind's named schema, as `SchemaType` wire bytes.
    pub result_schema: Vec<u8>,
}
