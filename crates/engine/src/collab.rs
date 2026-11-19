//! Verto: co-editing — several people in one document at once, as in Google Docs.
//!
//! The document stays WordCraft's own; a CRDT (Loro) mirrors it. After each top-level command the
//! session compares the document with the one the mirror matched ([`Collab::capture`]) and writes
//! only what changed; the page carries those updates to the others, and their updates come back
//! through [`Session::collab_receive`], which rebuilds just the blocks they touched.
//!
//! What the mirror keeps (Loro root containers):
//! - Block lists, each a movable list of block ids: `body` (the document), `s:{n}` (part `n`:
//!   a header, footer, note, comment's text or text box) and `l:{id}` (a table cell).
//! - `t:{id}`: a paragraph's text, merged character by character, with its runs' formatting as a
//!   mark (`f`, the run's `CharProps` as JSON) that sticks to the text it formats.
//! - `p:{id}`: `k` (`p` paragraph, `t` table), and `p` (the paragraph's own properties, mark,
//!   section and inline objects, as JSON) or `j` (a table's structure as JSON: its rows and cells
//!   with their properties, each cell naming its block list instead of holding its blocks). Text
//!   in cells merges like any other; two people changing a table's structure at once (adding a
//!   row, merging cells) keep the last change.
//! - `parts`: each part's kind and settings (its blocks are in `s:{n}`).
//! - `meta`: the document's settings, styles, numbering, section… one JSON per field.
//! - `comments`, `media` and `pass`: one entry each.
//!
//! Block ids are unique in the whole document, so a caret is told to the others as the id of its
//! paragraph (in the body, a cell, a header…) and a character offset.
//!
//! Undo and Redo in a shared document undo this person's own changes ([`UndoManager`]), adjusted
//! to what the others did since. The caret stays where it was in the text when others edit
//! (Loro cursors).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use loro::cursor::{Cursor, Side};
use loro::event::Diff;
use loro::{
    ContainerID, ExpandType, ExportMode, Frontiers, LoroDoc, LoroMap, LoroMovableList, LoroText, LoroValue, StyleConfig, StyleConfigMap, TextDelta,
    UndoManager, ValueOrContainer, VersionVector,
};
use serde_json::{Value as Json, json};
use wordcraft_doc::{Block, Cell, Document, Paragraph, Part, Path, Pos, Row, StoryRef, Table, para_block};

use crate::{CmdError, Session};

/// The shared document's format: a document shared by another format starts over from the file.
pub const FORMAT: i64 = 2;

const BODY: &str = "body";
const META: &str = "meta";
const PARTS: &str = "parts";
const COMMENTS: &str = "comments";
const MEDIA: &str = "media";
const PASS: &str = "pass";
/// The text mark that holds a run's formatting.
const FMT: &str = "f";
/// The placeholder character an inline object takes in a paragraph's text.
const OBJ: char = '\u{FFFC}';

type R<T> = std::result::Result<T, CmdError>;

fn err(e: impl std::fmt::Display) -> CmdError {
    CmdError::Failed(format!("co-editing: {e}"))
}

/// The document fields kept in `meta`, as (field, key).
macro_rules! meta_fields {
    ($m:ident) => {
        $m!(last_section, "lastSection");
        $m!(styles, "styles");
        $m!(numbering, "numbering");
        $m!(revisions, "revisions");
        $m!(settings, "settings");
        $m!(core, "core");
        $m!(sources, "sources");
        $m!(custom_props, "customProps");
        $m!(footnote_separators, "footnoteSeparators");
        $m!(endnote_separators, "endnoteSeparators");
        $m!(glossary, "glossary");
        $m!(fit_state, "fitState");
    };
}

/// Someone else editing the document: where their caret is, for drawing it.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    /// The id of the paragraph their caret is in (in the body, a table cell, a header…), and where
    /// in it (characters), as their presence says: [`Session::collab_peer_pos`] finds it here.
    pub block: String,
    pub off: usize,
}

/// Where a caret was, kept across someone else's changes.
#[derive(Clone, Debug)]
pub(crate) struct Anchor {
    story: StoryRef,
    /// The paragraph's id, and the id of the top-level block it is in.
    id: String,
    top: String,
    index: usize,
    off: usize,
    cursor: Option<Cursor>,
}

/// A block's place in the mirror: its id and, for a table, each cell's block list.
#[derive(Clone, Debug, Default, PartialEq)]
struct Node {
    /// Empty for a paragraph the mirror doesn't have (one added here so a story or cell doesn't
    /// end empty or on a table): the next capture puts it in.
    id: String,
    cells: Vec<Vec<CellNode>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct CellNode {
    /// The id of the cell's block list (`l:{list}`).
    list: String,
    blocks: Vec<Node>,
}

/// What a block list must hold when read back, as [`Document::ensure_nonempty`] wants it.
#[derive(Clone, Copy)]
enum Fix {
    /// A paragraph somewhere, and not ending on a table.
    Story,
    /// Something, and not ending on a table.
    Cell,
    /// Something.
    Part,
}

pub struct Collab {
    doc: LoroDoc,
    /// The body's blocks, in the document's order.
    body: Vec<Node>,
    /// Each part's blocks.
    parts: BTreeMap<u32, Vec<Node>>,
    /// The document the mirror matches.
    base: Document,
    /// The session revision last captured: an unchanged revision is an unchanged document.
    seen_rev: Option<u64>,
    outbox: Vec<Vec<u8>>,
    sent: VersionVector,
    next_id: u64,
    undo: Arc<Mutex<UndoManager>>,
}

impl std::fmt::Debug for Collab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collab").field("peer", &self.doc.peer_id()).field("blocks", &self.body.len()).field("parts", &self.parts.len()).finish()
    }
}

fn new_doc() -> LoroDoc {
    let doc = LoroDoc::new();
    let mut styles = StyleConfigMap::new();
    styles.insert(FMT.into(), StyleConfig { expand: ExpandType::After });
    doc.config_text_style(styles);
    doc
}

fn new_undo(doc: &LoroDoc) -> Arc<Mutex<UndoManager>> {
    let mut u = UndoManager::new(doc);
    u.set_max_undo_steps(100);
    Arc::new(Mutex::new(u))
}

fn text_of(doc: &LoroDoc, id: &str) -> LoroText {
    doc.get_text(format!("t:{id}").as_str())
}

fn block_map(doc: &LoroDoc, id: &str) -> LoroMap {
    doc.get_map(format!("p:{id}").as_str())
}

fn cell_list(id: &str) -> String {
    format!("l:{id}")
}

fn story_list(k: u32) -> String {
    format!("s:{k}")
}

fn as_string(v: &ValueOrContainer) -> Option<String> {
    match v {
        ValueOrContainer::Value(LoroValue::String(s)) => Some(s.as_str().to_owned()),
        _ => None,
    }
}

fn get_str(map: &LoroMap, key: &str) -> Option<String> {
    map.get(key).as_ref().and_then(as_string)
}

/// Writes `value` under `key` unless it is already there; `None` removes the key.
fn put(map: &LoroMap, key: &str, value: Option<String>) -> R<()> {
    if get_str(map, key) == value {
        return Ok(());
    }
    match value {
        Some(v) => map.insert(key, v).map_err(err),
        None => {
            if map.get(key).is_some() {
                map.delete(key).map_err(err)?;
            }
            Ok(())
        }
    }
}

fn movable_strings(list: &LoroMovableList) -> Vec<String> {
    list.to_vec().into_iter().filter_map(|v| if let LoroValue::String(s) = v { Some(s.as_str().to_owned()) } else { None }).collect()
}

