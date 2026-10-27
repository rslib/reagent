//! Repair of file-tool arguments that weak models get wrong: aliases for
//! `path`, `edits` in another shape, numbers sent as strings, a missing
//! `path` and a mistyped absolute path.
//!
//! A well-formed call with a relative path returns `None` from [`repair`]
//! after one borrowed parse: no future, no file system call and no copy. One
//! with an absolute path costs a boxed future and one `metadata` call, and is
//! parsed again only when that file does not exist.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use reagent_agent::tool::Repaired;

use crate::blocking;

/// The most files [`Touched`] keeps.
const TOUCHED_CAP: usize = 16;
/// The most files a missing-path refusal names.
const NAMED: usize = 3;
/// The most changed characters between a mistyped folder and the real one.
const MAX_TYPOS: usize = 4;

/// The files the run's read, edit and write calls touched, newest last,
/// shared by those tools. A file inside the working folder is kept relative
/// to it, so `/var/...` and `/private/var/...` spellings meet.
#[derive(Debug, Default, Clone)]
pub struct Touched(Arc<Mutex<Vec<PathBuf>>>);

impl Touched {
    /// Records that a call in `working_dir` read or changed `path`, as the
    /// call named it.
    pub fn note(&self, working_dir: &Path, path: &Path) {
        let mut files = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = match path.is_relative() {
            true => path,
            false => inside(working_dir, path).map_or(path, Path::new),
        };
        // A repeat of the newest file, the common case, costs no block.
        if files.last().is_some_and(|last| last == key) {
            return;
        }
        files.retain(|seen| seen != key);
        if files.len() == TOUCHED_CAP {
            files.remove(0);
        }
        files.push(key.to_path_buf());
    }

    fn snapshot(&self) -> Vec<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn newest(&self) -> Option<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }

    fn contains(&self, path: &Path) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|seen| seen == path)
    }
}

/// Which file tool a call is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Read,
    Edit,
    Write,
}

/// The fields whose shape decides whether a call needs repair, borrowed.
#[derive(Deserialize)]
struct Probe<'a> {
    #[serde(default, borrow)]
    path: Option<Borrowed<'a>>,
    #[serde(default, borrow)]
    file_path: Option<&'a RawValue>,
    #[serde(default, borrow, rename = "filePath")]
    file_path_camel: Option<&'a RawValue>,
    #[serde(default, borrow)]
    file: Option<&'a RawValue>,
    #[serde(default)]
    edits: Option<EditsShape>,
    #[serde(default, borrow)]
    offset: Option<&'a RawValue>,
    #[serde(default, borrow)]
    limit: Option<&'a RawValue>,
    #[serde(default, borrow)]
    line: Option<&'a RawValue>,
}

impl Probe<'_> {
    /// Whether the call is in the documented shape, so only an absolute
    /// path that may not exist is left to check.
    fn well_formed(&self) -> bool {
        let quoted = |raw: Option<&RawValue>| raw.is_some_and(|raw| raw.get().starts_with('"'));
        self.path.is_some()
            && self.file_path.is_none()
            && self.file_path_camel.is_none()
            && self.file.is_none()
            && !quoted(self.offset)
            && !quoted(self.limit)
            && !quoted(self.line)
            && self.edits.as_ref().is_none_or(|edits| edits.0)
    }
}

/// A string borrowed from the call when it holds no escape: serde borrows a
/// `Cow` only as a direct field, never inside an `Option`.
#[derive(Deserialize)]
struct Borrowed<'a>(#[serde(borrow)] Cow<'a, str>);

/// Whether `edits` is a list whose items carry no `path` and no `line` given
/// as a string, read item by item without keeping them.
struct EditsShape(bool);

/// The fields of one `edits` item that need repair.
#[derive(Deserialize)]
struct EditProbe<'a> {
    #[serde(default, borrow)]
    path: Option<&'a RawValue>,
    #[serde(default, borrow)]
    line: Option<&'a RawValue>,
}

