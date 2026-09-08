//! Throwaway probe, never landed: what this binary (main before ADR-0216's two
//! appended fields) stamps as the schema digest of the sealed instruction
//! bundle. #5822 pins that value as `MODEL_PROCESS_INSTRUCTIONS_PRE_READER_DIGEST`;
//! this asserts the pin against the producing binary rather than a reproduction.

use aether_bloomery::{ModelProcessInstructions, encode_hex, schema_digest};
use aether_data::{Kind, Schema};

#[test]
fn the_pre_reader_instruction_bundle_stamps_the_digest_5822_pins() {
    let digest = schema_digest(ModelProcessInstructions::NAME, &<ModelProcessInstructions as Schema>::SCHEMA)
        .expect("the bundle schema renders");
    assert_eq!(encode_hex(digest.as_bytes()), "c0a9677ad8116334fe7b217401fb5f14b06965ae33af4add641f685c5768f3e6");
}