fn clear(list: &LoroMovableList) -> R<()> {
    let n = list.len();
    if n > 0 {
        list.delete(0, n).map_err(err)?;
    }
    Ok(())
}

fn to_json<T: serde::Serialize>(v: &T) -> R<String> {
    serde_json::to_string(v).map_err(err)
}

/// A paragraph's own part (its properties, mark, section and objects) as JSON: no text or runs.
fn shell_of(p: &Paragraph) -> R<String> {
    let mut v = serde_json::to_value(p).map_err(err)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("text");
        o.remove("runs");
    }
    serde_json::to_string(&v).map_err(err)
}

/// A table's structure as JSON: each cell names its block list (`list`) instead of its blocks.
fn table_shell(t: &Table, cells: &[Vec<CellNode>]) -> R<String> {
    let bare = Table {
        props: t.props.clone(),
        grid: t.grid.clone(),
        controls: t.controls.clone(),
        rows: t
            .rows
            .iter()
            .map(|r| Row {
                props: r.props.clone(),
                controls: r.controls.clone(),
                cells: r.cells.iter().map(|c| Cell { props: c.props.clone(), blocks: Vec::new(), controls: c.controls.clone() }).collect(),
            })
            .collect(),
    };
    let mut v = serde_json::to_value(&bare).map_err(err)?;
    if let Some(rows) = v.get_mut("rows").and_then(Json::as_array_mut) {
        for (row, rn) in rows.iter_mut().zip(cells) {
            let Some(cs) = row.get_mut("cells").and_then(Json::as_array_mut) else { continue };
            for (cell, cn) in cs.iter_mut().zip(rn) {
                if let Some(o) = cell.as_object_mut() {
                    o.remove("blocks");
                    o.insert("list".into(), Json::String(cn.list.clone()));
                }
            }
        }
    }
    serde_json::to_string(&v).map_err(err)
}

/// A part's kind and settings as JSON: no blocks.
fn part_shell(p: &Part) -> R<String> {
    let mut v = serde_json::to_value(Part { kind: p.kind, blocks: Vec::new(), body: p.body }).map_err(err)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("blocks");
    }
    serde_json::to_string(&v).map_err(err)
}

/// A run's formatting as the mark holds it; `None` for the paragraph's plain text.
fn fmt_of(props: &wordcraft_doc::props::CharProps) -> Option<String> {
    if props.is_empty() { None } else { serde_json::to_string(props).ok() }
}

/// Byte offset → character offset in `s`.
fn chars_at(s: &str, byte: usize) -> usize {
    s.get(..byte.min(s.len())).map_or_else(|| s.chars().count(), |p| p.chars().count())
}

/// Character offset → byte offset in `s`.
fn bytes_at(s: &str, ch: usize) -> usize {
    s.char_indices().nth(ch).map_or(s.len(), |(b, _)| b)
}

/// The text and its formatted pieces (bytes, mark value) as the mirror has them.
fn text_runs(t: &LoroText) -> (String, Vec<(usize, Option<String>)>) {
    let mut text = String::new();
    let mut runs: Vec<(usize, Option<String>)> = Vec::new();
    for d in t.to_delta() {
        let TextDelta::Insert { insert, attributes } = d else { continue };
        let f = attributes.and_then(|a| match a.get(FMT) {
            Some(LoroValue::String(s)) => Some(s.as_str().to_owned()),
            _ => None,
        });
        match runs.last_mut() {
            Some(last) if last.1 == f => last.0 += insert.len(),
            _ => runs.push((insert.len(), f)),
        }
        text.push_str(&insert);
    }
    (text, runs)
}

/// The paragraph `id` as the mirror has it.
fn para_from(doc: &LoroDoc, id: &str) -> Option<Paragraph> {
    let shell: Json = get_str(&block_map(doc, id), "p").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
    let (text, runs) = text_runs(&text_of(doc, id));
    let mut v = if shell.is_object() { shell } else { json!({}) };
    let runs: Vec<Json> = runs
        .into_iter()
        .filter(|(len, _)| *len > 0)
        .map(|(len, f)| match f.and_then(|f| serde_json::from_str::<Json>(&f).ok()) {
            Some(props) => json!({"len": len, "props": props}),
            None => json!({"len": len}),
        })
        .collect();
    v["text"] = Json::String(text);
    v["runs"] = Json::Array(runs);
    let mut p: Paragraph = serde_json::from_value(v).ok()?;
    fix_objects(&mut p);
    Some(p)
}

/// Two people inserting and deleting pictures at once can leave the text with more object
/// placeholders than objects (or fewer): the extra placeholders go, the extra objects too.
fn fix_objects(p: &mut Paragraph) {
    let marks = p.text.chars().filter(|c| *c == OBJ).count();
    if marks == p.objects.len() {
        return;
    }
    if marks < p.objects.len() {
        p.objects.truncate(marks);
        return;
    }
    // Drop placeholders past the objects, shrinking the runs they were in.
    let keep = p.objects.len();
    let mut seen = 0;
    let mut text = String::with_capacity(p.text.len());
    let mut cut: Vec<usize> = Vec::new();
    for (b, c) in p.text.char_indices() {
        if c == OBJ {
            seen += 1;
            if seen > keep {
                cut.push(b);
                continue;
            }
        }
        text.push(c);
    }
    let mut start = 0;
    for run in &mut p.runs {
        let end = start + run.len;
        let removed = cut.iter().filter(|b| **b >= start && **b < end).count() * OBJ.len_utf8();
        start = end;
        run.len -= removed.min(run.len);
    }
    p.runs.retain(|r| r.len > 0);
    p.text = text;
}

fn blocks_same(a: &[Arc<Block>], b: &[Arc<Block>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| Arc::ptr_eq(x, y))
}

fn cells_same(a: &Cell, b: &Cell) -> bool {
    a.props == b.props && a.controls == b.controls && blocks_same(&a.blocks, &b.blocks)
}

fn rows_same(a: &Row, b: &Row) -> bool {
    a.props == b.props && a.controls == b.controls && a.cells.len() == b.cells.len() && a.cells.iter().zip(&b.cells).all(|(x, y)| cells_same(x, y))
}

/// How alike two blocks are, for pairing an edited block with what it became: shared text at the
/// start and end of two paragraphs, 1 for two tables, 0 otherwise.
fn likeness(a: &Block, b: &Block) -> usize {
    match (a, b) {
        (Block::Para(x), Block::Para(y)) => {
            let pre = x.text.chars().zip(y.text.chars()).take_while(|(p, q)| p == q).count();
            let suf = x.text.chars().rev().zip(y.text.chars().rev()).take_while(|(p, q)| p == q).count();
            1 + (pre + suf).min(x.text.chars().count().max(y.text.chars().count()))
        }
        (Block::Table(_), Block::Table(_)) => 1,
        _ => 0,
    }
}

fn row_likeness(a: &Row, b: &Row) -> usize {
    1 + a.cells.iter().zip(&b.cells).filter(|(x, y)| blocks_same(&x.blocks, &y.blocks)).count()
}

fn cell_likeness(a: &Cell, b: &Cell) -> usize {
    1 + a.blocks.iter().zip(&b.blocks).filter(|(x, y)| Arc::ptr_eq(x, y)).count()
}