impl<'de> Deserialize<'de> for EditsShape {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Shape;

        impl<'de> serde::de::Visitor<'de> for Shape {
            type Value = EditsShape;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the edits")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<EditsShape, A::Error> {
                let mut plain = true;
                while let Some(item) = seq.next_element::<EditProbe<'de>>()? {
                    let quoted_line = item.line.is_some_and(|line| line.get().starts_with('"'));
                    plain &= item.path.is_none() && !quoted_line;
                }
                Ok(EditsShape(plain))
            }

            fn visit_str<E>(self, _text: &str) -> Result<EditsShape, E> {
                Ok(EditsShape(false))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<EditsShape, A::Error> {
                while map
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(EditsShape(false))
            }
        }

        deserializer.deserialize_any(Shape)
    }
}

/// The repair of one call to a `kind` tool in `working_dir`, or `None` when
/// the call needs none.
pub fn repair<'a>(
    kind: Kind,
    working_dir: &'a Path,
    touched: &'a Touched,
    args: &'a str,
) -> Option<BoxFuture<'a, Repaired>> {
    let probe: Probe<'_> = serde_json::from_str(args).ok()?;
    if !probe.well_formed() {
        return Some(Box::pin(repair_slow(kind, working_dir, touched, args)));
    }
    let path = probe.path?.0;
    if kind == Kind::Write || !Path::new(path.as_ref()).is_absolute() {
        return None;
    }
    // A well-formed absolute path is parsed again only when it names no file.
    Some(Box::pin(async move {
        let probed = PathBuf::from(path.as_ref());
        match blocking::run(move || is_file(&probed)).await {
            true => Repaired::Keep,
            false => repair_slow(kind, working_dir, touched, args).await,
        }
    }))
}

async fn repair_slow(kind: Kind, working_dir: &Path, touched: &Touched, args: &str) -> Repaired {
    let Ok(Value::Object(mut call)) = serde_json::from_str::<Value>(args) else {
        return Repaired::Keep;
    };
    let mut changed = match reshape(&mut call) {
        Ok(changed) => changed,
        Err(message) => return Repaired::Refuse(message),
    };
    let mut note = None;
    match call.get("path").and_then(Value::as_str).map(str::to_owned) {
        None if kind == Kind::Edit => match fill_path(working_dir, touched, &call).await {
            Ok(path) => {
                note = Some(format!("path missing; used {path}"));
                call.insert("path".into(), Value::String(path));
                changed = true;
            }
            Err(message) => return Repaired::Refuse(message),
        },
        None if kind == Kind::Read => {
            if let Some(file) = touched.newest() {
                let path = shown(&file);
                note = Some(format!(
                    "path missing; used {path}, the newest file this run touched"
                ));
                call.insert("path".into(), Value::String(path));
                changed = true;
            }
        }
        Some(given) if kind != Kind::Write && Path::new(&given).is_absolute() => {
            let given_path = PathBuf::from(&given);
            let dir = working_dir.to_path_buf();
            let found = blocking::run(move || match is_file(&given_path) {
                true => None,
                false => near_file(&dir, &given_path),
            })
            .await;
            if let Some(found) = found {
                if kind == Kind::Edit && !touched.contains(Path::new(&found)) {
                    return Repaired::Refuse(format!(
                        "cannot edit {given}: no such file. Did you mean `{found}`? Read it first."
                    ));
                }
                note = Some(format!("no such file; used {found}"));
                call.insert("path".into(), Value::String(found));
                changed = true;
            }
        }
        _ => {}
    }
    if !changed {
        return Repaired::Keep;
    }
    Repaired::Args {
        args: Value::Object(call).to_string(),
        note,
    }
}

