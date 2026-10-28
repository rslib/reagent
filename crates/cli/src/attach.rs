//! The `@path` words of a task: the image files they name, prepared for the
//! first user message.

use std::path::{Path, PathBuf};

use reagent_ai::Content;

/// The blocks an `@path` word adds after the task text: one note line and one
/// image per image file the text names, in the order the text names them.
///
/// A word that names no file, or a file that is not an image, adds nothing: the
/// text keeps the word and the run goes on.
pub async fn blocks(cwd: &Path, text: &str) -> Result<Vec<Content>, String> {
    let words: Vec<String> = mentions(text).into_iter().map(str::to_string).collect();
    if words.is_empty() {
        return Ok(Vec::new());
    }
    let cwd = cwd.to_path_buf();
    let home = home();
    // The images of one task are read and prepared in one job, off the runtime
    // thread: every file and every decoder runs there.
    tokio::task::spawn_blocking(move || read_all(&cwd, home.as_deref(), &words))
        .await
        .map_err(|error| format!("could not read the attached images: {error}"))?
}

/// The most images one task attaches. Each is at most about 4.5 MB of
/// base64, so the first user message stays well under the 64 MiB a session
/// record holds.
const MAX_IMAGES: usize = 8;

/// Reads and prepares every image `words` names, in order. A file named twice
/// is attached once.
fn read_all(cwd: &Path, home: Option<&Path>, words: &[String]) -> Result<Vec<Content>, String> {
    let mut blocks = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for word in words {
        let path = resolve(cwd, home, word);
        if seen.contains(&path) {
            continue;
        }
        let Some((image, note)) = reagent_tools::read::image_file(&path)? else {
            continue;
        };
        if seen.len() == MAX_IMAGES {
            return Err(format!(
                "a task attaches at most {MAX_IMAGES} images; {} is one more",
                path.display()
            ));
        }
        seen.push(path);
        blocks.push(Content::text(note));
        blocks.push(Content::Image(image));
    }
    Ok(blocks)
}

/// The path words of `text`, in order. A `@` counts at the start of the text or
/// after whitespace, so a mail address keeps its `@`. The path runs to the next
/// whitespace, or to the closing quote of `@"..."`, and loses trailing
/// punctuation such as the `.` that ends a sentence.
fn mentions(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find('@') {
        let at = from + offset;
        let after = &text[at + 1..];
        if at > 0 && !text.as_bytes()[at - 1].is_ascii_whitespace() {
            from = at + 1;
            continue;
        }
        if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => {
                    if end > 0 {
                        out.push(&quoted[..end]);
                    }
                    from = at + 3 + end;
                }
                // An unclosed quote names no file.
                None => from = at + 1,
            }
            continue;
        }
        let len = after.find(char::is_whitespace).unwrap_or(after.len());
        let word = after[..len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        if !word.is_empty() {
            out.push(word);
        }
        from = at + 1 + len;
    }
    out
}

/// The file a word names: absolute as written, `~/` from the home folder, and
/// anything else from the working folder.
fn resolve(cwd: &Path, home: Option<&Path>, word: &str) -> PathBuf {
    if let Some(rest) = word.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    let path = Path::new(word);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The home folder, for a `~/` path.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_word_is_a_path() {
        assert_eq!(mentions("why is this cut off? @shot.png"), ["shot.png"]);
        assert_eq!(mentions("@shot.png"), ["shot.png"]);
        assert_eq!(mentions("a @one.png and @two.jpg"), ["one.png", "two.jpg"]);
    }

    #[test]
    fn a_quoted_path_may_hold_spaces() {
        assert_eq!(mentions(r#"@"my shot.png""#), ["my shot.png"]);
        assert_eq!(
            mentions(r#"look @"dir/my shot.png" now"#),
            ["dir/my shot.png"]
        );
        // An unclosed quote names nothing.
        assert!(mentions(r#"@"my shot.png"#).is_empty());
    }

    #[test]
    fn a_mail_address_is_not_a_path() {
        assert!(mentions("mail me@host.com").is_empty());
        assert_eq!(mentions("mail me@host.com about @notes.txt"), ["notes.txt"]);
    }

    #[test]
    fn trailing_punctuation_is_cut() {
        assert_eq!(mentions("see @shot.png."), ["shot.png"]);
        assert_eq!(mentions("see @shot.png)"), ["shot.png"]);
        assert_eq!(mentions("is it @shot.png?"), ["shot.png"]);
        assert!(mentions("@").is_empty());
    }

    #[tokio::test]
    async fn a_missing_file_stays_text() {
        let tmp = tempfile::tempdir().unwrap();
        let added = blocks(tmp.path(), "look at @nope.png").await.unwrap();
        assert!(added.is_empty());
    }

    #[tokio::test]
    async fn a_text_file_stays_text() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("notes.txt"), "notes\n")
            .await
            .unwrap();
        let added = blocks(tmp.path(), "read @notes.txt").await.unwrap();
        assert!(added.is_empty());
    }

    #[tokio::test]
    async fn a_text_file_that_starts_with_bm_stays_text() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(
            tmp.path().join("bmw.txt"),
            "BMW service notes: oil change due\n",
        )
        .await
        .unwrap();
        let added = blocks(tmp.path(), "see @bmw.txt").await.unwrap();
        assert!(added.is_empty());
    }

    /// The 64x48 PNG the tools tests use.
    async fn shot(tmp: &tempfile::TempDir, name: &str) {
        let png = include_bytes!("../../tools/tests/fixtures/shot.png");
        tokio::fs::write(tmp.path().join(name), png).await.unwrap();
    }

    #[tokio::test]
    async fn a_file_named_twice_is_attached_once() {
        let tmp = tempfile::tempdir().unwrap();
        shot(&tmp, "a.png").await;
        let added = blocks(tmp.path(), "@a.png and again @a.png").await.unwrap();
        assert_eq!(added.len(), 2, "one note and one image");
        assert!(matches!(added[1], Content::Image(_)));
    }

    #[tokio::test]
    async fn a_ninth_image_is_an_error_that_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut task = String::new();
        for index in 0..=MAX_IMAGES {
            shot(&tmp, &format!("{index}.png")).await;
            task.push_str(&format!(" @{index}.png"));
        }
        let error = blocks(tmp.path(), &task).await.unwrap_err();
        assert!(error.contains("at most 8 images"), "{error}");
        assert!(error.contains("8.png"), "{error}");
        let eight = task.rsplit_once(" @").unwrap().0.to_string();
        assert_eq!(blocks(tmp.path(), &eight).await.unwrap().len(), 16);
    }
}