/// Pairs `m` old items with `n` new ones in order, best total likeness first (`None`: deleted or
/// inserted; a likeness of 0 never pairs). Small regions are aligned exactly; large ones position
/// by position.
fn pair_by(m: usize, n: usize, like: impl Fn(usize, usize) -> usize) -> Vec<(Option<usize>, Option<usize>)> {
    if m.saturating_mul(n) > 40_000 {
        return (0..m.max(n)).map(|i| ((i < m).then_some(i), (i < n).then_some(i))).collect();
    }
    let likes: Vec<Vec<usize>> = (0..m).map(|i| (0..n).map(|j| like(i, j)).collect()).collect();
    // score[i][j]: best total likeness of old[i..] with new[j..].
    let mut score = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            let l = likes[i][j];
            let take = if l > 0 { l + score[i + 1][j + 1] } else { 0 };
            score[i][j] = take.max(score[i + 1][j]).max(score[i][j + 1]);
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n {
            let l = likes[i][j];
            if l > 0 && score[i][j] == l + score[i + 1][j + 1] {
                out.push((Some(i), Some(j)));
                i += 1;
                j += 1;
                continue;
            }
            if score[i][j] == score[i + 1][j] {
                out.push((Some(i), None));
                i += 1;
            } else {
                out.push((None, Some(j)));
                j += 1;
            }
        } else if i < m {
            out.push((Some(i), None));
            i += 1;
        } else {
            out.push((None, Some(j)));
            j += 1;
        }
    }
    out
}

/// [`pair_by`] after the unchanged start and end (`same`), which pair as they are.
fn align(m: usize, n: usize, same: impl Fn(usize, usize) -> bool, like: impl Fn(usize, usize) -> usize) -> Vec<(Option<usize>, Option<usize>)> {
    let pre = (0..m.min(n)).take_while(|&i| same(i, i)).count();
    let suf = (0..(m - pre).min(n - pre)).take_while(|&k| same(m - 1 - k, n - 1 - k)).count();
    let mut out: Vec<(Option<usize>, Option<usize>)> = (0..pre).map(|i| (Some(i), Some(i))).collect();
    for (a, b) in pair_by(m - pre - suf, n - pre - suf, |i, j| like(pre + i, pre + j)) {
        out.push((a.map(|x| x + pre), b.map(|x| x + pre)));
    }
    out.extend((0..suf).map(|k| (Some(m - suf + k), Some(n - suf + k))));
    out
}

/// Whether someone else's update touched `n` or anything inside it.
fn touched(n: &Node, c: &Changes) -> bool {
    c.blocks.contains(&n.id) || n.cells.iter().flatten().any(|cn| c.cells.contains(&cn.list) || cn.blocks.iter().any(|b| touched(b, c)))
}

/// The node at `path` (`[block]`, or `[block, row, cell, block…]` inside tables).
fn node_at<'a>(nodes: &'a [Node], path: &[u32]) -> Option<&'a Node> {
    let (first, rest) = path.split_first()?;
    let n = nodes.get(*first as usize)?;
    match rest {
        [] => Some(n),
        [r, c, more @ ..] => node_at(&n.cells.get(*r as usize)?.get(*c as usize)?.blocks, more),
        _ => None,
    }
}

/// Fills `path` with where the block `id` is among `nodes`.
fn path_of(nodes: &[Node], id: &str, path: &mut Vec<u32>) -> bool {
    for (i, n) in nodes.iter().enumerate() {
        path.push(i as u32);
        if n.id == id {
            return true;
        }
        for (r, row) in n.cells.iter().enumerate() {
            for (ci, cell) in row.iter().enumerate() {
                path.push(r as u32);
                path.push(ci as u32);
                if path_of(&cell.blocks, id, path) {
                    return true;
                }
                path.pop();
                path.pop();
            }
        }
        path.pop();
    }
    false
}

impl Collab {
    fn empty(doc: LoroDoc, base: Document) -> Collab {
        let undo = new_undo(&doc);
        Collab { sent: doc.oplog_vv(), doc, body: Vec::new(), parts: BTreeMap::new(), base, seen_rev: None, outbox: Vec::new(), next_id: 0, undo }
    }

    /// Starts a shared document from `doc`: the first person to open the file.
    pub fn seed(doc: &Document) -> R<Collab> {
        let mut c = Collab::empty(new_doc(), Document::default());
        c.doc.get_map("format").insert("version", FORMAT).map_err(err)?;
        let list = c.doc.get_movable_list(BODY);
        c.body = c.sync_list(&list, Vec::new(), &[], &doc.body)?;
        c.write_doc_fields(None, doc)?;
        c.doc.commit();
        c.sent = c.doc.oplog_vv();
        c.base = doc.clone();
        // The seed is where history starts: nothing before it is undone.
        c.undo = new_undo(&c.doc);
        Ok(c)
    }

    /// Joins a shared document from its snapshot: `local` is the same file opened here; the
    /// document returned has the shared content in its place.
    pub fn join(snapshot: &[u8], local: &Document) -> R<(Collab, Document)> {
        let doc = new_doc();
        doc.import(snapshot).map_err(err)?;
        let version = match doc.get_map("format").get("version") {
            Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
            _ => 0,
        };
        if version != FORMAT {
            return Err(CmdError::Failed(format!(
                "A edição compartilhada foi aberta por outra versão do editor (formato {version}, esta é {FORMAT}). Feche e abra de novo o documento em todas as janelas."
            )));
        }
        let mut c = Collab::empty(doc, local.clone());
        let out = c.rebuild(&Changes { all: true, ..Changes::default() })?;
        c.base = out.clone();
        Ok((c, out))
    }

    pub fn snapshot(&self) -> R<Vec<u8>> {
        self.doc.export(ExportMode::Snapshot).map_err(err)
    }

    /// The updates for the others made since the last call (typing commits each key; they go
    /// out together, as one update, when the page asks).
    pub fn take_outbox(&mut self) -> Vec<Vec<u8>> {
        if let Err(e) = self.flush() {
            log::warn!("{e}");
        }
        std::mem::take(&mut self.outbox)
    }

    fn flush(&mut self) -> R<()> {
        self.doc.commit();
        let now = self.doc.oplog_vv();
        if now != self.sent {
            let bytes = self.doc.export(ExportMode::updates(&self.sent)).map_err(err)?;
            self.outbox.push(bytes);
            self.sent = now;
        }
        Ok(())
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("{:x}.{}", self.doc.peer_id(), self.next_id)
    }

    /// Brings the block list `list` (whose blocks were `old`, placed as `nodes`) to `new`: the
    /// blocks that changed are written, the new ones added, the gone ones removed. The new nodes.
    fn sync_list(&mut self, list: &LoroMovableList, nodes: Vec<Node>, old: &[Arc<Block>], new: &[Arc<Block>]) -> R<Vec<Node>> {
        if nodes.len() != old.len() {
            // Out of step (it shouldn't happen): the list is written again.
            log::warn!("co-editing: a block list was out of step; written again");
            clear(list)?;
            let mut out = Vec::with_capacity(new.len());
            for (i, b) in new.iter().enumerate() {
                out.push(self.insert_block(list, i, b)?);
            }
            return Ok(out);
        }
        if blocks_same(old, new) {
            return Ok(nodes);
        }
        let pairs = align(old.len(), new.len(), |i, j| Arc::ptr_eq(&old[i], &new[j]), |i, j| likeness(&old[i], &new[j]));
        let mut slots: Vec<Option<Node>> = nodes.into_iter().map(Some).collect();
        let mut out = Vec::with_capacity(new.len());
        // Where the next block is in the mirror's list (blocks it doesn't have take no place).
        let mut at = 0usize;
        for (o, n) in pairs {
            match (o, n) {
                (Some(oi), Some(ni)) => {
                    let node = slots[oi].take().unwrap_or_default();
                    if node.id.is_empty() {
                        out.push(self.insert_block(list, at, &new[ni])?);
                    } else if Arc::ptr_eq(&old[oi], &new[ni]) {
                        out.push(node);
                    } else {
                        out.push(self.write_block(node, Some(&old[oi]), &new[ni])?);
                    }
                    at += 1;
                }
                (Some(oi), None) => {
                    if slots[oi].as_ref().is_some_and(|n| !n.id.is_empty()) {
                        list.delete(at, 1).map_err(err)?;
                    }
                }
                (None, Some(ni)) => {
                    out.push(self.insert_block(list, at, &new[ni])?);
                    at += 1;
                }
                (None, None) => {}
            }
        }
        Ok(out)
    }

