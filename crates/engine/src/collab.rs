//! Verto: co-editing — several people in one document at once, as in Google Docs.
//!
//! The document stays WordCraft's own; a CRDT (Loro) mirrors it. After each top-level command the
//! session compares the document with the one the mirror matched ([`Collab::capture`]) and writes
//! only what changed; the page carries those updates to the others, and their updates come back
//! through [`Session::collab_receive`], which rebuilds just the blocks they touched.
//!
//! What the mirror keeps (Loro root containers):
//! - `body`: the top-level blocks' ids, in order (a movable list).
//! - `t:{id}`: a paragraph's text, merged character by character, with its runs' formatting as a
//!   mark (`f`, the run's `CharProps` as JSON) that sticks to the text it formats.
//! - `p:{id}`: `k` (`p` paragraph, `t` table), and `p` (the paragraph's own properties, mark,
//!   section and inline objects, as JSON) or `j` (a table, whole, as JSON: two people editing the
//!   same table at once keep the last change).
//! - `meta`: the document's settings, styles, numbering, section… one JSON per field.
//! - `parts` (headers, footers, notes…), `comments`, `media` and `pass`: one entry each.
//!
//! Undo and Redo in a shared document undo this person's own changes ([`UndoManager`]), adjusted
//! to what the others did since. The caret stays where it was in the text when others edit
//! (Loro cursors).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use loro::cursor::{Cursor, Side};
use loro::event::Diff;
use loro::{
    ContainerID, ExpandType, ExportMode, Frontiers, LoroDoc, LoroMap, LoroMovableList, LoroText, LoroValue, StyleConfig, StyleConfigMap, TextDelta,
    UndoManager, ValueOrContainer, VersionVector,
};
use serde_json::{Value as Json, json};
use wordcraft_doc::{Block, Document, Paragraph, Path, Pos, StoryRef};

use crate::{CmdError, Session};

/// The shared document's format: a document shared by another format starts over from the file.
pub const FORMAT: i64 = 1;

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
    /// The id of the top-level block their caret is in, and where in it (characters), as their
    /// presence says: [`Session::collab_peer_pos`] finds it here.
    pub block: String,
    pub off: usize,
}

/// Where a caret was, kept across someone else's changes.
#[derive(Clone, Debug)]
pub(crate) struct Anchor {
    id: String,
    index: usize,
    rest: Vec<u32>,
    off: usize,
    cursor: Option<Cursor>,
}