/// Moves aliases and nested paths to `path`, turns `edits` into a list and
/// numbers sent as strings into numbers. Returns whether anything changed, or
/// a refusal when the edits name different files.
fn reshape(call: &mut Map<String, Value>) -> Result<bool, String> {
    let mut changed = false;
    for alias in ["file_path", "filePath", "file"] {
        if let Some(value) = call.remove(alias) {
            changed = true;
            if !call.contains_key("path") {
                call.insert("path".into(), value);
            }
        }
    }
    if let Some(Value::String(text)) = call.get("edits")
        && let Ok(parsed) = serde_json::from_str::<Value>(text)
    {
        call.insert("edits".into(), parsed);
        changed = true;
    }
    if let Some(Value::Object(one)) = call.get("edits") {
        let one = Value::Object(one.clone());
        call.insert("edits".into(), Value::Array(vec![one]));
        changed = true;
    }
    let top = call.get("path").cloned();
    if let Some(Value::Array(edits)) = call.get_mut("edits") {
        let count = edits.len();
        let paths: Vec<Value> = edits
            .iter_mut()
            .filter_map(Value::as_object_mut)
            .filter_map(|edit| edit.remove("path"))
            .collect();
        // An item path is dropped only when it names the call's one file;
        // another file would take an edit meant for it.
        let one = match &top {
            Some(top) => paths.iter().all(|path| path == top),
            None => paths.len() == count && paths.iter().all(|path| Some(path) == paths.first()),
        };
        if !paths.is_empty() && !one {
            return Err(
                "invalid arguments: the edits name different files; send one edit call per file"
                    .into(),
            );
        }
        changed |= !paths.is_empty();
        for edit in edits.iter_mut().filter_map(Value::as_object_mut) {
            changed |= numbers(edit, &["line"]);
        }
        if top.is_none()
            && let Some(first) = paths.into_iter().next()
        {
            call.insert("path".into(), first);
        }
    }
    Ok(changed | numbers(call, &["offset", "limit", "line"]))
}

/// Turns each of `keys` that holds a string of digits into a number.
fn numbers(object: &mut Map<String, Value>, keys: &[&str]) -> bool {
    let mut changed = false;
    for key in keys {
        if let Some(Value::String(text)) = object.get(*key)
            && let Ok(number) = text.trim().parse::<u64>()
        {
            object.insert((*key).into(), Value::from(number));
            changed = true;
        }
    }
    changed
}

/// The one file the run touched, as a path relative to `working_dir` when it
/// lies inside, if every `old_string` of the call matches it exactly once.
async fn fill_path(
    working_dir: &Path,
    touched: &Touched,
    call: &Map<String, Value>,
) -> Result<String, String> {
    let files = touched.snapshot();
    let refuse = || {
        if files.is_empty() {
            return "invalid arguments: `path` is missing; name the file to edit".to_string();
        }
        let named: Vec<String> = files
            .iter()
            .rev()
            .take(NAMED)
            .map(|file| shown(file))
            .collect();
        format!(
            "invalid arguments: `path` is missing; the files this run touched most recently: {}",
            named.join(", ")
        )
    };
    let [file] = files.as_slice() else {
        return Err(refuse());
    };
    let path = working_dir.join(file);
    let Ok(text) = blocking::run(move || read_text(&path)).await else {
        return Err(refuse());
    };
    let mut olds: Vec<&str> = Vec::new();
    if let Some(old) = call.get("old_string").and_then(Value::as_str) {
        olds.push(old);
    }
    if let Some(edits) = call.get("edits").and_then(Value::as_array) {
        olds.extend(
            edits
                .iter()
                .filter_map(|edit| edit.get("old_string")?.as_str()),
        );
    }
    let once = |old: &&str| !old.is_empty() && text.match_indices(*old).take(2).count() == 1;
    if olds.is_empty() || !olds.iter().all(once) {
        return Err(refuse());
    }
    Ok(shown(file))
}

