use aether_character_creator::{MORPH_TARGETS, generate_head_glb};
use serde_json::Value;

#[test]
fn generated_head_is_a_self_contained_glb_with_named_morphs() {
    let glb = generate_head_glb().expect("head document should serialize");
    assert!(glb.len() < 10_000_000, "generated GLB should keep welded geometry compact");
    assert_eq!(&glb[..4], b"glTF");
    assert_eq!(u32::from_le_bytes(glb[4..8].try_into().expect("version bytes")), 2);
    assert_eq!(u32::from_le_bytes(glb[8..12].try_into().expect("length bytes")) as usize, glb.len());

    let json_length = u32::from_le_bytes(glb[12..16].try_into().expect("JSON length bytes")) as usize;
    assert_eq!(u32::from_le_bytes(glb[16..20].try_into().expect("JSON kind bytes")), 0x4e4f_534a);
    let document: Value = serde_json::from_slice(&glb[20..20 + json_length]).expect("valid GLB JSON");

    assert_eq!(document["asset"]["version"], "2.0");
    assert_eq!(document["buffers"][0]["uri"], Value::Null);
    assert_eq!(document["scenes"].as_array().expect("scenes array").len(), 1);
    assert_eq!(document["nodes"].as_array().expect("nodes array").len(), 15);
    assert_eq!(document["meshes"].as_array().expect("meshes array").len(), 8);
    assert_eq!(document["images"], Value::Null);

    let names = document["meshes"][0]["extras"]["targetNames"].as_array().expect("targetNames array");
    assert_eq!(names.len(), MORPH_TARGETS.len());
    for (actual, expected) in names.iter().zip(MORPH_TARGETS) {
        assert_eq!(actual, expected);
    }
    assert_eq!(
        document["meshes"][0]["primitives"][0]["targets"].as_array().expect("targets array").len(),
        MORPH_TARGETS.len()
    );
}
