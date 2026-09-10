//! Restricted adapter over a PDF's embedded outline (bookmarks).
//!
//! `lopdf` already exposes low-level outline primitives
//! (`Document::get_outlines`, `Document::get_toc`), but those are unsuitable
//! for direct exposure to scripting runtimes:
//!
//! * they accept `GoToR` (remote-file) destinations alongside `GoTo`,
//! * they abort the whole table of contents on the first malformed entry,
//! * they walk `/First` / `/Next` links with no visited-set, depth limit, or
//!   node budget, so a cyclic outline never terminates.
//!
//! This module therefore implements its own bounded traversal and exposes
//! only stable plain data:
//!
//! * [`OutlineItem`] — `{ title, level, physical_page }`, where `level` is
//!   1-based (top-level entries are `1`, matching `lopdf`'s `TocType::level`)
//!   and `physical_page` is the 1-based physical page, or `None` when the
//!   entry cannot be resolved to a page in this document.
//! * [`OutlineResult`] — `{ items, unresolved_count }`.
//!
//! # Semantics
//!
//! * No embedded outline is a normal success: `items` is empty and
//!   `unresolved_count` is `0`.
//! * Only destinations inside the current PDF are resolved: explicit
//!   destination arrays (`/Dest [page /XYZ …]`, `/A << /S /GoTo /D […] >>`)
//!   and named destinations (`/Dest (name)`, `/D (name)`).
//! * External / executable actions — `GoToR`, `URI`, `Launch`, `JavaScript`,
//!   and anything else that is not `GoTo` — are never followed. The entry
//!   keeps its safe title and level with `physical_page = None` and is
//!   counted in `unresolved_count`. Entries with a missing, dangling, or
//!   non-page destination degrade the same way. This single uniform rule
//!   means every visitable outline dictionary yields exactly one item, so
//!   hierarchy is always preserved.
//! * Titles decode plain bytes as UTF-8 and `FE FF` / `FF FE` prefixed
//!   strings as UTF-16BE / UTF-16LE. Anything else falls back to
//!   [`FALLBACK_TITLE`]; control characters are stripped and over-long
//!   titles are truncated to [`MAX_TITLE_BYTES`] bytes.
//! * Nothing but title text, levels, page numbers, and the unresolved count
//!   ever leaves this module: no object references, action dictionaries,
//!   URIs, JavaScript, file paths, or document body text.
//!
//! # Resource bounds (deterministic degradation)
//!
//! * [`MAX_OUTLINE_NODES`] entries are emitted at most; traversal stops
//!   afterwards in document order (pre-order DFS).
//! * Nesting deeper than [`MAX_OUTLINE_DEPTH`] is not descended into.
//! * Revisited outline dictionaries (cyclic `/Next` / `/First` links) are
//!   skipped via a visited-set, so malformed outlines always terminate.
//! * Reference indirections are capped ([`MAX_REF_HOPS`]) and the named
//!   destination table is capped ([`MAX_NAMED_DESTS`]).

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};

use crate::PdfError;

/// Fallback title when an outline entry has no decodable title.
pub const FALLBACK_TITLE: &str = "(untitled)";
/// Maximum outline entries emitted; traversal stops deterministically after
/// this many in document order.
pub const MAX_OUTLINE_NODES: usize = 5000;
/// Maximum outline nesting depth descended into (`1` = top level only).
pub const MAX_OUTLINE_DEPTH: u32 = 32;
/// Maximum reference indirections followed while resolving one destination.
const MAX_REF_HOPS: usize = 8;
/// Maximum named destinations indexed from `/Dests` / `/Names`.
const MAX_NAMED_DESTS: usize = 5000;
/// Maximum name-tree nodes visited while indexing named destinations.
const MAX_NAME_TREE_NODES: usize = 1024;
/// Maximum title size in bytes (truncated on a character boundary).
const MAX_TITLE_BYTES: usize = 1024;

