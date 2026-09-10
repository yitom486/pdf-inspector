//! Buffer-level tests for the embedded-outline adapter: the public
//! `extract_embedded_outline_mem` path (validation + load + traversal),
//! a real fixture without an outline, and load-failure passthrough.

use lopdf::{dictionary, Document, Object, ObjectId};
use pdf_inspector::extract_embedded_outline_mem;

fn two_page_doc_with_outline() -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let pages_id = doc.new_object_id();
    let mut page_ids = Vec::new();
    for _ in 0..2 {
        let page_id = doc.new_object_id();
        doc.objects.insert(
            page_id,
            dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            }
            .into(),
        );
        page_ids.push(page_id);
    }
    let kids: Vec<Object> = page_ids.iter().map(|id| Object::Reference(*id)).collect();
    doc.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => Object::Integer(2),
        }
        .into(),
    );

    // Named destination (chap2) -> [page2 /Fit] via the /Names name tree.
    let dest_array = Object::Array(vec![
        Object::Reference(page_ids[1]),
        Object::Name(b"Fit".to_vec()),
    ]);
    let tree_id = doc.add_object(dictionary! {
        "Names" => vec![Object::string_literal("chap2"), dest_array],
    });
    let names_id = doc.add_object(dictionary! {
        "Dests" => Object::Reference(tree_id),
    });

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
        "Names" => Object::Reference(names_id),
    });
    doc.trailer.set("Root", catalog_id);

    let direct = |page: ObjectId| {
        Object::Array(vec![
            Object::Reference(page),
            Object::Name(b"XYZ".to_vec()),
            Object::Null,
            Object::Null,
            Object::Null,
        ])
    };
    let mut second_dict = lopdf::Dictionary::new();
    second_dict.set(b"Title", Object::string_literal("Second (named)"));
    second_dict.set(b"Dest", Object::string_literal("chap2"));
    let second = doc.add_object(Object::Dictionary(second_dict));

    let mut first_dict = lopdf::Dictionary::new();
    first_dict.set(b"Title", Object::string_literal("First (direct)"));
    first_dict.set(b"Dest", direct(page_ids[0]));
    first_dict.set(b"Next", Object::Reference(second));
    let first = doc.add_object(Object::Dictionary(first_dict));

    let outlines_id = doc.add_object(dictionary! {
        "First" => first,
        "Count" => Object::Integer(2),
    });
    doc.get_dictionary_mut(catalog_id)
        .unwrap()
        .set(b"Outlines", Object::Reference(outlines_id));

    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn mem_roundtrip_resolves_direct_and_named_destinations() {
    let bytes = two_page_doc_with_outline();
    let result = extract_embedded_outline_mem(&bytes).unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.items[0].title, "First (direct)");
    assert_eq!(result.items[0].level, 1);
    assert_eq!(result.items[0].physical_page, Some(1));
    assert_eq!(result.items[1].title, "Second (named)");
    assert_eq!(result.items[1].physical_page, Some(2));
    assert_eq!(result.unresolved_count, 0);
}

#[test]
fn real_fixture_without_outline_is_empty_success() {
    let bytes = std::fs::read("tests/fixtures/thermo-freon12.pdf").unwrap();
    let result = extract_embedded_outline_mem(&bytes).unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.unresolved_count, 0);
}

#[test]
fn invalid_bytes_are_errors_not_empty_outline() {
    assert!(extract_embedded_outline_mem(b"not a pdf").is_err());
    assert!(extract_embedded_outline_mem(b"").is_err());
}

#[test]
fn encrypted_pdf_without_password_is_error() {
    let bytes = std::fs::read("tests/fixtures/encrypted-secret123.pdf").unwrap();
    assert!(extract_embedded_outline_mem(&bytes).is_err());
}