pub struct Collab {
    doc: LoroDoc,
    /// The top-level blocks' ids, in the document's order.
    ids: Vec<String>,
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
        f.debug_struct("Collab").field("peer", &self.doc.peer_id()).field("blocks", &self.ids.len()).finish()
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

/// The block `id` as the mirror has it.
fn block_from(doc: &LoroDoc, id: &str) -> Option<Arc<Block>> {
    match get_str(&block_map(doc, id), "k").as_deref() {
        Some("t") => {
            let j = get_str(&block_map(doc, id), "j")?;
            serde_json::from_str::<Block>(&j).ok().map(Arc::new)
        }
        _ => para_from(doc, id).map(|p| Arc::new(Block::Para(p))),
    }
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

/// Pairs the old blocks with the new ones in order, best likeness first (`None`: deleted or
/// inserted). Small regions are aligned exactly; large ones position by position.
fn pair(old: &[Arc<Block>], new: &[Arc<Block>]) -> Vec<(Option<usize>, Option<usize>)> {
    let (m, n) = (old.len(), new.len());
    if m * n > 40_000 {
        return (0..m.max(n)).map(|i| ((i < m).then_some(i), (i < n).then_some(i))).collect();
    }
    // score[i][j]: best total likeness of old[i..] with new[j..].
    let mut score = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            let l = likeness(&old[i], &new[j]);
            let take = if l > 0 { l + score[i + 1][j + 1] } else { 0 };
            score[i][j] = take.max(score[i + 1][j]).max(score[i][j + 1]);
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n {
            let l = likeness(&old[i], &new[j]);
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

impl Collab {
    fn empty(doc: LoroDoc, base: Document) -> Collab {
        let undo = new_undo(&doc);
        Collab { sent: doc.oplog_vv(), doc, ids: Vec::new(), base, seen_rev: None, outbox: Vec::new(), next_id: 0, undo }
    }

    /// Starts a shared document from `doc`: the first person to open the file.
    pub fn seed(doc: &Document) -> R<Collab> {
        let mut c = Collab::empty(new_doc(), Document::default());
        c.doc.get_map("format").insert("version", FORMAT).map_err(err)?;
        let list = c.doc.get_movable_list(BODY);
        for (i, b) in doc.body.iter().enumerate() {
            let id = format!("b{i}");
            list.push(id.as_str()).map_err(err)?;
            c.write_block(&id, b)?;
            c.ids.push(id);
        }
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
        c.ids = movable_strings(&c.doc.get_movable_list(BODY));
        let all: BTreeSet<String> = c.ids.iter().cloned().collect();
        let keys = |name: &str| -> BTreeSet<String> { c.doc.get_map(name).keys().map(|k| k.to_string()).collect() };
        let changes = Changes { body: true, blocks: all, meta: keys(META), parts: keys(PARTS), comments: keys(COMMENTS), media: keys(MEDIA), pass: keys(PASS) };
        let out = c.rebuild(&changes, true)?;
        c.base = out.clone();
        Ok((c, out))
    }

    pub fn snapshot(&self) -> R<Vec<u8>> {
        self.doc.export(ExportMode::Snapshot).map_err(err)
    }

    pub fn take_outbox(&mut self) -> Vec<Vec<u8>> {
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

    /// Writes a block under `id` (its text merged into what the mirror has).
    fn write_block(&self, id: &str, b: &Block) -> R<()> {
        let m = block_map(&self.doc, id);
        match b {
            Block::Para(p) => {
                put(&m, "k", Some("p".into()))?;
                put(&m, "p", Some(shell_of(p)?))?;
                self.write_text(id, p)
            }
            Block::Table(_) => {
                put(&m, "k", Some("t".into()))?;
                put(&m, "j", Some(to_json(b)?))
            }
        }
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
    fn write_doc_fields(&self, before: Option<&Document>, after: &Document) -> R<()> {
        let meta = self.doc.get_map(META);
        macro_rules! put_field {
            ($f:ident, $k:literal) => {
                if before.is_none_or(|b| b.$f != after.$f) {
                    put(&meta, $k, Some(to_json(&after.$f)?))?;
                }
            };
        }
        meta_fields!(put_field);
        let parts = self.doc.get_map(PARTS);
        let keys: BTreeSet<u32> = after.parts.keys().chain(before.map(|b| b.parts.keys()).into_iter().flatten()).copied().collect();
        for k in keys {
            let (old, new) = (before.and_then(|b| b.parts.get(&k)), after.parts.get(&k));
            if before.is_none() || old != new {
                put(&parts, &k.to_string(), new.map(to_json).transpose()?)?;
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
        let old = self.base.body.clone();
        let new = &after.body;
        let same = old.len() == new.len() && old.iter().zip(new).all(|(a, b)| Arc::ptr_eq(a, b));
        if !same {
            let pre = old.iter().zip(new).take_while(|(a, b)| Arc::ptr_eq(a, b)).count();
            let suf = old[pre..].iter().rev().zip(new[pre..].iter().rev()).take_while(|(a, b)| Arc::ptr_eq(a, b)).count();
            let (om, nm) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
            let list = self.doc.get_movable_list(BODY);
            let mut ids: Vec<String> = self.ids[..pre].to_vec();
            let mut at = pre;
            for (o, n) in pair(om, nm) {
                match (o, n) {
                    (Some(oi), Some(ni)) => {
                        let id = self.ids[pre + oi].clone();
                        if !Arc::ptr_eq(&om[oi], &nm[ni]) && *om[oi] != *nm[ni] {
                            self.write_block(&id, &nm[ni])?;
                        }
                        ids.push(id);
                        at += 1;
                    }
                    (Some(_), None) => {
                        list.delete(at, 1).map_err(err)?;
                    }
                    (None, Some(ni)) => {
                        let id = self.new_id();
                        list.insert(at, id.as_str()).map_err(err)?;
                        self.write_block(&id, &nm[ni])?;
                        ids.push(id);
                        at += 1;
                    }
                    (None, None) => {}
                }
            }
            ids.extend(self.ids[old.len() - suf..].iter().cloned());
            self.ids = ids;
        }
        self.write_doc_fields(Some(&self.base.clone()), after)?;
        self.flush()?;
        self.base = after.clone();
        Ok(())
    }

    /// Takes in an update from someone else: the document with their changes, or `None` when it
    /// changed nothing here.
    pub fn receive(&mut self, update: &[u8]) -> R<Option<Document>> {
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
                    }
                }
            }
        }
        let out = self.rebuild(&c, false)?;
        self.base = out.clone();
        self.seen_rev = None;
        Ok(Some(out))
    }

    /// The document the mirror matches now, from `base` and what changed.
    fn rebuild(&mut self, c: &Changes, all: bool) -> R<Document> {
        let mut out = self.base.clone();
        let known: HashMap<String, Arc<Block>> = self.ids.iter().cloned().zip(self.base.body.iter().cloned()).collect();
        if c.body {
            self.ids = movable_strings(&self.doc.get_movable_list(BODY));
        }
        if c.body || !c.blocks.is_empty() || all {
            let mut body = Vec::with_capacity(self.ids.len());
            let mut ids = Vec::with_capacity(self.ids.len());
            for id in &self.ids {
                let block = if all || c.blocks.contains(id) || !known.contains_key(id) { block_from(&self.doc, id) } else { known.get(id).cloned() };
                if let Some(b) = block {
                    body.push(b);
                    ids.push(id.clone());
                }
            }
            self.ids = ids;
            out.body = body;
        }
        let meta = self.doc.get_map(META);
        macro_rules! get_field {
            ($f:ident, $k:literal) => {
                if c.meta.contains($k)
                    && let Some(s) = get_str(&meta, $k)
                    && let Ok(v) = serde_json::from_str(&s)
                {
                    out.$f = v;
                }
            };
        }
        meta_fields!(get_field);
        let parts = self.doc.get_map(PARTS);
        for k in &c.parts {
            let Ok(n) = k.parse::<u32>() else { continue };
            match get_str(&parts, k).and_then(|s| serde_json::from_str(&s).ok()) {
                Some(p) => {
                    out.parts.insert(n, p);
                }
                None => {
                    out.parts.remove(&n);
                }
            }
        }
        let comments = self.doc.get_map(COMMENTS);
        for k in &c.comments {
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
            for k in keys {
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
        out.ensure_nonempty();
        Ok(out)
    }

    /// Where `p` is, kept across the others' changes (top-level blocks of the body).
    pub(crate) fn anchor(&self, p: &Pos, doc: &Document) -> Option<Anchor> {
        if p.story != StoryRef::Body {
            return None;
        }
        let index = *p.path.0.first()? as usize;
        let id = self.ids.get(index)?.clone();
        let cursor = if p.path.0.len() == 1 {
            let text = doc.body.get(index).and_then(|b| b.as_para()).map(|q| q.text.as_str()).unwrap_or("");
            text_of(&self.doc, &id).get_cursor(chars_at(text, p.off), Side::Middle)
        } else {
            None
        };
        Some(Anchor { id, index, rest: p.path.0[1..].to_vec(), off: p.off, cursor })
    }

    /// Where an anchor is now in `doc` (the document after the changes).
    pub(crate) fn resolve(&self, a: &Anchor, doc: &Document) -> Pos {
        let Some(index) = self.ids.iter().position(|i| *i == a.id) else {
            // Its block is gone: the block that took its place, at the start.
            let index = a.index.min(doc.body.len().saturating_sub(1));
            return Pos { story: StoryRef::Body, path: Path(vec![index as u32]), off: 0 };
        };
        let mut path = vec![index as u32];
        path.extend(&a.rest);
        let off = match (&a.cursor, doc.body.get(index).and_then(|b| b.as_para())) {
            (Some(cur), Some(q)) => self.doc.get_cursor_pos(cur).map(|r| bytes_at(&q.text, r.current.pos)).unwrap_or(a.off.min(q.text.len())),
            _ => a.off,
        };
        Pos { story: StoryRef::Body, path: Path(path), off }
    }

    /// The top-level block id and character offset of `p`, for the others to draw this caret.
    pub(crate) fn presence_of(&self, p: &Pos, doc: &Document) -> Option<(String, usize)> {
        if p.story != StoryRef::Body {
            return None;
        }
        let index = *p.path.0.first()? as usize;
        let id = self.ids.get(index)?.clone();
        let off = if p.path.0.len() == 1 { doc.body.get(index).and_then(|b| b.as_para()).map_or(0, |q| chars_at(&q.text, p.off)) } else { 0 };
        Some((id, off))
    }

    /// Where someone else's caret is here (from their block id and character offset).
    pub(crate) fn peer_pos(&self, id: &str, off: usize, doc: &Document) -> Option<Pos> {
        let index = self.ids.iter().position(|i| i == id)?;
        let off = doc.body.get(index).and_then(|b| b.as_para()).map_or(0, |q| bytes_at(&q.text, off));
        Some(Pos { story: StoryRef::Body, path: Path(vec![index as u32]), off })
    }
}

/// What someone else's update changed, by the mirror's keys.
#[derive(Default)]
struct Changes {
    body: bool,
    blocks: BTreeSet<String>,
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

    /// This person's caret for the others: (block id, character offset).
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

    fn pair_sessions(text: &str) -> (Session, Session) {
        let mut a = session(text);
        let snapshot = a.collab_seed().unwrap();
        let mut b = Session::new(Document::new());
        b.collab_join(&snapshot).unwrap();
        (a, b)
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
        assert_eq!(texts(&b), vec!["primeira", "segunda"]);
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
}