/// One embedded-outline (bookmark) entry with stable plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    /// Decoded, sanitized title (never empty; see [`FALLBACK_TITLE`]).
    pub title: String,
    /// 1-based nesting depth: top-level entries are `1`.
    pub level: u32,
    /// 1-based physical page, or `None` when the entry's destination is
    /// missing, external (`GoToR` / `URI` / …), or otherwise unresolvable
    /// inside this document.
    pub physical_page: Option<u32>,
}

/// Bounded embedded-outline extraction result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutlineResult {
    /// Entries in document order (pre-order depth-first traversal).
    pub items: Vec<OutlineItem>,
    /// Number of entries with `physical_page == None`.
    pub unresolved_count: u32,
}

/// Extract the embedded outline from a PDF in memory.
///
/// Returns an empty [`OutlineResult`] when the PDF has no outline. I/O,
/// parse, and encryption failures propagate as [`PdfError`] through the
/// normal load path — they are never masqueraded as "no outline".
pub fn extract_embedded_outline_mem(buffer: &[u8]) -> Result<OutlineResult, PdfError> {
    crate::validate_pdf_bytes(buffer)?;
    let (doc, _page_count) = crate::load_document_from_mem(buffer)?;
    Ok(extract_embedded_outline(&doc))
}

/// Extract the embedded outline from an already-loaded document.
///
/// Never fails: a missing or malformed outline yields an empty or partially
/// degraded [`OutlineResult`], never a panic or an error.
pub fn extract_embedded_outline(doc: &Document) -> OutlineResult {
    extract_with_limits(doc, MAX_OUTLINE_NODES, MAX_OUTLINE_DEPTH)
}

/// Bounded core; `pub(crate)` so tests can prove each cap terminates.
pub(crate) fn extract_with_limits(
    doc: &Document,
    max_nodes: usize,
    max_depth: u32,
) -> OutlineResult {
    let page_of: HashMap<ObjectId, u32> = doc
        .get_pages()
        .into_iter()
        .map(|(page_num, page_id)| (page_id, page_num))
        .collect();
    let named = collect_named_dests(doc);
    let mut walker = Walker {
        doc,
        page_of: &page_of,
        named: &named,
        visited: HashSet::new(),
        items: Vec::new(),
        unresolved_count: 0,
        max_nodes,
        max_depth: max_depth.max(1),
        truncated: false,
    };
    if let Some(first) = outline_first(doc) {
        walker.walk_siblings(&first, 1);
    }
    OutlineResult {
        items: walker.items,
        unresolved_count: walker.unresolved_count,
    }
}

/// The `/Outlines /First` link object, if the catalog carries an outline.
fn outline_first(doc: &Document) -> Option<Object> {
    let outlines = doc.catalog().ok()?.get(b"Outlines").ok()?.clone();
    let outlines = deref_owned(doc, &outlines)?;
    match outlines {
        Object::Dictionary(dict) => dict.get(b"First").ok().cloned(),
        _ => None,
    }
}

struct Walker<'a> {
    doc: &'a Document,
    page_of: &'a HashMap<ObjectId, u32>,
    named: &'a HashMap<Vec<u8>, Vec<Object>>,
    visited: HashSet<ObjectId>,
    items: Vec<OutlineItem>,
    unresolved_count: u32,
    max_nodes: usize,
    max_depth: u32,
    truncated: bool,
}

