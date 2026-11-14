//! The skills a run finds, listed in the system prompt so the model can read one.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reagent_util::text;

use crate::prompt;

/// The most bytes of skill text added to the system prompt.
const SKILLS_LIMIT: usize = 24 * 1024;

/// The line that ends the skills text when the limit cut it.
const CUT_NOTE: &str = "[skills list cut at 24 KB]";

/// The most bytes read from the head of a skill file.
const HEAD_BYTES: u64 = 8 * 1024;

/// The most bytes of a skill description.
const DESCRIPTION_LIMIT: usize = 1024;

/// The opening of the skills text, appended after the instructions. The folder
/// list and one line per skill follow it.
const HEADER: &str = "\n\n# Skills\n\n\
     These skills hold instructions for specific tasks. When a task matches a skill's \
     description, read the skill's file with the read tool first, then follow it. \
     A skill marked [n] is at <folder n>/<name>/SKILL.md, or at <folder n>/<dir>/SKILL.md \
     when marked [n/dir]; resolve a relative path in it against that folder.\n\n\
     Skill folders:\n";

/// The name of a skill folder's file, in the Agent Skills format.
const FILE: &str = "SKILL.md";

/// The folder of `.agents/skills` under a repo folder.
const AGENTS_SKILLS: &str = ".agents/skills";

/// The skills of a run, as one blocking lookup finds them.
///
/// The folders are, in the order they are read: `global`'s `skills` folder (the global
/// config folder), then `.agents/skills` of each folder from
/// `repo_root` down to `cwd`. With no `repo_root`, only `cwd` is read. A folder nearer the
/// work folder wins on a name clash. A skill is a direct subfolder holding a `SKILL.md`
/// with front matter naming a description; symlinks are followed and a broken one is
/// skipped. The list is in name order and stops at 24 KB. No skill leaves both fields
/// empty.
pub async fn find(global: Option<&Path>, repo_root: Option<&Path>, cwd: &Path) -> Listing {
    let global = global.map(Path::to_path_buf);
    let repo_root = repo_root.map(Path::to_path_buf);
    // The repo root is absolute, so a relative --cwd must be too to sit below it.
    let cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    tokio::task::spawn_blocking(move || collect(global.as_deref(), repo_root.as_deref(), &cwd))
        .await
        .unwrap_or_default()
}

/// The skills text of a run, as the system prompt appends it.
#[cfg(test)]
pub async fn prompt_text(global: Option<&Path>, repo_root: Option<&Path>, cwd: &Path) -> String {
    find(global, repo_root, cwd).await.prompt
}

/// What one lookup of a run's skills returns.
#[derive(Debug, Default)]
pub struct Listing {
    /// The text appended to the system prompt, empty when no skill was found.
    pub prompt: String,
    /// The skills the text lists, in name order.
    pub found: Vec<Found>,
}

/// One skill as the text lists it, for a client that shows the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The skill's name.
    pub name: Arc<str>,
    /// The absolute path of the skill's `SKILL.md`.
    pub location: Arc<str>,
    /// The folder it was found in.
    pub scope: Scope,
}

/// The folder a skill was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The `skills` folder of the global config folder.
    Global,
    /// A `.agents/skills` folder of the project.
    Project,
}

