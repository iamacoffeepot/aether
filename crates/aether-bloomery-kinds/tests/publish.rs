use std::error::Error;

use aether_bloomery_kinds::{ClosureArtifact, EncodedArtifact, Head, Publish, Tree};
use aether_data::{Digest, Kind, Ref, Storage, artifact_digest};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.publish.document")]
struct Document {
    title: String,
    tree: Ref<Tree>,
}

#[test]
fn encoded_artifact_keeps_nested_citations_and_a_decodable_payload() -> Result<(), Box<dyn Error>> {
    // Catches a constructor that dropped the citation walk, prefixed the
    // wrong kind, or encoded a payload the kind cannot decode.
    let tree = Ref::<Tree>::from_digest(Digest::from_bytes([7; 32]));
    let document = Document { title: "draft".into(), tree };
    let artifact = EncodedArtifact::new(&document)?;

    assert_eq!(artifact.kind(), Document::ID);
    assert_eq!(artifact.citations().len(), 1);
    assert_eq!(artifact.citations()[0].kind(), Tree::ID);
    assert_eq!(artifact.citations()[0].bytes(), tree.digest().as_bytes());
    let digest = artifact.digest();
    let (kind, payload, _) = artifact.into_parts();
    let payload = ClosureArtifact::new(kind, payload).load(digest)?;
    assert_eq!(Document::decode_storage(&payload)?.value, document);
    assert_eq!(digest, artifact_digest(Document::ID, &payload));
    Ok(())
}

#[test]
fn head_publish_moves_its_head_to_the_published_artifact() -> Result<(), Box<dyn Error>> {
    // Catches a convenience constructor whose move pointed somewhere other
    // than the artifact it staged, or lost the head's kind or the fence.
    let document = Document { title: "main".into(), tree: Ref::from_digest(Digest::from_bytes([8; 32])) };
    let publish = Publish::head(&Head::<Document>::new("main"), &document, 41)?;

    assert_eq!(publish.expected_seq(), 41);
    let [artifact] = publish.artifacts() else {
        panic!("one artifact")
    };
    let [moved] = publish.moves() else {
        panic!("one move")
    };
    assert_eq!(moved.head().kind(), Document::ID);
    assert_eq!(moved.head().as_str(), "main");
    assert_eq!(moved.to(), artifact.digest());
    assert_eq!(artifact, &EncodedArtifact::new(&document)?);
    Ok(())
}