impl Walker<'_> {
    /// Walk one sibling level (`/First` … `/Next` chain) at `level`.
    fn walk_siblings(&mut self, first: &Object, level: u32) {
        let mut current = self.resolve_dict(first);
        while let Some((id, dict)) = current {
            if self.items.len() >= self.max_nodes {
                self.truncated = true;
                break;
            }
            if let Some(id) = id {
                if !self.visited.insert(id) {
                    // Cyclic `/Next` link: stop this level instead of looping.
                    break;
                }
            }
            self.emit(&dict, level);
            if level < self.max_depth {
                if let Ok(first_child) = dict.get(b"First") {
                    let first_child = first_child.clone();
                    self.walk_siblings(&first_child, level + 1);
                }
            }
            current = dict
                .get(b"Next")
                .ok()
                .cloned()
                .and_then(|next| self.resolve_dict(&next));
        }
    }

    /// Emit exactly one item per visitable outline dictionary (uniform
    /// unresolved rule: keep safe title/level, `physical_page = None`).
    fn emit(&mut self, dict: &Dictionary, level: u32) {
        let title = self.node_title(dict);
        let physical_page = self.node_page(dict);
        if physical_page.is_none() {
            self.unresolved_count += 1;
        }
        self.items.push(OutlineItem {
            title,
            level,
            physical_page,
        });
    }

    /// Resolve a sibling/child link to an owned dictionary. Returns the
    /// object id (when indirect, for cycle detection) and a clone — outline
    /// dictionaries are tiny, and owning avoids borrow clashes during the
    /// mutable traversal.
    fn resolve_dict(&self, obj: &Object) -> Option<(Option<ObjectId>, Dictionary)> {
        match obj {
            Object::Reference(id) => self
                .doc
                .get_dictionary(*id)
                .ok()
                .map(|dict| (Some(*id), dict.clone())),
            Object::Dictionary(dict) => Some((None, dict.clone())),
            _ => None,
        }
    }

    fn node_title(&self, dict: &Dictionary) -> String {
        let title = dict
            .get(b"Title")
            .ok()
            .and_then(|obj| deref_owned(self.doc, obj));
        let bytes: Option<&[u8]> = match title.as_ref() {
            Some(Object::String(bytes, _)) => Some(bytes),
            Some(Object::Name(bytes)) => Some(bytes),
            _ => None,
        };
        match bytes {
            Some(bytes) => decode_outline_title(bytes),
            None => FALLBACK_TITLE.to_string(),
        }
    }

    /// Resolve an entry to its 1-based physical page.
    ///
    /// `/A` takes precedence when both `/A` and `/Dest` are present. Only
    /// `/S /GoTo` is followed; every other action (`GoToR`, `URI`,
    /// `Launch`, `JavaScript`, …) yields `None` without touching its payload.
    fn node_page(&self, dict: &Dictionary) -> Option<u32> {
        if let Ok(action_obj) = dict.get(b"A") {
            let action = deref_owned(self.doc, action_obj)?;
            let action_dict = action.as_dict().ok()?;
            let kind = action_dict.get(b"S").ok()?.as_name().ok()?;
            if kind != b"GoTo" {
                return None;
            }
            let dest = action_dict.get(b"D").ok()?;
            return self.dest_to_page(dest);
        }
        let dest = dict.get(b"Dest").ok()?;
        self.dest_to_page(dest)
    }

    /// Resolve a destination value (direct array, reference, dictionary with
    /// `/D`, or named-destination string/name) to a 1-based physical page.
    fn dest_to_page(&self, dest: &Object) -> Option<u32> {
        let mut current = dest.clone();
        for _ in 0..MAX_REF_HOPS {
            match current {
                Object::Reference(id) => {
                    current = self.doc.get_object(id).ok()?.clone();
                }
                Object::Array(array) => {
                    let page_id = match array.first()? {
                        Object::Reference(id) => *id,
                        _ => return None,
                    };
                    return self.page_of.get(&page_id).copied();
                }
                Object::Dictionary(dict) => {
                    current = dict.get(b"D").ok()?.clone();
                }
                Object::String(bytes, _) => {
                    current = Object::Array(self.named.get(&bytes)?.clone());
                }
                Object::Name(bytes) => {
                    current = Object::Array(self.named.get(&bytes)?.clone());
                }
                _ => return None,
            }
        }
        None
    }
}

/// Follow reference indirections (capped); returns the owned target.
fn deref_owned(doc: &Document, obj: &Object) -> Option<Object> {
    let mut current = obj.clone();
    for _ in 0..MAX_REF_HOPS {
        match current {
            Object::Reference(id) => {
                current = doc.get_object(id).ok()?.clone();
            }
            _ => return Some(current),
        }
    }
    None
}

