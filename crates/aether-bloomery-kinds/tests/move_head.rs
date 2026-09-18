use std::error::Error;

use aether_bloomery_kinds::{Head, MoveHead, MoveHeadResult, Ref, Tree, artifact_digest};
use aether_data::{Kind, Storage};

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.move_head.document")]
struct Document {
    title: String,
    tree: Ref<Tree>,
}

#[test]
fn typed_constructor_preserves_encoded_value_and_nested_citations_through_mail() -> Result<(), Box<dyn Error>> {
    let tree = Ref::<Tree>::from_digest(aether_bloomery_kinds::Digest::from_bytes([7; 32]));
    let document = Document { title: "draft".into(), tree };
    let head = Head::<Document>::new("main");
    let command = MoveHead::new(&head, &document, 41)?;
    assert_eq!(command.head().kind(), Document::ID);
    assert_eq!(command.head().as_str(), "main");
    assert_eq!(command.expected_seq(), 41);
    assert_eq!(command.citations().len(), 1);
    assert_eq!(command.citations()[0].kind(), Tree::ID);
    assert_eq!(command.citations()[0].bytes(), tree.digest().as_bytes());
    assert_eq!(Document::decode_storage(command.artifact_bytes())?.value, document);

    let decoded = MoveHead::decode_from_bytes(&command.encode_into_bytes()).expect("decode command mail");
    assert_eq!(decoded, command);
    assert_eq!(decoded.citations()[0].bytes(), tree.digest().as_bytes());
    let artifact = artifact_digest(decoded.head().kind(), decoded.artifact_bytes());
    assert_eq!(
        MoveHeadResult::decode_from_bytes(&MoveHeadResult::Committed { seq: 42, artifact }.encode_into_bytes())
            .expect("decode result mail"),
        MoveHeadResult::Committed { seq: 42, artifact }
    );
    Ok(())
}