    /// Adds `b` to `list` at `at`, under a new id.
    fn insert_block(&mut self, list: &LoroMovableList, at: usize, b: &Block) -> R<Node> {
        let id = self.new_id();
        list.insert(at, id.as_str()).map_err(err)?;
        self.write_block(Node { id, cells: Vec::new() }, None, b)
    }

    /// Writes the block `node` now holds (`new`; it held `old`): a paragraph's text merged into
    /// what the mirror has, a table's cells each in their own list.
    fn write_block(&mut self, node: Node, old: Option<&Block>, new: &Block) -> R<Node> {
        let m = block_map(&self.doc, &node.id);
        match new {
            Block::Para(p) => {
                put(&m, "k", Some("p".into()))?;
                put(&m, "p", Some(shell_of(p)?))?;
                self.write_text(&node.id, p)?;
                Ok(Node { id: node.id, cells: Vec::new() })
            }
            Block::Table(t) => {
                put(&m, "k", Some("t".into()))?;
                let cells = self.write_cells(node.cells, old.and_then(Block::as_table), t)?;
                put(&m, "j", Some(table_shell(t, &cells)?))?;
                Ok(Node { id: node.id, cells })
            }
        }
    }

    /// A table's cells: each row paired with what it was, each cell's blocks synced in its list.
    fn write_cells(&mut self, nodes: Vec<Vec<CellNode>>, old: Option<&Table>, new: &Table) -> R<Vec<Vec<CellNode>>> {
        let (orows, nodes): (&[Row], Vec<Vec<CellNode>>) = match old {
            Some(t) if t.rows.len() == nodes.len() && t.rows.iter().zip(&nodes).all(|(r, n)| r.cells.len() == n.len()) => (&t.rows[..], nodes),
            _ => (&[], Vec::new()),
        };
        let pairs = align(orows.len(), new.rows.len(), |i, j| rows_same(&orows[i], &new.rows[j]), |i, j| row_likeness(&orows[i], &new.rows[j]));
        let mut slots: Vec<Option<Vec<CellNode>>> = nodes.into_iter().map(Some).collect();
        let mut out = Vec::with_capacity(new.rows.len());
        for (o, n) in pairs {
            let Some(ni) = n else { continue };
            let row = &new.rows[ni];
            match o {
                Some(oi) => {
                    let rn = slots[oi].take().unwrap_or_default();
                    if rows_same(&orows[oi], row) {
                        out.push(rn);
                    } else {
                        out.push(self.write_row(rn, Some(&orows[oi]), row)?);
                    }
                }
                None => out.push(self.write_row(Vec::new(), None, row)?),
            }
        }
        Ok(out)
    }

    fn write_row(&mut self, nodes: Vec<CellNode>, old: Option<&Row>, new: &Row) -> R<Vec<CellNode>> {
        let none: &[Arc<Block>] = &[];
        let (ocells, nodes): (&[Cell], Vec<CellNode>) = match old {
            Some(r) if r.cells.len() == nodes.len() => (&r.cells[..], nodes),
            _ => (&[], Vec::new()),
        };
        let pairs = align(ocells.len(), new.cells.len(), |i, j| cells_same(&ocells[i], &new.cells[j]), |i, j| cell_likeness(&ocells[i], &new.cells[j]));
        let mut slots: Vec<Option<CellNode>> = nodes.into_iter().map(Some).collect();
        let mut out = Vec::with_capacity(new.cells.len());
        for (o, n) in pairs {
            let Some(ni) = n else { continue };
            let (cn, old_blocks) = match o {
                Some(oi) => (slots[oi].take().unwrap_or_default(), &ocells[oi].blocks[..]),
                None => (CellNode::default(), none),
            };
            // A cell new to the mirror gets a list of its own.
            let (list_id, nodes, old_blocks) = if cn.list.is_empty() { (self.new_id(), Vec::new(), none) } else { (cn.list, cn.blocks, old_blocks) };
            let list = self.doc.get_movable_list(cell_list(&list_id).as_str());
            let blocks = self.sync_list(&list, nodes, old_blocks, &new.cells[ni].blocks)?;
            out.push(CellNode { list: list_id, blocks });
        }
        Ok(out)
    }