/// The text of `path`, read inside a blocking job.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn read_text(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// The part of the absolute `path` after `working_dir`, with or without a
/// macOS `/private` prefix on either, borrowed; `None` outside it.
fn inside<'p>(working_dir: &Path, path: &'p Path) -> Option<&'p str> {
    let unprivate = |text: &'p str| text.strip_prefix("/private").unwrap_or(text);
    let root = working_dir.to_str()?;
    let root = root.strip_prefix("/private").unwrap_or(root);
    let rest = unprivate(path.to_str()?)
        .strip_prefix(root)?
        .strip_prefix('/')?;
    (!rest.is_empty()).then_some(rest)
}

fn shown(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// The one file in `working_dir` that `given` names with a mistyped folder:
/// the same trailing names, and a leading part within [`MAX_TYPOS`] changed
/// characters of the working folder. A macOS `/private` prefix counts as no
/// change. `None` when no tail or more than one tail fits.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn near_file(working_dir: &Path, given: &Path) -> Option<String> {
    let parts: Vec<Component<'_>> = given.components().collect();
    let roots = [
        working_dir.to_path_buf(),
        std::fs::canonicalize(working_dir).ok()?,
    ];
    let mut found: Option<String> = None;
    for cut in 1..parts.len() {
        // A tail of plain names only, so a repair stays inside the folder.
        if !parts[cut..]
            .iter()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            continue;
        }
        let head: PathBuf = parts[..cut].iter().collect();
        let tail: PathBuf = parts[cut..].iter().collect();
        let close = roots
            .iter()
            .any(|root| typos(&unprivate(&head), &unprivate(root)) <= MAX_TYPOS);
        if !close || !is_file(&working_dir.join(&tail)) {
            continue;
        }
        let tail = tail.to_string_lossy().into_owned();
        match &found {
            Some(seen) if *seen != tail => return None,
            _ => found = Some(tail),
        }
    }
    found
}

/// `path` without a leading `/private`, which macOS adds to `/var` and `/tmp`.
fn unprivate(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix("/private").unwrap_or(&text).to_string()
}