impl Scope {
    /// The word a client shows, and the `initialize` answer names.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

/// One skill found on disk, ready for the list.
struct Skill {
    description: String,
    location: PathBuf,
    scope: Scope,
    /// The place of its skills folder among the run's folders.
    folder: usize,
    /// The name of its own folder, which the line names when it is not the
    /// skill's name.
    dir: String,
}

fn collect(global: Option<&Path>, repo_root: Option<&Path>, cwd: &Path) -> Listing {
    let dirs = folders(global, repo_root, cwd);
    let mut skills: BTreeMap<String, Skill> = BTreeMap::new();
    for (index, (dir, scope)) in dirs.iter().enumerate() {
        scan(dir, *scope, index, &mut skills);
    }
    let mut listing = Listing::default();
    if skills.is_empty() {
        return listing;
    }
    // Only the folders a skill comes from are named, numbered from 1 in the
    // order they are read.
    let mut used: Vec<usize> = skills.values().map(|skill| skill.folder).collect();
    used.sort_unstable();
    used.dedup();
    let mut out = String::from(HEADER);
    for (number, &folder) in used.iter().enumerate() {
        let _ = writeln!(out, "[{}] {}", number + 1, dirs[folder].0.display());
    }
    out.push('\n');
    let mut cut = false;
    for (name, skill) in &skills {
        let number = used
            .iter()
            .position(|&folder| folder == skill.folder)
            .unwrap_or(0)
            + 1;
        let line = line(name, skill, number);
        if out.len() + line.len() > SKILLS_LIMIT - CUT_NOTE.len() {
            cut = true;
            break;
        }
        out.push_str(&line);
        listing.found.push(Found {
            name: Arc::from(name.as_str()),
            location: Arc::from(skill.location.display().to_string().as_str()),
            scope: skill.scope,
        });
    }
    if cut {
        out.push_str(CUT_NOTE);
    } else {
        // The text ends on the last line, as the instructions before it do.
        out.pop();
    }
    listing.prompt = out;
    listing
}

/// The skill folders of a run and the scope of each, in the order they are read;
/// a later one wins a name clash.
fn folders(global: Option<&Path>, repo_root: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, Scope)> {
    let mut dirs = Vec::new();
    if let Some(global) = global {
        dirs.push((global.join("skills"), Scope::Global));
    }
    let below = repo_root.and_then(|root| prompt::folders_below(root, cwd));
    for folder in below.unwrap_or_else(|| vec![cwd.to_path_buf()]) {
        dirs.push((folder.join(AGENTS_SKILLS), Scope::Project));
    }
    dirs
}

/// Adds the skills of `dir` to `into`, replacing a nearer name's skill.
#[expect(clippy::disallowed_methods, reason = "runs inside spawn_blocking")]
fn scan(dir: &Path, scope: Scope, folder_index: usize, into: &mut BTreeMap<String, Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let folder = entry.file_name();
        let folder = folder.to_string_lossy();
        if folder.starts_with('.') {
            continue;
        }
        let location = entry.path().join(FILE);
        let Some(front) = front_matter(&location) else {
            continue;
        };
        let description = front.description.unwrap_or_default();
        let description = text::head(description.trim(), DESCRIPTION_LIMIT);
        if description.is_empty() {
            continue;
        }
        let name = match front.name {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => folder.to_string(),
        };
        into.insert(
            name,
            Skill {
                description: description.split_whitespace().collect::<Vec<_>>().join(" "),
                location,
                scope,
                folder: folder_index,
                dir: folder.to_string(),
            },
        );
    }
}

/// The front matter fields this module reads.
#[derive(Default)]
struct Front {
    name: Option<String>,
    description: Option<String>,
}

/// Reads the front matter of a skill file, or `None` when it has none.
#[expect(clippy::disallowed_types, reason = "runs inside spawn_blocking")]
fn front_matter(path: &Path) -> Option<Front> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(HEAD_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    if lines.first()? != &"---" {
        return None;
    }
    let mut front = Front::default();
    let mut i = 1;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line == "---" {
            return Some(front);
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key != "name" && key != "description" {
            continue;
        }
        let value = value.trim();
        let value = match value {
            ">" | ">-" | "|" | "|-" => {
                let mut parts = Vec::new();
                while i < lines.len() && (lines[i].starts_with(' ') || lines[i].starts_with('\t')) {
                    parts.push(lines[i].trim());
                    i += 1;
                }
                parts.join(" ")
            }
            value => unquote(value),
        };
        match key {
            "name" => front.name = Some(value),
            _ => front.description = Some(value),
        }
    }
    None
}

/// Strips one pair of matching quotes around a value.
fn unquote(value: &str) -> String {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].trim().to_string();
        }
    }
    value.trim().to_string()
}