    /// Brings the mirror's text of `id` to the paragraph's: the changed middle is replaced, then
    /// the formatting is marked where it differs.
    fn write_text(&self, id: &str, p: &Paragraph) -> R<()> {
        let t = text_of(&self.doc, id);
        let cur = t.to_string();
        if cur != p.text {
            let a: Vec<(usize, char)> = cur.char_indices().collect();
            let b: Vec<(usize, char)> = p.text.char_indices().collect();
            let pre = a.iter().zip(&b).take_while(|(x, y)| x.1 == y.1).count();
            let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x.1 == y.1).count();
            let del = a.len() - pre - suf;
            if del > 0 {
                t.delete(pre, del).map_err(err)?;
            }
            let ins: String = b[pre..b.len() - suf].iter().map(|(_, c)| *c).collect();
            if !ins.is_empty() {
                t.insert(pre, &ins).map_err(err)?;
            }
        }
        // Formatting, by bytes: what the paragraph says against what the mirror has.
        let (_, have) = text_runs(&t);
        let mut want: Vec<(usize, Option<String>)> = Vec::new();
        for r in &p.runs {
            want.push((r.len, fmt_of(&r.props)));
        }
        let total: usize = want.iter().map(|w| w.0).sum();
        if total < p.text.len() {
            want.push((p.text.len() - total, None));
        }
        let cuts = |runs: &[(usize, Option<String>)]| -> Vec<(usize, usize, Option<String>)> {
            let mut at = 0;
            runs.iter()
                .map(|(len, f)| {
                    let s = at;
                    at += len;
                    (s, at, f.clone())
                })
                .collect()
        };
        let (want, have) = (cuts(&want), cuts(&have));
        let mut points: BTreeSet<usize> = BTreeSet::new();
        for (s, e, _) in want.iter().chain(&have) {
            points.insert(*s);
            points.insert(*e);
        }
        let points: Vec<usize> = points.into_iter().filter(|x| *x <= p.text.len()).collect();
        let value_at = |runs: &[(usize, usize, Option<String>)], at: usize| runs.iter().find(|(s, e, _)| *s <= at && at < *e).and_then(|r| r.2.clone());
        for w in points.windows(2) {
            let (s, e) = (w[0], w[1]);
            if s >= e {
                continue;
            }
            let (target, now) = (value_at(&want, s), value_at(&have, s));
            if target == now {
                continue;
            }
            let (cs, ce) = (chars_at(&p.text, s), chars_at(&p.text, e));
            match target {
                Some(v) => t.mark(cs..ce, FMT, v).map_err(err)?,
                None => t.unmark(cs..ce, FMT).map_err(err)?,
            }
        }
        Ok(())
    }

    /// The document fields, parts, comments and media that differ from `before` (all of them
    /// without one).
    fn write_doc_fields(&mut self, before: Option<&Document>, after: &Document) -> R<()> {
        let meta = self.doc.get_map(META);
        macro_rules! put_field {
            ($f:ident, $k:literal) => {
                if before.is_none_or(|b| b.$f != after.$f) {
                    put(&meta, $k, Some(to_json(&after.$f)?))?;
                }
            };
        }
        meta_fields!(put_field);
        let none: &[Arc<Block>] = &[];
        let parts = self.doc.get_map(PARTS);
        let keys: BTreeSet<u32> = after.parts.keys().chain(before.map(|b| b.parts.keys()).into_iter().flatten()).copied().collect();
        for k in keys {
            let (old, new) = (before.and_then(|b| b.parts.get(&k)), after.parts.get(&k));
            let same = match (old, new) {
                (Some(a), Some(b)) => a.kind == b.kind && a.body == b.body && blocks_same(&a.blocks, &b.blocks),
                (None, None) => true,
                _ => false,
            };
            if before.is_some() && same {
                continue;
            }
            let list = self.doc.get_movable_list(story_list(k).as_str());
            match new {
                Some(p) => {
                    put(&parts, &k.to_string(), Some(part_shell(p)?))?;
                    let (nodes, old_blocks) = match (self.parts.remove(&k), old) {
                        (Some(n), Some(o)) => (n, &o.blocks[..]),
                        // A part new here: whatever an earlier part of that number left goes.
                        _ => {
                            clear(&list)?;
                            (Vec::new(), none)
                        }
                    };
                    let nodes = self.sync_list(&list, nodes, old_blocks, &p.blocks)?;
                    self.parts.insert(k, nodes);
                }
                None => {
                    put(&parts, &k.to_string(), None)?;
                    clear(&list)?;
                    self.parts.remove(&k);
                }
            }
        }
        let comments = self.doc.get_map(COMMENTS);
        let keys: BTreeSet<u32> = after.comments.keys().chain(before.map(|b| b.comments.keys()).into_iter().flatten()).copied().collect();
        for k in keys {
            let (old, new) = (before.and_then(|b| b.comments.get(&k)), after.comments.get(&k));
            if before.is_none() || old != new {
                put(&comments, &k.to_string(), new.map(to_json).transpose()?)?;
            }
        }
        for (name, now, was) in [(MEDIA, &after.media, before.map(|b| &b.media)), (PASS, &after.passthrough, before.map(|b| &b.passthrough))] {
            let map = self.doc.get_map(name);
            let keys: BTreeSet<&String> = now.keys().chain(was.map(|w| w.keys()).into_iter().flatten()).collect();
            for k in keys {
                let (old, new) = (was.and_then(|w| w.get(k)), now.get(k));
                let same = match (old, new) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
                    (None, None) => true,
                    _ => false,
                };
                if was.is_some() && same {
                    continue;
                }
                match new {
                    Some(bytes) => map.insert(k, (**bytes).clone()).map_err(err)?,
                    None => {
                        if map.get(k).is_some() {
                            map.delete(k).map_err(err)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes what changed between the document the mirror matched and `after`, and queues the
    /// update for the others.
    pub fn capture(&mut self, after: &Document, rev: u64) -> R<()> {
        if self.seen_rev == Some(rev) {
            return Ok(());
        }
        self.seen_rev = Some(rev);
        let base = std::mem::take(&mut self.base);
        let nodes = std::mem::take(&mut self.body);
        let list = self.doc.get_movable_list(BODY);
        let done = self.sync_list(&list, nodes, &base.body, &after.body).and_then(|body| {
            self.body = body;
            self.write_doc_fields(Some(&base), after)
        });
        // Committed now (one undo step per command); exported for the others with the next
        // `take_outbox`.
        self.doc.commit();
        self.base = after.clone();
        done
    }

    /// Takes in an update from someone else: the document with their changes, or `None` when it
    /// changed nothing here.
    pub fn receive(&mut self, update: &[u8]) -> R<Option<Document>> {
        // This person's changes not yet handed out go first: what comes in counts as sent.
        self.flush()?;
        let before = self.doc.state_frontiers();
        self.doc.import(update).map_err(err)?;
        let after = self.doc.state_frontiers();
        self.sent = self.doc.oplog_vv();
        self.apply(&before, &after)
    }

    pub fn can_undo(&self, redo: bool) -> bool {
        let u = self.undo.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if redo { u.can_redo() } else { u.can_undo() }
    }

    /// Undoes (or redoes) this person's last change, queued for the others: the document after
    /// it, or `None` when there was nothing to undo.
    pub fn undo(&mut self, redo: bool) -> R<Option<Document>> {
        let before = self.doc.state_frontiers();
        let done = {
            let mut u = self.undo.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if redo { u.redo() } else { u.undo() }
        }
        .map_err(err)?;
        if !done {
            return Ok(None);
        }
        self.doc.commit();
        let after = self.doc.state_frontiers();
        let out = self.apply(&before, &after)?;
        self.flush()?;
        Ok(out)
    }

    fn apply(&mut self, before: &Frontiers, after: &Frontiers) -> R<Option<Document>> {
        if before == after {
            return Ok(None);
        }
        let batch = self.doc.diff(before, after).map_err(err)?;
        let mut c = Changes::default();
        for (cid, diff) in batch.iter() {
            let ContainerID::Root { name, .. } = cid else { continue };
            let name: &str = name.as_ref();
            let keys = || -> BTreeSet<String> {
                match diff {
                    Diff::Map(d) => d.updated.keys().map(|k| k.to_string()).collect(),
                    _ => BTreeSet::new(),
                }
            };
            match name {
                BODY => c.body = true,
                META => c.meta.extend(keys()),
                PARTS => c.parts.extend(keys()),
                COMMENTS => c.comments.extend(keys()),
                MEDIA => c.media.extend(keys()),
                PASS => c.pass.extend(keys()),
                _ => {
                    if let Some(id) = name.strip_prefix("t:").or_else(|| name.strip_prefix("p:")) {
                        c.blocks.insert(id.to_string());
                    } else if let Some(id) = name.strip_prefix("l:") {
                        c.cells.insert(id.to_string());
                    } else if let Some(k) = name.strip_prefix("s:").and_then(|k| k.parse::<u32>().ok()) {
                        c.stories.insert(k);
                    }
                }
            }
        }
        let out = self.rebuild(&c)?;
        self.base = out.clone();
        self.seen_rev = None;
        Ok(Some(out))
    }

    /// The blocks of the list `name` as the mirror has them; `old` (the blocks and their nodes)
    /// are kept where nothing in them changed.
    fn read_list(&self, name: &str, old: Option<(&[Arc<Block>], &[Node])>, c: &Changes, fix: Fix) -> (Vec<Arc<Block>>, Vec<Node>) {
        let ids = movable_strings(&self.doc.get_movable_list(name));
        let known: HashMap<&str, (&Arc<Block>, &Node)> =
            old.map(|(b, n)| b.iter().zip(n).filter(|(_, n)| !n.id.is_empty()).map(|(b, n)| (n.id.as_str(), (b, n))).collect()).unwrap_or_default();
        let mut blocks = Vec::with_capacity(ids.len() + 1);
        let mut nodes = Vec::with_capacity(ids.len() + 1);
        for id in &ids {
            if let Some((b, n)) = self.read_block(id, known.get(id.as_str()).copied(), c) {
                blocks.push(b);
                nodes.push(n);
            }
        }
        let ends_on_table = matches!(blocks.last().map(|b| &**b), Some(Block::Table(_)));
        let needs = match fix {
            Fix::Story => ends_on_table || !blocks.iter().any(|b| matches!(**b, Block::Para(_))),
            Fix::Cell => ends_on_table || blocks.is_empty(),
            Fix::Part => blocks.is_empty(),
        };
        if needs {
            blocks.push(para_block(Paragraph::new()));
            nodes.push(Node::default());
        }
        (blocks, nodes)
    }

    /// The block `id` as the mirror has it (`old`, as it was, when nothing in it changed).
    fn read_block(&self, id: &str, old: Option<(&Arc<Block>, &Node)>, c: &Changes) -> Option<(Arc<Block>, Node)> {
        if let Some((b, n)) = old
            && !c.all
            && !touched(n, c)
        {
            return Some((b.clone(), n.clone()));
        }
        let m = block_map(&self.doc, id);
        match get_str(&m, "k").as_deref() {
            Some("t") => {
                let old_t = old.and_then(|(b, n)| b.as_table().map(|t| (t, n)));
                let (t, cells) = self.read_table(&m, old_t, c)?;
                Some((Arc::new(Block::Table(t)), Node { id: id.to_string(), cells }))
            }
            _ => para_from(&self.doc, id).map(|p| (Arc::new(Block::Para(p)), Node { id: id.to_string(), cells: Vec::new() })),
        }
    }

    fn read_table(&self, m: &LoroMap, old: Option<(&Table, &Node)>, c: &Changes) -> Option<(Table, Vec<Vec<CellNode>>)> {
        let mut v: Json = serde_json::from_str(&get_str(m, "j")?).ok()?;
        let mut lists: Vec<Vec<String>> = Vec::new();
        if let Some(rows) = v.get_mut("rows").and_then(Json::as_array_mut) {
            for row in rows {
                let mut l = Vec::new();
                if let Some(cells) = row.get_mut("cells").and_then(Json::as_array_mut) {
                    for cell in cells {
                        let Some(o) = cell.as_object_mut() else {
                            l.push(String::new());
                            continue;
                        };
                        l.push(o.remove("list").and_then(|x| x.as_str().map(str::to_owned)).unwrap_or_default());
                        o.insert("blocks".into(), json!([]));
                    }
                }
                lists.push(l);
            }
        }
        let mut t: Table = serde_json::from_value(v).ok()?;
        let mut prev: HashMap<&str, (&Cell, &CellNode)> = HashMap::new();
        if let Some((ot, on)) = old {
            for (row, rn) in ot.rows.iter().zip(&on.cells) {
                for (cell, cn) in row.cells.iter().zip(rn) {
                    if !cn.list.is_empty() {
                        prev.insert(cn.list.as_str(), (cell, cn));
                    }
                }
            }
        }
        let mut nodes = Vec::with_capacity(t.rows.len());
        for (r, row) in t.rows.iter_mut().enumerate() {
            let mut rn = Vec::with_capacity(row.cells.len());
            for (ci, cell) in row.cells.iter_mut().enumerate() {
                let list = lists.get(r).and_then(|l| l.get(ci)).cloned().unwrap_or_default();
                let old = prev.get(list.as_str()).map(|(pc, pn)| (&pc.blocks[..], &pn.blocks[..]));
                let (blocks, bn) = self.read_list(&cell_list(&list), old, c, Fix::Cell);
                cell.blocks = blocks;
                rn.push(CellNode { list, blocks: bn });
            }
            nodes.push(rn);
        }
        Some((t, nodes))
    }

    /// The document the mirror matches now, from `base` and what changed.
    fn rebuild(&mut self, c: &Changes) -> R<Document> {
        let mut out = self.base.clone();
        if c.all || c.body || self.body.iter().any(|n| touched(n, c)) {
            let old = if c.all { None } else { Some((&self.base.body[..], &self.body[..])) };
            let (blocks, nodes) = self.read_list(BODY, old, c, Fix::Story);
            out.body = blocks;
            self.body = nodes;
        }
        let parts = self.doc.get_map(PARTS);
        let mut keys: BTreeSet<u32> = c.parts.iter().filter_map(|k| k.parse().ok()).collect();
        keys.extend(c.stories.iter().copied());
        keys.extend(self.parts.iter().filter(|(_, nodes)| nodes.iter().any(|n| touched(n, c))).map(|(k, _)| *k));
        if c.all {
            out.parts.clear();
            self.parts.clear();
            keys.extend(parts.keys().filter_map(|k| k.to_string().parse::<u32>().ok()));
        }
        for k in keys {
            let shell = get_str(&parts, &k.to_string()).and_then(|s| serde_json::from_str::<Json>(&s).ok()).filter(Json::is_object);
            let Some(mut v) = shell else {
                out.parts.remove(&k);
                self.parts.remove(&k);
                continue;
            };
            let old = match (out.parts.get(&k), self.parts.get(&k)) {
                (Some(p), Some(n)) => Some((&p.blocks[..], &n[..])),
                _ => None,
            };
            let (blocks, nodes) = self.read_list(&story_list(k), old, c, Fix::Part);
            v["blocks"] = json!([]);
            match serde_json::from_value::<Part>(v) {
                Ok(mut p) => {
                    p.blocks = blocks;
                    out.parts.insert(k, p);
                    self.parts.insert(k, nodes);
                }
                Err(e) => log::warn!("co-editing: part {k}: {e}"),
            }
        }
        let meta = self.doc.get_map(META);
        let meta_keys: BTreeSet<String> = if c.all { meta.keys().map(|k| k.to_string()).collect() } else { c.meta.clone() };
        macro_rules! get_field {
            ($f:ident, $k:literal) => {
                if meta_keys.contains($k)
                    && let Some(s) = get_str(&meta, $k)
                    && let Ok(v) = serde_json::from_str(&s)
                {
                    out.$f = v;
                }
            };
        }
        meta_fields!(get_field);
        let comments = self.doc.get_map(COMMENTS);
        let comment_keys: BTreeSet<String> = if c.all {
            out.comments.clear();
            comments.keys().map(|k| k.to_string()).collect()
        } else {
            c.comments.clone()
        };
        for k in &comment_keys {
            let Ok(n) = k.parse::<u32>() else { continue };
            match get_str(&comments, k).and_then(|s| serde_json::from_str(&s).ok()) {
                Some(v) => {
                    out.comments.insert(n, v);
                }
                None => {
                    out.comments.remove(&n);
                }
            }
        }
        for (name, keys, target) in [(MEDIA, &c.media, &mut out.media), (PASS, &c.pass, &mut out.passthrough)] {
            let map = self.doc.get_map(name);
            let keys: BTreeSet<String> = if c.all {
                target.clear();
                map.keys().map(|k| k.to_string()).collect()
            } else {
                keys.clone()
            };
            for k in &keys {
                match map.get(k) {
                    Some(ValueOrContainer::Value(v @ LoroValue::Binary(_))) => {
                        if let Ok(bytes) = Arc::<Vec<u8>>::try_from(v) {
                            target.insert(k.clone(), bytes);
                        }
                    }
                    _ => {
                        target.remove(k);
                    }
                }
            }
        }
        Ok(out)
    }

    fn nodes_of(&self, s: StoryRef) -> Option<&[Node]> {
        match s {
            StoryRef::Body => Some(&self.body),
            StoryRef::Part(k) => self.parts.get(&k).map(Vec::as_slice),
        }
    }

    /// Where the block `id` is now: its story and path.
    fn find(&self, id: &str) -> Option<(StoryRef, Vec<u32>)> {
        if id.is_empty() {
            return None;
        }
        let mut path = Vec::new();
        if path_of(&self.body, id, &mut path) {
            return Some((StoryRef::Body, path));
        }
        for (k, nodes) in &self.parts {
            path.clear();
            if path_of(nodes, id, &mut path) {
                return Some((StoryRef::Part(*k), path));
            }
        }
        None
    }

    /// Where `p` is, kept across the others' changes.
    pub(crate) fn anchor(&self, p: &Pos, doc: &Document) -> Option<Anchor> {
        let nodes = self.nodes_of(p.story)?;
        let index = *p.path.0.first()? as usize;
        let top = nodes.get(index)?.id.clone();
        let node = node_at(nodes, &p.path.0);
        let cursor = match (node, doc.para_at(p)) {
            (Some(n), Some(q)) if !n.id.is_empty() && n.cells.is_empty() => text_of(&self.doc, &n.id).get_cursor(chars_at(&q.text, p.off), Side::Middle),
            _ => None,
        };
        Some(Anchor { story: p.story, id: node.map(|n| n.id.clone()).unwrap_or_default(), top, index, off: p.off, cursor })
    }

    /// Where an anchor is now in `doc` (the document after the changes).
    pub(crate) fn resolve(&self, a: &Anchor, doc: &Document) -> Pos {
        if let Some((story, path)) = self.find(&a.id)
            && story == a.story
        {
            let pos = Pos { story, path: Path(path), off: 0 };
            let off = match (&a.cursor, doc.para_at(&pos)) {
                (Some(cur), Some(q)) => self.doc.get_cursor_pos(cur).map(|r| bytes_at(&q.text, r.current.pos)).unwrap_or(a.off.min(q.text.len())),
                (None, Some(q)) => a.off.min(q.text.len()),
                _ => 0,
            };
            return Pos { off, ..pos };
        }
        // Its paragraph is gone: the start of the block it was in, or of the one in its place.
        let blocks = doc.story(a.story).filter(|b| !b.is_empty());
        let Some(blocks) = blocks else { return Pos::body(0, 0) };
        let nodes = self.nodes_of(a.story).unwrap_or(&[]);
        let index = nodes.iter().position(|n| !a.top.is_empty() && n.id == a.top).unwrap_or(a.index).min(blocks.len() - 1);
        let path = match blocks.get(index).map(|b| &**b) {
            Some(Block::Table(_)) => vec![index as u32, 0, 0, 0],
            _ => vec![index as u32],
        };
        Pos { story: a.story, path: Path(path), off: 0 }
    }

    /// The paragraph id and character offset of `p`, for the others to draw this caret.
    pub(crate) fn presence_of(&self, p: &Pos, doc: &Document) -> Option<(String, usize)> {
        let node = node_at(self.nodes_of(p.story)?, &p.path.0)?;
        if node.id.is_empty() {
            return None;
        }
        let off = doc.para_at(p).map_or(0, |q| chars_at(&q.text, p.off));
        Some((node.id.clone(), off))
    }

    /// Where someone else's caret is here (from their paragraph id and character offset).
    pub(crate) fn peer_pos(&self, id: &str, off: usize, doc: &Document) -> Option<Pos> {
        let (story, path) = self.find(id)?;
        let pos = Pos { story, path: Path(path), off: 0 };
        let off = doc.para_at(&pos).map_or(0, |q| bytes_at(&q.text, off));
        Some(Pos { off, ..pos })
    }
}

/// What someone else's update changed, by the mirror's keys (`all`: everything, when joining).
#[derive(Default)]
struct Changes {
    all: bool,
    body: bool,
    /// Blocks (`p:`/`t:`), cell lists (`l:`) and part block lists (`s:`) changed.
    blocks: BTreeSet<String>,
    cells: BTreeSet<String>,
    stories: BTreeSet<u32>,
    meta: BTreeSet<String>,
    parts: BTreeSet<String>,
    comments: BTreeSet<String>,
    media: BTreeSet<String>,
    pass: BTreeSet<String>,
}

impl Session {
    /// Whether the document is being edited with others.
    pub fn collab_active(&self) -> bool {
        self.collab.is_some()
    }

    /// Shares the open document (the first person to open it): the snapshot the others join from.
    pub fn collab_seed(&mut self) -> R<Vec<u8>> {
        let c = Collab::seed(&self.doc)?;
        let snapshot = c.snapshot()?;
        self.collab = Some(Box::new(c));
        Ok(snapshot)
    }

    /// Joins the shared document from its snapshot (the same file must be open here).
    pub fn collab_join(&mut self, snapshot: &[u8]) -> R<()> {
        let (c, doc) = Collab::join(snapshot, &self.doc)?;
        self.collab = Some(Box::new(c));
        self.replace_shared(doc, None);
        Ok(())
    }

    /// Takes in someone else's changes.
    pub fn collab_receive(&mut self, update: &[u8]) -> R<()> {
        let Some(c) = self.collab.as_ref() else { return Ok(()) };
        let anchors = (c.anchor(&self.sel.anchor, &self.doc), c.anchor(&self.sel.focus, &self.doc));
        let next = self.collab.as_mut().map(|c| c.receive(update)).transpose()?.flatten();
        if let Some(doc) = next {
            self.replace_shared(doc, Some(anchors));
        }
        Ok(())
    }

    /// Undo / Redo in a shared document: this person's own changes.
    pub fn collab_undo(&mut self, redo: bool) -> R<Json> {
        let Some(c) = self.collab.as_ref() else { return Ok(json!({"done": false})) };
        let anchors = (c.anchor(&self.sel.anchor, &self.doc), c.anchor(&self.sel.focus, &self.doc));
        let next = self.collab.as_mut().map(|c| c.undo(redo)).transpose()?.flatten();
        match next {
            Some(doc) => {
                self.replace_shared(doc, Some(anchors));
                Ok(json!({"done": true}))
            }
            None => Ok(json!({"done": false})),
        }
    }

    pub fn collab_snapshot(&self) -> R<Vec<u8>> {
        self.collab.as_ref().ok_or_else(|| err("not shared"))?.snapshot()
    }

    pub fn collab_outbox(&mut self) -> Vec<Vec<u8>> {
        self.collab.as_mut().map(|c| c.take_outbox()).unwrap_or_default()
    }

    pub fn collab_stop(&mut self) {
        self.collab = None;
        self.collab_peers.clear();
    }

    /// This person's caret for the others: (paragraph id, character offset).
    pub fn collab_presence(&self) -> Option<(String, usize)> {
        self.collab.as_ref()?.presence_of(&self.sel.focus, &self.doc)
    }

    /// Someone else's caret here, from their presence.
    pub fn collab_peer_pos(&self, id: &str, off: usize) -> Option<Pos> {
        self.collab.as_ref()?.peer_pos(id, off, &self.doc)
    }

    /// Writes this person's changes into the shared document (after a top-level command).
    pub(crate) fn collab_capture(&mut self) {
        let rev = self.rev();
        let Some(c) = self.collab.as_mut() else { return };
        if let Err(e) = c.capture(&self.doc, rev) {
            self.status = e.to_string();
        }
    }

    /// Puts the shared document in place, keeping the caret where it was in the text.
    fn replace_shared(&mut self, doc: Document, anchors: Option<(Option<Anchor>, Option<Anchor>)>) {
        self.doc = doc;
        if let (Some(c), Some((a, f))) = (self.collab.as_ref(), anchors) {
            if let Some(a) = a {
                self.sel.anchor = c.resolve(&a, &self.doc);
            }
            if let Some(f) = f {
                self.sel.focus = c.resolve(&f, &self.doc);
            }
        }
        self.clamp_selection();
        self.touch();
        let rev = self.rev();
        if let Some(c) = self.collab.as_mut() {
            c.seen_rev = Some(rev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::PartKind;

    fn session(text: &str) -> Session {
        let mut s = Session::new(Document::new());
        s.run("text.insert", &json!({"text": text})).unwrap();
        s
    }

    fn texts(s: &Session) -> Vec<String> {
        s.doc.body.iter().filter_map(|b| b.as_para().map(|p| p.text.clone())).collect()
    }

    fn pass(from: &mut Session, to: &mut Session) {
        for u in from.collab_outbox() {
            to.collab_receive(&u).unwrap();
        }
    }

    fn share(a: &mut Session) -> Session {
        let snapshot = a.collab_seed().unwrap();
        let mut b = Session::new(Document::new());
        b.collab_join(&snapshot).unwrap();
        b
    }

    fn pair_sessions(text: &str) -> (Session, Session) {
        let mut a = session(text);
        let b = share(&mut a);
        (a, b)
    }

    fn caret(s: &mut Session, story: StoryRef, path: &[u32], off: usize) {
        s.sel = crate::Selection::caret(Pos { story, path: Path(path.to_vec()), off });
    }

    /// A paragraph, a 2×2 table (a b / c d) and the paragraph after it.
    fn with_table() -> Document {
        let mut d = Document::new();
        let row = |x: &str, y: &str| Row { props: Default::default(), cells: vec![Cell::with_text(x), Cell::with_text(y)], controls: Default::default() };
        let t = Table { props: Default::default(), grid: vec![100.0, 100.0], rows: vec![row("a", "b"), row("c", "d")], controls: Default::default() };
        d.body = vec![para_block(Paragraph::with_text("antes", Default::default())), Arc::new(Block::Table(t)), para_block(Paragraph::new())];
        d
    }

    fn cell(s: &Session, r: usize, c: usize) -> String {
        let t = s.doc.body[1].as_table().expect("the table");
        t.rows[r].cells[c].blocks.iter().filter_map(|b| b.as_para().map(|p| p.text.clone())).collect::<Vec<_>>().join("|")
    }

    #[test]
    fn typing_in_the_same_paragraph_keeps_both() {
        let (mut a, mut b) = pair_sessions("abc");
        assert_eq!(texts(&b), vec!["abc"]);
        // A types at the end, B at the start, at the same time.
        a.run("text.insert", &json!({"text": "X"})).unwrap();
        b.sel = crate::Selection::caret(Pos::body(0, 0));
        b.run("text.insert", &json!({"text": "Y"})).unwrap();
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        assert_eq!(texts(&a), vec!["YabcX"]);
        assert_eq!(texts(&b), vec!["YabcX"]);
        // B's caret stayed after the Y it typed, though A's X came in.
        assert_eq!(b.sel.focus, Pos::body(0, 1));
    }

    #[test]
    fn new_paragraphs_and_formatting_reach_the_other() {
        let (mut a, mut b) = pair_sessions("primeira");
        a.run("text.newParagraph", &json!({})).unwrap();
        a.run("text.insert", &json!({"text": "segunda"})).unwrap();
        a.run("select.all", &json!({})).unwrap();
        a.run("format.bold", &json!({})).unwrap();
        pass(&mut a, &mut b);
        // (AutoCorrect capitalises the first word when Enter ends it.)
        assert_eq!(texts(&b), vec!["Primeira", "segunda"]);
        assert_eq!(texts(&b), texts(&a));
        let bold = |s: &Session| s.doc.body.iter().filter_map(|x| x.as_para()).all(|p| p.runs.iter().all(|r| r.props.bold == Some(true)));
        assert!(bold(&a), "A's paragraphs are bold");
        assert!(bold(&b), "B sees them bold");
    }

    #[test]
    fn undo_takes_back_only_ones_own_typing() {
        let (mut a, mut b) = pair_sessions("base");
        a.run("text.insert", &json!({"text": "A"})).unwrap();
        pass(&mut a, &mut b);
        b.sel = crate::Selection::caret(Pos::body(0, 0));
        b.run("text.insert", &json!({"text": "B"})).unwrap();
        pass(&mut b, &mut a);
        a.run("edit.undo", &json!({})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(texts(&a), vec!["Bbase"]);
        assert_eq!(texts(&b), vec!["Bbase"]);
    }

    #[test]
    fn typing_in_two_cells_of_a_table_keeps_both() {
        let mut a = Session::new(with_table());
        let mut b = share(&mut a);
        assert_eq!(cell(&b, 1, 1), "d");
        // A types in the first cell, B in the last one, at the same time; then both in the same.
        caret(&mut a, StoryRef::Body, &[1, 0, 0, 0], 1);
        a.run("text.insert", &json!({"text": "X"})).unwrap();
        caret(&mut b, StoryRef::Body, &[1, 1, 1, 0], 0);
        b.run("text.insert", &json!({"text": "Y"})).unwrap();
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        caret(&mut b, StoryRef::Body, &[1, 0, 0, 0], 0);
        b.run("text.insert", &json!({"text": "Z"})).unwrap();
        a.run("text.insert", &json!({"text": "W"})).unwrap();
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        for s in [&a, &b] {
            assert_eq!(cell(s, 0, 0), "ZaXW");
            assert_eq!(cell(s, 1, 1), "Yd");
            assert_eq!(cell(s, 0, 1), "b");
        }
        // B's caret is still in the first cell, after its Z.
        assert_eq!(b.sel.focus, Pos { story: StoryRef::Body, path: Path(vec![1, 0, 0, 0]), off: 1 });
    }

    #[test]
    fn a_new_row_and_typing_in_a_cell_keep_both() {
        let mut a = Session::new(with_table());
        let mut b = share(&mut a);
        caret(&mut a, StoryRef::Body, &[1, 0, 1, 0], 1);
        a.run("text.insert", &json!({"text": "X"})).unwrap();
        caret(&mut b, StoryRef::Body, &[1, 1, 0, 0], 0);
        b.run("table.insertRowBelow", &json!({})).unwrap();
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        for s in [&a, &b] {
            assert_eq!(s.doc.body[1].as_table().unwrap().rows.len(), 3);
            assert_eq!(cell(s, 0, 1), "bX");
            assert_eq!(cell(s, 1, 0), "c");
        }
        // And text typed in the new row reaches the other.
        caret(&mut b, StoryRef::Body, &[1, 2, 0, 0], 0);
        b.run("text.insert", &json!({"text": "nova"})).unwrap();
        pass(&mut b, &mut a);
        assert_eq!(cell(&a, 2, 0), cell(&b, 2, 0));
        assert!(cell(&a, 2, 0).to_lowercase().starts_with("nova"), "{}", cell(&a, 2, 0));
    }

    #[test]
    fn typing_in_a_header_keeps_both() {
        let mut d = Document::new();
        d.parts.insert(1, Part { kind: PartKind::Header, blocks: vec![para_block(Paragraph::with_text("cab", Default::default()))], body: Default::default() });
        let mut a = Session::new(d);
        let mut b = share(&mut a);
        assert_eq!(b.doc.parts.get(&1).map(|p| p.blocks.len()), Some(1));
        caret(&mut a, StoryRef::Part(1), &[0], 3);
        a.run("text.insert", &json!({"text": "X"})).unwrap();
        caret(&mut b, StoryRef::Part(1), &[0], 0);
        b.run("text.insert", &json!({"text": "Y"})).unwrap();
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        let header = |s: &Session| s.doc.parts[&1].blocks[0].as_para().unwrap().text.clone();
        assert_eq!(header(&a), "YcabX");
        assert_eq!(header(&b), "YcabX");
    }

    #[test]
    fn a_caret_in_a_cell_or_header_is_found_by_the_other() {
        let mut d = with_table();
        d.parts.insert(4, Part { kind: PartKind::Footer, blocks: vec![para_block(Paragraph::with_text("rodapé", Default::default()))], body: Default::default() });
        let mut a = Session::new(d);
        let b = share(&mut a);
        for (story, path, off) in [(StoryRef::Body, vec![1, 1, 0, 0], 1), (StoryRef::Part(4), vec![0], 3), (StoryRef::Body, vec![0], 2)] {
            caret(&mut a, story, &path, off);
            let (id, chars) = a.collab_presence().expect("a presence");
            assert_eq!(b.collab_peer_pos(&id, chars), Some(Pos { story, path: Path(path.clone()), off }));
        }
    }
}