/// Decode an outline title: `FE FF` / `FF FE` prefixes select UTF-16BE /
/// UTF-16LE, anything else is UTF-8 (lossy). Control characters are
/// stripped, over-long titles truncated, and empty results fall back to
/// [`FALLBACK_TITLE`] so callers always get safe displayable text.
pub(crate) fn decode_outline_title(bytes: &[u8]) -> String {
    let decoded = if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (u16::from(pair[0]) << 8) | u16::from(pair[1]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (u16::from(pair[1]) << 8) | u16::from(pair[0]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    let mut text: String = decoded.chars().filter(|c| !c.is_control()).collect();
    if text.len() > MAX_TITLE_BYTES {
        let mut end = MAX_TITLE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        FALLBACK_TITLE.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Index named destinations from catalog `/Dests` and the `/Names /Dests`
/// name tree. Both tables are capped; failures degrade to fewer entries.
fn collect_named_dests(doc: &Document) -> HashMap<Vec<u8>, Vec<Object>> {
    let mut out = HashMap::new();
    let Ok(catalog) = doc.catalog() else {
        return out;
    };
    if let Ok(dests_obj) = catalog.get(b"Dests") {
        if let Some(Object::Dictionary(dests)) = deref_owned(doc, dests_obj) {
            for (key, value) in dests.iter() {
                if out.len() >= MAX_NAMED_DESTS {
                    break;
                }
                if let Some(array) = dest_value_to_array(doc, value) {
                    out.entry(key.clone()).or_insert(array);
                }
            }
        }
    }
    if let Ok(names_obj) = catalog.get(b"Names") {
        if let Some(Object::Dictionary(names)) = deref_owned(doc, names_obj) {
            if let Ok(dests_obj) = names.get(b"Dests") {
                let dests_obj = dests_obj.clone();
                walk_name_tree(doc, &dests_obj, &mut out, &mut HashSet::new(), 0);
            }
        }
    }
    out
}

/// Walk one name-tree node (`/Names` leaves, `/Kids` branches), bounded by a
/// visited-set and [`MAX_NAME_TREE_NODES`].
fn walk_name_tree(
    doc: &Document,
    node_obj: &Object,
    out: &mut HashMap<Vec<u8>, Vec<Object>>,
    visited: &mut HashSet<ObjectId>,
    depth: usize,
) {
    if out.len() >= MAX_NAMED_DESTS || depth > 64 {
        return;
    }
    let Some(node) = deref_owned(doc, node_obj) else {
        return;
    };
    let Ok(dict) = node.as_dict() else {
        return;
    };
    if let Ok(names) = dict.get(b"Names") {
        if let Some(names) = deref_owned(doc, names) {
            if let Ok(pairs) = names.as_array() {
                let mut iter = pairs.iter();
                while out.len() < MAX_NAMED_DESTS {
                    let (Some(key), Some(value)) = (iter.next(), iter.next()) else {
                        break;
                    };
                    let key_bytes: Option<&[u8]> = match key {
                        Object::String(bytes, _) => Some(bytes),
                        Object::Name(bytes) => Some(bytes),
                        _ => None,
                    };
                    if let Some(key_bytes) = key_bytes {
                        if let Some(array) = dest_value_to_array(doc, value) {
                            out.entry(key_bytes.to_vec()).or_insert(array);
                        }
                    }
                }
            }
        }
    }
    if let Ok(kids) = dict.get(b"Kids") {
        if let Some(kids) = deref_owned(doc, kids) {
            if let Ok(kids) = kids.as_array() {
                let kids = kids.clone();
                for kid in &kids {
                    if out.len() >= MAX_NAMED_DESTS || visited.len() >= MAX_NAME_TREE_NODES {
                        break;
                    }
                    if let Object::Reference(id) = kid {
                        if !visited.insert(*id) {
                            continue;
                        }
                    }
                    walk_name_tree(doc, kid, out, visited, depth + 1);
                }
            }
        }
    }
}

/// A named-destination value is either a direct array or a dictionary
/// wrapping one under `/D`, possibly behind references.
fn dest_value_to_array(doc: &Document, value: &Object) -> Option<Vec<Object>> {
    let mut current = value.clone();
    for _ in 0..MAX_REF_HOPS {
        match current {
            Object::Reference(id) => {
                current = doc.get_object(id).ok()?.clone();
            }
            Object::Array(array) => return Some(array),
            Object::Dictionary(dict) => {
                current = dict.get(b"D").ok()?.clone();
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, StringFormat};

    /// Minimal multi-page document: `page_count` pages, catalog, no outline.
    fn bare_doc(page_count: usize) -> (Document, Vec<ObjectId>) {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let mut page_ids = Vec::new();
        for _ in 0..page_count {
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
        let kids: Vec<Object> = page_ids.iter().map(|id| (*id).into()).collect();
        doc.objects.insert(
            pages_id,
            dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => Object::Integer(page_count as i64),
            }
            .into(),
        );
        let catalog_id = doc.new_object_id();
        doc.objects.insert(
            catalog_id,
            dictionary! {
                "Type" => "Catalog",
                "Pages" => pages_id,
            }
            .into(),
        );
        doc.trailer.set("Root", catalog_id);
        (doc, page_ids)
    }

    fn direct_dest(page_id: ObjectId) -> Object {
        Object::Array(vec![
            page_id.into(),
            Object::Name(b"XYZ".to_vec()),
            Object::Null,
            Object::Null,
            Object::Null,
        ])
    }

    /// Insert an outline item dict; returns its id. `parent`/`prev` wiring is
    /// intentionally omitted — the adapter only follows `/First`/`/Next`.
    fn insert_item(
        doc: &mut Document,
        title: Object,
        dest: Option<Object>,
        action: Option<Object>,
        first: Option<ObjectId>,
        next: Option<ObjectId>,
    ) -> ObjectId {
        let mut dict = Dictionary::new();
        dict.set(b"Title", title);
        if let Some(dest) = dest {
            dict.set(b"Dest", dest);
        }
        if let Some(action) = action {
            dict.set(b"A", action);
        }
        if let Some(first) = first {
            dict.set(b"First", Object::Reference(first));
        }
        if let Some(next) = next {
            dict.set(b"Next", Object::Reference(next));
        }
        doc.add_object(Object::Dictionary(dict))
    }

    /// Attach an outline root (`/First`) to the catalog.
    fn attach_outline_root(doc: &mut Document, first: ObjectId) {
        let outlines_id = doc.add_object(dictionary! {
            "First" => first,
            "Last" => first,
            "Count" => Object::Integer(1),
        });
        let catalog_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary_mut(catalog_id).unwrap();
        catalog.set(b"Outlines", Object::Reference(outlines_id));
    }

    // -- A: no outline is a normal empty success ---------------------------

    #[test]
    fn no_outline_yields_empty_success() {
        let (doc, _) = bare_doc(2);
        let result = extract_embedded_outline(&doc);
        assert_eq!(
            result,
            OutlineResult {
                items: Vec::new(),
                unresolved_count: 0,
            }
        );
    }

    #[test]
    fn outline_root_without_first_yields_empty_success() {
        let (mut doc, _) = bare_doc(1);
        let outlines_id = doc.add_object(dictionary! {
            "Count" => Object::Integer(0),
        });
        let catalog_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog_id)
            .unwrap()
            .set(b"Outlines", Object::Reference(outlines_id));
        let result = extract_embedded_outline(&doc);
        assert!(result.items.is_empty());
        assert_eq!(result.unresolved_count, 0);
    }

    // -- B: multi-level outline with direct destinations -------------------

    /// Chapter 1 (p1) → Section 1.1 (p2, child) → Chapter 2 (p2, UTF-16BE).
    fn multilevel_doc() -> (Document, Vec<ObjectId>) {
        let (mut doc, pages) = bare_doc(2);
        let child = insert_item(
            &mut doc,
            Object::string_literal("Section 1.1"),
            Some(direct_dest(pages[1])),
            None,
            None,
            None,
        );
        let chapter2 = insert_item(
            &mut doc,
            Object::String(
                vec![0xFE, 0xFF, 0x00, 0x43, 0x00, 0x68, 0x00, 0x2E],
                StringFormat::Literal,
            ),
            Some(direct_dest(pages[1])),
            None,
            None,
            None,
        );
        let chapter1 = insert_item(
            &mut doc,
            Object::string_literal("Chapter 1"),
            Some(direct_dest(pages[0])),
            None,
            Some(child),
            Some(chapter2),
        );
        attach_outline_root(&mut doc, chapter1);
        (doc, pages)
    }

    #[test]
    fn multilevel_direct_destinations_keep_title_level_page() {
        let (doc, _) = multilevel_doc();
        let result = extract_embedded_outline(&doc);
        assert_eq!(
            result.items,
            vec![
                OutlineItem {
                    title: "Chapter 1".to_string(),
                    level: 1,
                    physical_page: Some(1),
                },
                OutlineItem {
                    title: "Section 1.1".to_string(),
                    level: 2,
                    physical_page: Some(2),
                },
                OutlineItem {
                    title: "Ch.".to_string(),
                    level: 1,
                    physical_page: Some(2),
                },
            ]
        );
        assert_eq!(result.unresolved_count, 0);
    }

    #[test]
    fn goto_action_with_direct_dest_resolves() {
        let (mut doc, pages) = bare_doc(1);
        let action = dictionary! {
            "S" => Object::Name(b"GoTo".to_vec()),
            "D" => direct_dest(pages[0]),
        };
        let item = insert_item(
            &mut doc,
            Object::string_literal("Via action"),
            None,
            Some(action.into()),
            None,
            None,
        );
        attach_outline_root(&mut doc, item);
        let result = extract_embedded_outline(&doc);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].physical_page, Some(1));
        assert_eq!(result.unresolved_count, 0);
    }

    // -- C: named destinations ---------------------------------------------

    fn named_dest_doc() -> Document {
        let (mut doc, pages) = bare_doc(3);
        // Catalog /Names /Dests name tree: (chap3) -> [page3 /Fit].
        let dest_array = Object::Array(vec![pages[2].into(), Object::Name(b"Fit".to_vec())]);
        let tree_id = doc.add_object(dictionary! {
            "Names" => vec![
                Object::string_literal("chap3"),
                dest_array,
            ],
        });
        let names_id = doc.add_object(dictionary! {
            "Dests" => Object::Reference(tree_id),
        });
        let catalog_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(catalog_id)
            .unwrap()
            .set(b"Names", Object::Reference(names_id));
        // One entry via /Dest name, one via GoTo /D name.
        let action = dictionary! {
            "S" => Object::Name(b"GoTo".to_vec()),
            "D" => Object::string_literal("chap3"),
        };
        let second = insert_item(
            &mut doc,
            Object::string_literal("Via GoTo named"),
            None,
            Some(action.into()),
            None,
            None,
        );
        let first = insert_item(
            &mut doc,
            Object::string_literal("Named chapter"),
            Some(Object::string_literal("chap3")),
            None,
            None,
            Some(second),
        );
        attach_outline_root(&mut doc, first);
        doc
    }

    #[test]
    fn named_destinations_resolve_to_physical_pages() {
        let doc = named_dest_doc();
        let result = extract_embedded_outline(&doc);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].title, "Named chapter");
        assert_eq!(result.items[0].physical_page, Some(3));
        assert_eq!(result.items[1].title, "Via GoTo named");
        assert_eq!(result.items[1].physical_page, Some(3));
        assert_eq!(result.unresolved_count, 0);
    }

    // -- D: broken targets degrade without panic ----------------------------

    #[test]
    fn broken_targets_keep_title_and_count_unresolved() {
        let (mut doc, _pages) = bare_doc(2);
        let missing_id = doc.new_object_id(); // never inserted: dangling ref
        let dangling = insert_item(
            &mut doc,
            Object::string_literal("Dangling ref"),
            Some(Object::Reference(missing_id)),
            None,
            None,
            None,
        );
        let mut prev = dangling;
        // /Dest of the wrong type.
        prev = insert_item(
            &mut doc,
            Object::string_literal("Integer dest"),
            Some(Object::Integer(42)),
            None,
            None,
            Some(prev),
        );
        // Destination array pointing at a non-page object.
        let bogus = doc.add_object(Object::string_literal("not a page"));
        prev = insert_item(
            &mut doc,
            Object::string_literal("Non-page target"),
            Some(direct_dest(bogus)),
            None,
            None,
            Some(prev),
        );
        // No destination at all, and a non-string title for good measure.
        prev = insert_item(&mut doc, Object::Integer(7), None, None, None, Some(prev));
        // Unknown named destination.
        let head = insert_item(
            &mut doc,
            Object::string_literal("Unknown name"),
            Some(Object::string_literal("no-such-dest")),
            None,
            None,
            Some(prev),
        );
        // Valid tail entry proves traversal continues past breakage.
        attach_outline_root(&mut doc, head);
        let result = extract_embedded_outline(&doc);
        assert_eq!(result.items.len(), 5);
        assert!(result.items.iter().all(|item| item.physical_page.is_none()));
        assert_eq!(result.items[1].title, FALLBACK_TITLE);
        assert_eq!(result.unresolved_count, 5);
    }

    // -- E: external / executable actions are never followed ----------------

    fn blocked_action_doc(kind: &[u8], payload_key: &[u8], payload: Object) -> Document {
        let (mut doc, pages) = bare_doc(2);
        let mut action = Dictionary::new();
        action.set(b"S", Object::Name(kind.to_vec()));
        action.set(payload_key, payload);
        let item = insert_item(
            &mut doc,
            Object::string_literal(format!("Blocked {}", String::from_utf8_lossy(kind))),
            None,
            Some(Object::Dictionary(action)),
            None,
            None,
        );
        attach_outline_root(&mut doc, item);
        let _ = pages;
        doc
    }

    #[test]
    fn goto_r_is_blocked_even_with_local_page_payload() {
        let (mut doc, pages) = bare_doc(2);
        let mut action = Dictionary::new();
        action.set(b"S", Object::Name(b"GoToR".to_vec()));
        action.set(b"F", Object::string_literal("other.pdf"));
        action.set(b"D", direct_dest(pages[1]));
        let item = insert_item(
            &mut doc,
            Object::string_literal("Remote"),
            None,
            Some(Object::Dictionary(action)),
            None,
            None,
        );
        attach_outline_root(&mut doc, item);
        let result = extract_embedded_outline(&doc);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].title, "Remote");
        assert_eq!(result.items[0].physical_page, None);
        assert_eq!(result.unresolved_count, 1);
    }

    #[test]
    fn uri_launch_and_javascript_actions_are_blocked() {
        for (kind, key, payload) in [
            (
                b"URI".as_slice(),
                b"URI".as_slice(),
                Object::string_literal("https://example.com/secret"),
            ),
            (
                b"Launch".as_slice(),
                b"F".as_slice(),
                Object::string_literal("/tmp/evil.sh"),
            ),
            (
                b"JavaScript".as_slice(),
                b"JS".as_slice(),
                Object::string_literal("app.alert('x')"),
            ),
        ] {
            let doc = blocked_action_doc(kind, key, payload);
            let result = extract_embedded_outline(&doc);
            assert_eq!(result.items.len(), 1, "kind: {kind:?}");
            assert_eq!(result.items[0].physical_page, None, "kind: {kind:?}");
            assert_eq!(result.unresolved_count, 1, "kind: {kind:?}");
            let debug = format!("{:?}", result.items[0]);
            assert!(
                !debug.contains("example.com")
                    && !debug.contains("evil.sh")
                    && !debug.contains("app.alert"),
                "action payload leaked for {kind:?}: {debug}"
            );
        }
    }

    // -- F: cycles, depth, and node caps terminate --------------------------

    #[test]
    fn cyclic_next_link_terminates() {
        let (mut doc, pages) = bare_doc(1);
        let first = doc.new_object_id();
        let second = doc.new_object_id();
        // first -> second -> first (cycle), then first's /First points at
        // itself (child cycle).
        let mut first_dict = Dictionary::new();
        first_dict.set(b"Title", Object::string_literal("One"));
        first_dict.set(b"Dest", direct_dest(pages[0]));
        first_dict.set(b"First", Object::Reference(first));
        first_dict.set(b"Next", Object::Reference(second));
        doc.objects.insert(first, Object::Dictionary(first_dict));
        let mut second_dict = Dictionary::new();
        second_dict.set(b"Title", Object::string_literal("Two"));
        second_dict.set(b"Dest", direct_dest(pages[0]));
        second_dict.set(b"Next", Object::Reference(first));
        doc.objects.insert(second, Object::Dictionary(second_dict));
        attach_outline_root(&mut doc, first);
        let result = extract_embedded_outline(&doc);
        // first (level 1) + second; the child self-loop and the revisited
        // /Next links are skipped.
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].title, "One");
        assert_eq!(result.items[1].title, "Two");
    }

    fn chained_doc(depth: usize) -> Document {
        let (mut doc, pages) = bare_doc(1);
        let mut child: Option<ObjectId> = None;
        for i in (0..depth).rev() {
            let item = insert_item(
                &mut doc,
                Object::string_literal(format!("Level {i}")),
                Some(direct_dest(pages[0])),
                None,
                child,
                None,
            );
            child = Some(item);
        }
        attach_outline_root(&mut doc, child.unwrap());
        doc
    }

    #[test]
    fn depth_cap_stops_descent_deterministically() {
        let doc = chained_doc(5);
        let result = extract_with_limits(&doc, MAX_OUTLINE_NODES, 2);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].level, 1);
        assert_eq!(result.items[1].level, 2);
    }

    #[test]
    fn node_cap_stops_in_document_order() {
        let (mut doc, pages) = bare_doc(1);
        let mut head: Option<ObjectId> = None;
        for i in (0..5).rev() {
            head = Some(insert_item(
                &mut doc,
                Object::string_literal(format!("Item {i}")),
                Some(direct_dest(pages[0])),
                None,
                None,
                head,
            ));
        }
        attach_outline_root(&mut doc, head.unwrap());
        let result = extract_with_limits(&doc, 2, MAX_OUTLINE_DEPTH);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].title, "Item 0");
        assert_eq!(result.items[1].title, "Item 1");
    }

    // -- Title decoding ------------------------------------------------------

    #[test]
    fn title_decoding_covers_utf8_utf16_and_fallback() {
        assert_eq!(decode_outline_title(b"Plain ASCII"), "Plain ASCII");
        assert_eq!(decode_outline_title("Tübingen".as_bytes()), "Tübingen");
        assert_eq!(
            decode_outline_title(&[0xFE, 0xFF, 0x00, 0x41, 0x00, 0x42]),
            "AB"
        );
        assert_eq!(
            decode_outline_title(&[0xFF, 0xFE, 0x41, 0x00, 0x42, 0x00]),
            "AB"
        );
        // Undecodable / empty / control-only titles get the safe fallback.
        assert_eq!(decode_outline_title(b""), FALLBACK_TITLE);
        assert_eq!(decode_outline_title(b"   "), FALLBACK_TITLE);
        assert_eq!(decode_outline_title(b"\x01\x02\x03"), FALLBACK_TITLE);
        assert_eq!(decode_outline_title(b"\xc3\x28"), "�(");
    }

    #[test]
    fn overlong_titles_are_truncated() {
        let long = vec![b'a'; MAX_TITLE_BYTES + 100];
        let decoded = decode_outline_title(&long);
        assert_eq!(decoded.len(), MAX_TITLE_BYTES);
    }
}