/// One skill's line of the list: its name, the number of its folder, and its
/// own folder when that is not its name.
fn line(name: &str, skill: &Skill, number: usize) -> String {
    if skill.dir == name {
        format!("- {name} [{number}]: {}\n", skill.description)
    } else {
        format!("- {name} [{number}/{}]: {}\n", skill.dir, skill.description)
    }
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
mod tests {
    use super::*;

    const CUT: &str = "[skills list cut at 24 KB]";

    /// Whether the skills text lists the skill `name`.
    fn listed(text: &str, name: &str) -> bool {
        text.contains(&format!("\n- {name} ["))
    }

    struct Dir(tempfile::TempDir);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("reagent-skills-{name}-"))
                .tempdir()
                .expect("makes a temp folder");
            Self(dir)
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    fn skill(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n")
    }

    #[tokio::test]
    async fn a_nearer_skill_of_the_same_name_wins() {
        let d = Dir::new("clash");
        d.write("global/skills/tidy/SKILL.md", &skill("tidy", "global tidy"));
        let repo = d.path().join("repo");
        d.write(
            "repo/.agents/skills/tidy/SKILL.md",
            &skill("tidy", "project tidy"),
        );
        let got = prompt_text(Some(&d.path().join("global")), Some(&repo), &repo).await;
        assert_eq!(got.matches("\n- tidy [").count(), 1, "{got}");
        assert!(got.contains("project tidy"), "{got}");
        assert!(!got.contains("global tidy"), "{got}");
        assert!(
            got.contains(&format!("\n[1] {}/.agents/skills\n", repo.display())),
            "{got}"
        );
        assert!(got.contains("\n- tidy [1]: project tidy"), "{got}");
    }

    #[tokio::test]
    async fn the_work_folder_skills_are_listed_and_a_sibling_is_not() {
        let d = Dir::new("walk");
        d.write(
            "repo/.agents/skills/root-one/SKILL.md",
            &skill("root-one", "at the root"),
        );
        d.write(
            "repo/sub/.agents/skills/here/SKILL.md",
            &skill("here", "in the work folder"),
        );
        d.write(
            "repo/sibling/.agents/skills/there/SKILL.md",
            &skill("there", "in a sibling"),
        );
        let repo = d.path().join("repo");
        let got = prompt_text(None, Some(&repo), &repo.join("sub")).await;
        assert!(listed(&got, "here"), "{got}");
        assert!(listed(&got, "root-one"), "{got}");
        assert!(!got.contains("there"), "{got}");
    }

    #[tokio::test]
    async fn a_skill_without_a_description_or_front_matter_is_skipped() {
        let d = Dir::new("skip");
        d.write("skills/no-desc/SKILL.md", "---\nname: no-desc\n---\n");
        d.write(
            "skills/empty-desc/SKILL.md",
            "---\nname: empty-desc\ndescription: \"\"\n---\n",
        );
        d.write("skills/no-front/SKILL.md", "just text\n");
        d.write("skills/real/SKILL.md", &skill("real", "a real skill"));
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        assert!(listed(&got, "real"), "{got}");
        assert!(!got.contains("no-desc"), "{got}");
        assert!(!got.contains("empty-desc"), "{got}");
        assert!(!got.contains("no-front"), "{got}");
    }

    #[tokio::test]
    async fn a_skill_without_a_name_uses_its_folder_name() {
        let d = Dir::new("fallback");
        d.write(
            "skills/my-folder/SKILL.md",
            "---\ndescription: no name given\n---\n",
        );
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        assert!(listed(&got, "my-folder"), "{got}");
        assert!(got.contains("no name given"), "{got}");
    }

    #[tokio::test]
    async fn a_quoted_value_and_a_block_description_parse() {
        let d = Dir::new("yaml");
        d.write(
            "skills/quoted/SKILL.md",
            "---\nname: \"a quoted name\"\ndescription: >-\n  folded line one\n  folded line two\n---\n",
        );
        d.write(
            "skills/literal/SKILL.md",
            "---\nname: 'single'\ndescription: |\n  kept line one\n  kept line two\n---\n",
        );
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        assert!(listed(&got, "a quoted name"), "{got}");
        assert!(
            got.contains("- a quoted name [1/quoted]: folded line one folded line two"),
            "{got}"
        );
        assert!(listed(&got, "single"), "{got}");
        assert!(
            got.contains("- single [1/literal]: kept line one kept line two"),
            "{got}"
        );
    }

    #[tokio::test]
    async fn a_description_is_written_as_it_is() {
        let d = Dir::new("plain");
        d.write(
            "skills/esc/SKILL.md",
            &skill("esc", "a < b & c > d \"e\" 'f'"),
        );
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        assert!(
            got.contains("\n- esc [1]: a < b & c > d \"e\" 'f'"),
            "{got}"
        );
    }

    #[tokio::test]
    async fn the_skills_list_numbers_only_the_folders_it_uses() {
        let d = Dir::new("folders");
        d.write("global/skills/tidy/SKILL.md", &skill("tidy", "global tidy"));
        let repo = d.path().join("repo");
        d.write(
            "repo/sub/.agents/skills/here/SKILL.md",
            &skill("here", "in the work folder"),
        );
        let got = prompt_text(
            Some(&d.path().join("global")),
            Some(&repo),
            &repo.join("sub"),
        )
        .await;
        assert!(
            got.contains(&format!(
                "Skill folders:\n[1] {}/global/skills\n[2] {}/sub/.agents/skills\n\n",
                d.path().display(),
                repo.display()
            )),
            "{got}"
        );
        assert!(got.contains("\n- here [2]: in the work folder"), "{got}");
        assert!(got.contains("\n- tidy [1]: global tidy"), "{got}");
        assert!(
            !got.contains(&format!("{}/.agents/skills\n", repo.display())),
            "{got}"
        );
    }

    #[tokio::test]
    async fn no_skills_folder_adds_nothing() {
        let d = Dir::new("none");
        let got = prompt_text(None, None, d.path()).await;
        assert_eq!(got, "");
    }

    #[tokio::test]
    async fn the_skills_list_is_cut_at_24_kb() {
        let d = Dir::new("long");
        let description = "x".repeat(300);
        for i in 0..100 {
            d.write(
                &format!("skills/s{i:03}/SKILL.md"),
                &skill(&format!("s{i:03}"), &description),
            );
        }
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        assert!(got.len() <= 24 * 1024, "{}", got.len());
        assert!(
            got.ends_with(&format!("{}\n{CUT}", "x".repeat(300))),
            "{}",
            &got[got.len() - 80..]
        );
    }

    #[tokio::test]
    async fn the_list_is_the_expected_block() {
        let d = Dir::new("block");
        d.write(
            "skills/tidy/SKILL.md",
            &skill("tidy", "keep the tree clean"),
        );
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        let want = format!(
            "\n\n# Skills\n\n\
             These skills hold instructions for specific tasks. When a task matches a skill's \
             description, read the skill's file with the read tool first, then follow it. \
             A skill marked [n] is at <folder n>/<name>/SKILL.md, or at <folder n>/<dir>/SKILL.md \
             when marked [n/dir]; resolve a relative path in it against that folder.\n\n\
             Skill folders:\n[1] {}/skills\n\n- tidy [1]: keep the tree clean",
            d.path().display()
        );
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn the_skills_list_holds_the_name_the_location_and_the_scope() {
        let d = Dir::new("found");
        d.write("global/skills/tidy/SKILL.md", &skill("tidy", "global tidy"));
        let repo = d.path().join("repo");
        d.write(
            "repo/.agents/skills/here/SKILL.md",
            &skill("here", "in the repo"),
        );
        let listing = find(Some(&d.path().join("global")), Some(&repo), &repo).await;
        let here = repo.join(".agents/skills/here/SKILL.md");
        let tidy = d.path().join("global/skills/tidy/SKILL.md");
        assert_eq!(
            listing.found,
            vec![
                Found {
                    name: Arc::from("here"),
                    location: Arc::from(here.display().to_string().as_str()),
                    scope: Scope::Project,
                },
                Found {
                    name: Arc::from("tidy"),
                    location: Arc::from(tidy.display().to_string().as_str()),
                    scope: Scope::Global,
                },
            ]
        );
        assert!(
            here.is_absolute() && tidy.is_absolute(),
            "the locations are absolute paths"
        );
        // The list names the skills the prompt lists, and no other.
        assert!(listed(&listing.prompt, "here"), "{}", listing.prompt);
        assert!(listed(&listing.prompt, "tidy"), "{}", listing.prompt);
    }

    #[tokio::test]
    async fn a_project_skills_skill_wins_over_a_global_one_in_the_list() {
        let d = Dir::new("clash-list");
        d.write("global/skills/tidy/SKILL.md", &skill("tidy", "global tidy"));
        let repo = d.path().join("repo");
        d.write(
            "repo/.agents/skills/tidy/SKILL.md",
            &skill("tidy", "project tidy"),
        );
        let listing = find(Some(&d.path().join("global")), Some(&repo), &repo).await;
        assert_eq!(listing.found.len(), 1, "{:?}", listing.found);
        let found = &listing.found[0];
        assert_eq!(found.name.as_ref(), "tidy");
        assert_eq!(found.scope, Scope::Project);
        let want = repo.join(".agents/skills/tidy/SKILL.md");
        assert_eq!(found.location.as_ref(), want.display().to_string());
    }

    #[tokio::test]
    async fn the_skills_list_is_in_name_order() {
        let d = Dir::new("order-list");
        for name in ["zeta", "alpha", "mike"] {
            d.write(
                &format!("skills/{name}/SKILL.md"),
                &skill(name, &format!("the {name} skill")),
            );
        }
        let listing = find(Some(d.path()), None, d.path()).await;
        let names: Vec<&str> = listing
            .found
            .iter()
            .map(|found| found.name.as_ref())
            .collect();
        assert_eq!(names, ["alpha", "mike", "zeta"]);
    }

    #[tokio::test]
    async fn a_run_with_no_skills_has_an_empty_skills_list() {
        let d = Dir::new("none-list");
        let listing = find(None, None, d.path()).await;
        assert!(listing.found.is_empty(), "{:?}", listing.found);
        assert_eq!(listing.prompt, "");
    }

    #[tokio::test]
    async fn the_skills_list_stops_where_the_prompt_text_is_cut() {
        let d = Dir::new("cut-list");
        let description = "x".repeat(300);
        for i in 0..100 {
            d.write(
                &format!("skills/s{i:03}/SKILL.md"),
                &skill(&format!("s{i:03}"), &description),
            );
        }
        let listing = find(Some(d.path()), None, d.path()).await;
        assert!(
            listing.found.len() < 100,
            "a list cut at 24 KB is shorter: {}",
            listing.found.len()
        );
        for found in &listing.found {
            assert!(
                listed(&listing.prompt, &found.name),
                "{} is not in the text",
                found.name
            );
        }
        assert!(!listed(&listing.prompt, "s099"), "the tail is cut");
    }

    #[tokio::test]
    async fn the_skills_are_listed_in_name_order() {
        let d = Dir::new("order");
        for name in ["zeta", "alpha", "mike"] {
            d.write(
                &format!("skills/{name}/SKILL.md"),
                &skill(name, &format!("the {name} skill")),
            );
        }
        let got = prompt_text(Some(d.path()), None, d.path()).await;
        let alpha = got.find("\n- alpha [").expect("alpha");
        let mike = got.find("\n- mike [").expect("mike");
        let zeta = got.find("\n- zeta [").expect("zeta");
        assert!(alpha < mike && mike < zeta, "{got}");
    }
}