/// The edit distance between `a` and `b` in bytes, or more than
/// [`MAX_TYPOS`] as soon as it cannot be within it.
fn typos(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len().abs_diff(b.len()) > MAX_TYPOS {
        return MAX_TYPOS + 1;
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, &x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        let mut best = row[0];
        for (j, &y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(x != y));
            diagonal = above;
            best = best.min(row[j + 1]);
        }
        if best > MAX_TYPOS {
            return MAX_TYPOS + 1;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(kind: Kind, root: &Path, touched: &Touched, args: Value) -> Option<Repaired> {
        let args = args.to_string();
        match repair(kind, root, touched, &args) {
            Some(future) => Some(future.await),
            None => None,
        }
    }

    fn args_of(repaired: Option<Repaired>) -> (Value, String) {
        match repaired {
            Some(Repaired::Args { args, note }) => (
                serde_json::from_str(&args).unwrap(),
                note.unwrap_or_default(),
            ),
            other => panic!("expected repaired arguments, got {other:?}"),
        }
    }

    /// A well-formed absolute path that exists is checked in one blocking job.
    #[tokio::test]
    async fn one_hop_repair_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        tokio::fs::write(&file, "fn a() {}\n").await.unwrap();
        let touched = Touched::default();
        let call = serde_json::json!({
            "path": file.display().to_string(),
            "old_string": "fn a()",
            "new_string": "fn b()"
        });
        crate::blocking::reset_hops();
        let repaired = run(Kind::Edit, tmp.path(), &touched, call).await;
        assert_eq!(repaired, Some(Repaired::Keep));
        assert_eq!(crate::blocking::hops(), 1);
    }

    #[tokio::test]
    async fn repair_skips_a_well_formed_relative_call() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        let call = serde_json::json!({"path": "a.txt", "offset": 3, "edits": [{"old_string": "x", "new_string": "y"}]});
        assert!(run(Kind::Edit, tmp.path(), &touched, call).await.is_none());
    }

    #[tokio::test]
    async fn repair_reads_aliases_string_edits_and_string_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        let (call, note) = args_of(
            run(
                Kind::Read,
                tmp.path(),
                &touched,
                serde_json::json!({"file_path": "a.txt", "offset": "10", "limit": "2"}),
            )
            .await,
        );
        assert_eq!(
            call,
            serde_json::json!({"path": "a.txt", "offset": 10, "limit": 2})
        );
        assert_eq!(note, "");
        let edits = r#"[{"old_string":"x","new_string":"y"}]"#;
        let (call, _) = args_of(
            run(
                Kind::Edit,
                tmp.path(),
                &touched,
                serde_json::json!({"path": "a.txt", "edits": edits}),
            )
            .await,
        );
        assert_eq!(
            call["edits"],
            serde_json::json!([{"old_string": "x", "new_string": "y"}])
        );
        let (call, _) = args_of(
            run(Kind::Edit, tmp.path(), &touched, serde_json::json!({"edits": [{"path": "a.txt", "old_string": "x", "new_string": "y", "line": "4"}]})).await,
        );
        assert_eq!(
            call,
            serde_json::json!({"path": "a.txt", "edits": [{"old_string": "x", "new_string": "y", "line": 4}]})
        );
    }

    #[tokio::test]
    async fn repair_fills_a_missing_path_from_the_one_touched_file() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(tmp.path().join("src")).await.unwrap();
        let file = tmp.path().join("src/a.rs");
        tokio::fs::write(&file, "fn a() {}\n").await.unwrap();
        let touched = Touched::default();
        touched.note(tmp.path(), &file);
        let call = serde_json::json!({"edits": [{"old_string": "fn a()", "new_string": "fn b()"}]});
        let (call, note) = args_of(run(Kind::Edit, tmp.path(), &touched, call).await);
        assert_eq!(call["path"], "src/a.rs");
        assert_eq!(note, "path missing; used src/a.rs");
    }

    #[tokio::test]
    async fn repair_read_fills_a_missing_path_from_the_newest_touched_file() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        touched.note(tmp.path(), Path::new("a.txt"));
        touched.note(tmp.path(), Path::new("b.txt"));
        let call = serde_json::json!({"offset": 3, "limit": 2});
        let (call, note) = args_of(run(Kind::Read, tmp.path(), &touched, call).await);
        assert_eq!(call["path"], "b.txt");
        assert_eq!(call["offset"], 3);
        assert_eq!(call["limit"], 2);
        assert!(note.contains("b.txt"), "{note}");
    }

    #[tokio::test]
    async fn repair_read_keeps_a_missing_path_when_nothing_was_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        let call = serde_json::json!({"offset": 3});
        let repaired = run(Kind::Read, tmp.path(), &touched, call).await;
        assert!(matches!(repaired, Some(Repaired::Keep)), "got {repaired:?}");
    }

    #[tokio::test]
    async fn repair_refuses_a_missing_path_with_two_touched_files() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        for name in ["a.rs", "b.rs"] {
            let file = tmp.path().join(name);
            tokio::fs::write(&file, "x\n").await.unwrap();
            touched.note(tmp.path(), &file);
        }
        let call = serde_json::json!({"old_string": "x", "new_string": "y"});
        match run(Kind::Edit, tmp.path(), &touched, call).await {
            Some(Repaired::Refuse(message)) => {
                assert!(message.contains("b.rs, a.rs"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let none = Touched::default();
        let call = serde_json::json!({"old_string": "x", "new_string": "y"});
        assert!(matches!(
            run(Kind::Edit, tmp.path(), &none, call).await,
            Some(Repaired::Refuse(_))
        ));
    }

    #[tokio::test]
    async fn repair_refuses_a_missing_path_when_the_text_is_not_there_once() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.rs");
        tokio::fs::write(&file, "x\nx\n").await.unwrap();
        let touched = Touched::default();
        touched.note(tmp.path(), &file);
        let call = serde_json::json!({"old_string": "x", "new_string": "y"});
        assert!(matches!(
            run(Kind::Edit, tmp.path(), &touched, call).await,
            Some(Repaired::Refuse(_))
        ));
    }

    #[tokio::test]
    async fn repair_corrects_a_mistyped_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("w-abc123").join("work");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("ledger.py"), "x\n")
            .await
            .unwrap();
        let typo = tmp.path().join("w-abd123").join("work").join("ledger.py");
        let touched = Touched::default();
        let call = serde_json::json!({"path": typo.to_string_lossy()});
        let (call, note) = args_of(run(Kind::Read, &root, &touched, call).await);
        assert_eq!(call["path"], "ledger.py");
        assert_eq!(note, "no such file; used ledger.py");

        // An edit needs a read of that file first.
        let edit = serde_json::json!({"path": typo.to_string_lossy(), "old_string": "x", "new_string": "y"});
        match run(Kind::Edit, &root, &touched, edit.clone()).await {
            Some(Repaired::Refuse(message)) => {
                assert!(message.contains("`ledger.py`"), "{message}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        touched.note(&root, Path::new("ledger.py"));
        let (call, _) = args_of(run(Kind::Edit, &root, &touched, edit).await);
        assert_eq!(call["path"], "ledger.py");
    }

    #[tokio::test]
    async fn repair_leaves_a_far_or_existing_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        tokio::fs::create_dir_all(root.join("a")).await.unwrap();
        tokio::fs::create_dir_all(root.join("b")).await.unwrap();
        tokio::fs::write(root.join("a/x.py"), "x\n").await.unwrap();
        tokio::fs::write(root.join("b/x.py"), "x\n").await.unwrap();
        let touched = Touched::default();
        let far = serde_json::json!({"path": "/nope/x.py"});
        assert_eq!(
            run(Kind::Read, &root, &touched, far).await,
            Some(Repaired::Keep)
        );
        let real = serde_json::json!({"path": root.join("a/x.py").to_string_lossy()});
        assert_eq!(
            run(Kind::Read, &root, &touched, real).await,
            Some(Repaired::Keep)
        );
        let write = serde_json::json!({"path": "/nope/x.py", "content": "x"});
        assert!(run(Kind::Write, &root, &touched, write).await.is_none());
    }

    #[tokio::test]
    async fn repair_through_the_batch_fills_a_path_after_a_read() {
        use reagent_agent::batch::run_all_guarded_seen;
        use reagent_agent::tool::Registry;
        use reagent_ai::{Message, Text, ToolCall};
        use tokio_util::sync::CancellationToken;

        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("a.rs"), "fn a() {}\n")
            .await
            .unwrap();
        let touched = Touched::default();
        let mut registry = Registry::new();
        registry.add(crate::read::Read::new(tmp.path()).with_touched(touched.clone()));
        registry.add(crate::edit::Edit::new(tmp.path()).with_touched(touched));
        let call = |id: &str, name: &str, args: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: Text::from(args),
        };
        let token = CancellationToken::new();
        run_all_guarded_seen(
            &[call("c1", "read", r#"{"file_path":"a.rs"}"#)],
            &registry,
            None,
            &token,
        )
        .await;
        let edit = call(
            "c2",
            "edit",
            r#"{"edits":[{"old_string":"fn a()","new_string":"fn b()"}]}"#,
        );
        let results = run_all_guarded_seen(&[edit], &registry, None, &token).await;
        let Message::ToolResult(result) = &results[0].0 else {
            panic!("expected a tool result");
        };
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .starts_with("[repaired: path missing; used a.rs]\n"),
            "{}",
            result.content
        );
        let text = tokio::fs::read_to_string(tmp.path().join("a.rs"))
            .await
            .unwrap();
        assert_eq!(text, "fn b() {}\n");
    }

    #[tokio::test]
    async fn repair_refuses_edits_that_name_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        for call in [
            serde_json::json!({"path": "a.rs", "edits": [{"path": "b.rs", "old_string": "x", "new_string": "y"}]}),
            serde_json::json!({"edits": [{"path": "a.rs", "old_string": "x", "new_string": "y"}, {"path": "b.rs", "old_string": "z", "new_string": "w"}]}),
        ] {
            match run(Kind::Edit, tmp.path(), &touched, call).await {
                Some(Repaired::Refuse(message)) => {
                    assert!(message.contains("one edit call per file"), "{message}")
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        let same = serde_json::json!({"path": "a.rs", "edits": [{"path": "a.rs", "old_string": "x", "new_string": "y"}]});
        let (call, _) = args_of(run(Kind::Edit, tmp.path(), &touched, same).await);
        assert_eq!(
            call,
            serde_json::json!({"path": "a.rs", "edits": [{"old_string": "x", "new_string": "y"}]})
        );
    }

    #[tokio::test]
    async fn repair_reads_a_spaced_string_line_in_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let touched = Touched::default();
        let args =
            r#"{"path": "a.rs", "edits": [{"old_string": "x", "new_string": "y", "line": "4"}]}"#;
        let future = repair(Kind::Edit, tmp.path(), &touched, args).expect("a repair");
        let (call, _) = args_of(Some(future.await));
        assert_eq!(call["edits"][0]["line"], 4);
    }

    #[tokio::test]
    async fn repair_refuses_two_tails_and_parent_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("w");
        tokio::fs::create_dir_all(root.join("s")).await.unwrap();
        tokio::fs::write(root.join("x.py"), "x\n").await.unwrap();
        tokio::fs::write(root.join("s/x.py"), "x\n").await.unwrap();
        tokio::fs::write(tmp.path().join("secret.txt"), "k\n")
            .await
            .unwrap();
        let touched = Touched::default();
        // `s/x.py` and `x.py` both fit a mistyped `.../v/s/x.py`.
        let two = serde_json::json!({"path": tmp.path().join("v/s/x.py").to_string_lossy()});
        assert_eq!(
            run(Kind::Read, &root, &touched, two).await,
            Some(Repaired::Keep)
        );
        // A tail with `..` would leave the folder.
        let up = serde_json::json!({"path": format!("{}/v/../secret.txt", tmp.path().display())});
        assert_eq!(
            run(Kind::Read, &root, &touched, up).await,
            Some(Repaired::Keep)
        );
    }

    #[test]
    fn repair_inside_meets_the_private_prefix_and_no_neighbor() {
        let root = Path::new("/var/x/w");
        assert_eq!(
            inside(root, Path::new("/private/var/x/w/a.rs")),
            Some("a.rs")
        );
        assert_eq!(
            inside(Path::new("/private/var/x/w"), Path::new("/var/x/w/a.rs")),
            Some("a.rs")
        );
        assert_eq!(inside(root, Path::new("/var/x/wq/a.rs")), None);
        assert_eq!(inside(root, Path::new("/var/x/w")), None);
    }

    #[test]
    fn repair_touched_keeps_one_key_per_file() {
        let touched = Touched::default();
        let root = Path::new("/var/x/w");
        touched.note(root, Path::new("a.rs"));
        touched.note(root, Path::new("/private/var/x/w/a.rs"));
        touched.note(root, Path::new("/var/x/w/a.rs"));
        assert_eq!(touched.snapshot(), [PathBuf::from("a.rs")]);
    }

    #[test]
    fn repair_typos_counts_changed_bytes() {
        assert_eq!(typos("/tmp/w-abc123/work", "/tmp/w-abd123/work"), 1);
        assert_eq!(typos("abc", "abc"), 0);
        assert!(typos("/a/very/different/place", "/tmp/w") > MAX_TYPOS);
    }
}
