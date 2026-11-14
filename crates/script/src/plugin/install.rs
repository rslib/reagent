//! Installs a plugin from npm (through an esm.sh-style CDN) or from a git
//! repository, with its lockfile.
//!
//! `add` resolves the package's exact version by the redirect of its
//! `package.json`, fetches the plugin's own files from its entry, then bundles
//! every bare import through the CDN and rewrites the absolute specifiers the
//! bundles name into relative ones, so the folder runs as it was fetched.
//! Everything lands in a staging folder first: the caller prints what it
//! fetched, asks, and commits by moving the folder into place.
//!
//! A `git:` source is staged the same way, from a clone of the repository
//! rather than from the CDN: the `git` module of this install copies the files
//! it tracks into the staging folder and the bare imports of those files go
//! through the CDN here.

mod git;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::LOCATION;
use serde_json::Value as JsonValue;

use super::lock::{self, Lock};

/// The most bytes one response may hold.
const RESPONSE_LIMIT: u64 = 16 * 1024 * 1024;

/// The most bytes every response of one install may hold.
const TOTAL_LIMIT: u64 = 64 * 1024 * 1024;

/// The most files one install may write.
const FILES_LIMIT: usize = 2000;

/// How long one request may take, its body and its redirects included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How many redirects one request may follow.
const REDIRECTS: usize = 10;

/// What a staged install fetched, for the confirm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The exact source, such as `npm:demo@1.0.0`.
    pub source: String,
    /// The exact version the CDN answered with.
    pub version: String,
    /// How many files the install writes.
    pub files: usize,
    /// How many bytes the CDN answered with.
    pub bytes: u64,
    /// The exact version of each dependency whose bundle the imports reached.
    pub dependencies: BTreeMap<String, String>,
    /// `reagent.permissions` as one line of JSON, or `None` when it names none.
    pub permissions: Option<String>,
    /// The full hash of the commit a git source resolved to; `None` for npm.
    pub commit: Option<String>,
}

impl Summary {
    /// The block the confirm prints before its question.
    pub fn text(&self) -> String {
        let dependencies = if self.dependencies.is_empty() {
            "none".to_owned()
        } else {
            self.dependencies
                .iter()
                .map(|(name, version)| format!("{name}@{version}"))
                .collect::<Vec<String>>()
                .join(", ")
        };
        let point = match &self.commit {
            Some(commit) => format!("commit: {}\n", short(commit)),
            None => format!("version: {}\n", self.version),
        };
        format!(
            "source: {}\n{point}files: {} ({} bytes)\ndependencies: {dependencies}\npermissions: {}\n",
            self.source,
            self.files,
            self.bytes,
            self.permissions.as_deref().unwrap_or("none"),
        )
    }

    /// The exact point the install resolved to: the first 12 characters of the
    /// commit of a git source, else its version.
    pub fn revision(&self) -> &str {
        match &self.commit {
            Some(commit) => short(commit),
            None => &self.version,
        }
    }
}

/// The first 12 characters of a commit.
fn short(commit: &str) -> &str {
    commit
        .char_indices()
        .nth(12)
        .map_or(commit, |(at, _)| &commit[..at])
}

/// One install fetched into a staging folder, waiting for the confirm.
#[derive(Debug)]
pub struct Pending {
    /// The folder the files are written into now.
    staging: PathBuf,
    /// The folder they move to.
    folder: PathBuf,
    /// The folder's name under the plugins root.
    pub name: String,
    /// What the install fetched.
    pub summary: Summary,
    /// The lockfile the install wrote into the staging folder.
    lock: Lock,
    /// Whether the folder stands now and is replaced, as an update does.
    replace: bool,
}

impl Pending {
    /// Moves the staging folder into place, replacing the old one for an
    /// update: the old folder is parked under a hidden name until the new one
    /// stands, then deleted.
    pub async fn commit(self) -> Result<(), String> {
        let fail = |error: std::io::Error| format!("{}: {error}", self.folder.display());
        if !self.replace {
            return tokio::fs::rename(&self.staging, &self.folder)
                .await
                .map_err(fail);
        }
        let aside = aside_of(&self.folder);
        let _ = tokio::fs::remove_dir_all(&aside).await;
        tokio::fs::rename(&self.folder, &aside)
            .await
            .map_err(fail)?;
        if let Err(error) = tokio::fs::rename(&self.staging, &self.folder).await {
            let _ = tokio::fs::rename(&aside, &self.folder).await;
            return Err(fail(error));
        }
        let _ = tokio::fs::remove_dir_all(&aside).await;
        Ok(())
    }

    /// Deletes everything the install fetched.
    pub async fn abort(self) {
        let _ = tokio::fs::remove_dir_all(&self.staging).await;
    }
}

/// Where an update parks the folder it replaces.
fn aside_of(folder: &Path) -> PathBuf {
    let name = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    folder.with_file_name(format!(".{name}.old-{}", std::process::id()))
}

/// What an update found.
#[derive(Debug)]
pub enum Staged {
    /// The folder already holds what the CDN serves.
    UpToDate,
    /// Something newer, waiting for the confirm.
    Ready(Box<Update>),
}

/// One update ready to replace a folder.
#[derive(Debug)]
pub struct Update {
    /// The staged install.
    pub pending: Pending,
    /// The version the folder holds now.
    pub old_version: String,
    /// The permissions it holds now, as one line of JSON.
    pub old_permissions: Option<String>,
}

/// Fetches `requested` into a staging folder under `plugins`, ready for the
/// confirm, and refuses when the folder stands already.
pub async fn stage_add(
    client: &reqwest::Client,
    requested: &str,
    plugins: &Path,
) -> Result<Pending, String> {
    if requested.starts_with("git:") {
        return git::stage_add(client, requested, plugins).await;
    }
    let (name, range) = parse_npm(requested)?;
    let folder = folder_of(&name);
    if tokio::fs::symlink_metadata(plugins.join(&folder))
        .await
        .is_ok()
    {
        return Err(format!(
            "{folder} is installed; run reagent plugin update {folder}"
        ));
    }
    stage(client, requested, &name, &range, plugins, &folder, false).await
}

/// Fetches `add`'s source of the plugin in `folder` again, ready for the
/// confirm, or says that the folder already holds it.
pub async fn stage_update(
    client: &reqwest::Client,
    folder: &str,
    plugins: &Path,
) -> Result<Staged, String> {
    check_folder(folder)?;
    let dir = plugins.join(folder);
    let old = tokio::task::spawn_blocking(move || lock::read(&dir))
        .await
        .map_err(|_| "the lockfile task failed".to_string())?
        .map_err(|error| format!("{folder}: {error}"))?;
    let Some(old) = old else {
        return Err(format!(
            "{folder} was not installed by reagent plugin add; delete the folder yourself"
        ));
    };
    if old.requested.starts_with("git:") {
        return git::stage_update(client, folder, &old, plugins).await;
    }
    let (name, range) = parse_npm(&old.requested)?;
    let pending = stage(client, &old.requested, &name, &range, plugins, folder, true).await?;
    if pending.lock.source == old.source && pending.lock.files == old.files {
        pending.abort().await;
        return Ok(Staged::UpToDate);
    }
    let installed = plugins.join(folder);
    let (old_version, old_permissions) = installed_of(&installed).await;
    Ok(Staged::Ready(Box::new(Update {
        pending,
        old_version,
        old_permissions,
    })))
}

/// Fetches one source into `<plugins>/.staging-<folder>-<pid>/`, and writes its
/// lockfile there.
async fn stage(
    client: &reqwest::Client,
    requested: &str,
    name: &str,
    range: &str,
    plugins: &Path,
    folder: &str,
    replace: bool,
) -> Result<Pending, String> {
    let staging = plugins.join(format!(".staging-{folder}-{}", std::process::id()));
    if tokio::fs::symlink_metadata(&staging).await.is_ok() {
        tokio::fs::remove_dir_all(&staging)
            .await
            .map_err(|error| format!("{}: {error}", staging.display()))?;
    }
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|error| format!("{}: {error}", staging.display()))?;
    let built = build(client, requested, name, range, &staging).await;
    let (summary, lock) = match built {
        Ok(built) => built,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
    };
    Ok(Pending {
        staging,
        folder: plugins.join(folder),
        name: folder.to_owned(),
        summary,
        lock,
        replace,
    })
}

/// The version and the permissions of the plugin the folder holds now.
async fn installed_of(folder: &Path) -> (String, Option<String>) {
    let manifest = tokio::fs::read_to_string(folder.join("package.json"))
        .await
        .ok()
        .and_then(|text| serde_json::from_str::<JsonValue>(&text).ok());
    let version = manifest
        .as_ref()
        .and_then(|json| json.get("version"))
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0")
        .to_owned();
    let permissions = manifest
        .as_ref()
        .and_then(|json| json.pointer("/reagent/permissions"))
        .map(one_line);
    (version, permissions)
}

/// `json` as one line of text.
fn one_line(json: &JsonValue) -> String {
    serde_json::to_string(json).unwrap_or_else(|_| "null".to_owned())
}

/// Fetches the source and writes it into `folder`; answers the summary and the
/// lockfile.
async fn build(
    client: &reqwest::Client,
    requested: &str,
    name: &str,
    range: &str,
    folder: &Path,
) -> Result<(Summary, Lock), String> {
    let mut fetcher = Fetcher::new(client, cdn()?);
    let fetched = crawl(&mut fetcher, name, range).await?;
    let lock = write_plan(&fetched.plan, folder, requested, name, &fetched.version).await?;
    let summary = Summary {
        source: lock.source.clone(),
        version: fetched.version.clone(),
        files: lock.files.len(),
        bytes: fetched.plan.bytes,
        dependencies: fetched.plan.dependencies.clone(),
        permissions: fetched.permissions.clone(),
        commit: None,
    };
    Ok((summary, lock))
}

/// What an install needs from a plugin's `package.json`.
struct Plugin {
    /// `name`, when the manifest has one.
    name: Option<String>,
    /// `version`, when the manifest has one.
    version: Option<String>,
    /// `reagent.entry`, the file the plugin loads from.
    entry: String,
    /// `reagent.permissions` as one line of JSON.
    permissions: Option<String>,
    /// The range `dependencies` gives each package it names.
    ranges: BTreeMap<String, String>,
}

impl Plugin {
    /// The manifest in `text`, whose source `label` names in a message.
    fn parse(text: &str, label: &str) -> Result<Self, String> {
        let manifest: JsonValue =
            serde_json::from_str(text).map_err(|error| format!("{label} package.json: {error}"))?;
        let entry = manifest
            .pointer("/reagent/entry")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                format!("{label} is not a reagent plugin: its package.json has no reagent.entry")
            })?
            .to_owned();
        let permissions = manifest.pointer("/reagent/permissions").map(one_line);
        let mut ranges: BTreeMap<String, String> = BTreeMap::new();
        if let Some(table) = manifest.get("dependencies") {
            let object = table
                .as_object()
                .ok_or_else(|| format!("{label}: dependencies is not an object"))?;
            for (dependency, range) in object {
                let range = range
                    .as_str()
                    .ok_or_else(|| format!("{label}: the range of {dependency} is not a string"))?;
                ranges.insert(dependency.clone(), range.to_owned());
            }
        }
        let text = manifest
            .get("name")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let version = manifest
            .get("version")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        Ok(Self {
            name: text,
            version,
            entry,
            permissions,
            ranges,
        })
    }
}

/// Where the files of the plugin itself come from.
enum Own<'a, 'c> {
    /// The CDN, at the exact version the install resolved.
    Cdn {
        /// The install's requests.
        fetcher: &'a mut Fetcher<'c>,
        /// The package name, for the CDN path.
        name: &'a str,
        /// The exact version, for the CDN path.
        version: &'a str,
    },
    /// The tracked files of a git clone, on disk under the plugin's folder.
    Disk(&'a Path),
}

impl Own<'_, '_> {
    /// The text of the file at `path`, which is inside the plugin's folder.
    async fn read(&mut self, path: &str) -> Result<String, String> {
        match self {
            Self::Cdn {
                fetcher,
                name,
                version,
            } => {
                let url = fetcher.cdn.url(&format!("/{name}@{version}/{path}?raw"));
                let (_, text) = fetcher.get(&url).await?;
                Ok(text)
            }
            Self::Disk(dir) => {
                let full = dir.join(path);
                let bytes = tokio::fs::read(&full)
                    .await
                    .map_err(|error| format!("{path}: {error}"))?;
                String::from_utf8(bytes).map_err(|_| format!("{path}: the file is not UTF-8"))
            }
        }
    }
}

/// Everything one install fetched, before it is written.
struct Plan {
    /// The files to write, by their path under the plugin's folder.
    files: Vec<(String, String)>,
    /// The file each bare import name loads.
    imports: BTreeMap<String, String>,
    /// The exact version of each dependency the imports reached.
    dependencies: BTreeMap<String, String>,
    /// The saved path of each CDN path a bundle named, without its query.
    known: BTreeMap<String, String>,
    /// The bytes the CDN answered with.
    bytes: u64,
}

/// What one crawl fetched.
struct Fetched {
    /// The files, their imports and their dependencies.
    plan: Plan,
    /// The exact version the CDN resolved.
    version: String,
    /// `reagent.permissions` of the package, as one line of JSON.
    permissions: Option<String>,
}

/// Fetches the npm package `name` at `range` with every file its imports reach.
async fn crawl(fetcher: &mut Fetcher<'_>, name: &str, range: &str) -> Result<Fetched, String> {
    let url = fetcher
        .cdn
        .url(&format!("/{name}@{range}/package.json?raw"));
    let (final_url, raw) = fetcher.get(&url).await?;
    let version =
        version_of(&final_url, name).ok_or_else(|| format!("{url} names no version of {name}"))?;
    let label = format!("{name}@{version}");
    let plugin = Plugin::parse(&raw, &label)?;
    let mut plan = Plan {
        files: Vec::new(),
        imports: BTreeMap::new(),
        dependencies: BTreeMap::new(),
        known: BTreeMap::new(),
        bytes: raw.len() as u64,
    };
    plan.files.push(("package.json".to_owned(), raw));
    // The plugin's own files: the entry and every relative import under it.
    let (files, bare, bytes) = {
        let mut own = Own::Cdn {
            fetcher,
            name,
            version: &version,
        };
        own_files(&mut own, &plugin.entry, &label, plan.files.len()).await?
    };
    plan.files.extend(files);
    plan.bytes += bytes;
    crawl_bare(fetcher, &mut plan, &label, &plugin.ranges, bare).await?;
    Ok(Fetched {
        plan,
        version,
        permissions: plugin.permissions,
    })
}

/// The files of the plugin itself, from its `entry` and every relative import
/// under it, and the bare imports they name.
///
/// `used` is how many files the install holds already; the answer ends with the
/// bytes these files hold.
async fn own_files(
    own: &mut Own<'_, '_>,
    entry: &str,
    label: &str,
    used: usize,
) -> Result<(Vec<(String, String)>, BTreeSet<String>, u64), String> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut queue = VecDeque::from([resolve_relative("", entry)?]);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut bare: BTreeSet<String> = BTreeSet::new();
    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        if used + files.len() >= FILES_LIMIT {
            return Err(format!("{label} holds more than {FILES_LIMIT} files"));
        }
        let text = own.read(&path).await?;
        bytes += text.len() as u64;
        for spec in specifiers(&text) {
            let specifier = spec.text(&text);
            if is_relative(specifier) {
                let target = resolve_relative(&path, specifier)?;
                if !seen.contains(&target) {
                    queue.push_back(target);
                }
            } else if !specifier.starts_with("node:") {
                bare.insert(specifier.to_owned());
            }
        }
        files.push((path, text));
    }
    Ok((files, bare, bytes))
}

/// Bundles every bare import `bare` names through the CDN, with the files each
/// bundle names, into `plan`.
async fn crawl_bare(
    fetcher: &mut Fetcher<'_>,
    plan: &mut Plan,
    label: &str,
    ranges: &BTreeMap<String, String>,
    bare: BTreeSet<String>,
) -> Result<(), String> {
    for specifier in bare {
        let (dependency, subpath) = split_bare(&specifier);
        let range = ranges.get(dependency).ok_or_else(|| {
            format!(
                "{specifier} is not a dependency of {label}: add {dependency} to its dependencies"
            )
        })?;
        let path = match subpath {
            Some(subpath) => format!("/{dependency}@{range}/{subpath}?bundle&target=node"),
            None => format!("/{dependency}@{range}?bundle&target=node"),
        };
        let url = fetcher.cdn.url(&path);
        let (final_url, stub) = fetcher.get(&url).await?;
        let exact = exact_version(&final_url, &stub, dependency, &fetcher.cdn.origin)
            .ok_or_else(|| format!("{url} names no version of {dependency}"))?;
        plan.dependencies
            .entry(dependency.to_owned())
            .or_insert_with(|| exact.clone());
        let saved = dep_saved(&format!(
            "/{dependency}@{exact}/{}.entry.mjs",
            subpath.unwrap_or("index")
        ))?;
        plan.imports.insert(specifier, saved.clone());
        plan.bytes += stub.len() as u64;
        crawl_deps(fetcher, plan, saved, stub).await?;
    }
    Ok(())
}

/// Saves the bundle `text` at `saved`, with every absolute specifier it names
/// fetched into `deps/` and rewritten to the relative path of that file.
async fn crawl_deps(
    fetcher: &mut Fetcher<'_>,
    plan: &mut Plan,
    saved: String,
    text: String,
) -> Result<(), String> {
    let mut work: Vec<(String, String)> = vec![(saved, text)];
    while let Some((saved, text)) = work.pop() {
        if plan.files.len() >= FILES_LIMIT {
            return Err(format!("the plugin holds more than {FILES_LIMIT} files"));
        }
        let mut replace: BTreeMap<String, String> = BTreeMap::new();
        for spec in specifiers(&text) {
            let specifier = spec.text(&text).to_owned();
            let Some(path) = cdn_path(&specifier, &fetcher.cdn.origin) else {
                if specifier.starts_with("http://") || specifier.starts_with("https://") {
                    return Err(format!(
                        "{saved}: the import {specifier} names another origin"
                    ));
                }
                continue;
            };
            let key = strip_query(&path).to_owned();
            let child = match plan.known.get(&key) {
                Some(child) => child.clone(),
                None => {
                    let child = dep_saved(&path)?;
                    let url = fetcher.cdn.url(&path);
                    let (_, body) = fetcher.get(&url).await?;
                    plan.bytes += body.len() as u64;
                    plan.known.insert(key, child.clone());
                    work.push((child.clone(), body));
                    child
                }
            };
            replace.insert(specifier, relative_to(&saved, &child));
        }
        plan.files.push((saved, rewrite(&text, &replace)));
    }
    Ok(())
}

/// Writes every file of `plan` into `folder` and answers the lockfile that pins
/// them.
async fn write_plan(
    plan: &Plan,
    folder: &Path,
    requested: &str,
    name: &str,
    version: &str,
) -> Result<Lock, String> {
    let lock = Lock {
        version: lock::VERSION,
        requested: requested.to_owned(),
        source: format!("npm:{name}@{version}"),
        commit: None,
        files: write_files(&plan.files, folder).await?,
        imports: plan.imports.clone(),
    };
    write_lock(folder, &lock).await?;
    Ok(lock)
}

/// Writes `files` into `folder` and answers the sha256 of each, by its path.
async fn write_files(
    files: &[(String, String)],
    folder: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let mut hashes = BTreeMap::new();
    for (path, text) in files {
        if !lock::inside(path) {
            return Err(format!("the saved path {path:?} leaves the plugin folder"));
        }
        let full = folder.join(path);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("{path}: {error}"))?;
        }
        tokio::fs::write(&full, text)
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        hashes.insert(path.clone(), lock::hash(text.as_bytes()));
    }
    Ok(hashes)
}

/// Writes `lock` into `folder`, off the async runtime.
async fn write_lock(folder: &Path, lock: &Lock) -> Result<(), String> {
    let dir = folder.to_path_buf();
    let written = lock.clone();
    tokio::task::spawn_blocking(move || lock::write(&dir, &written))
        .await
        .map_err(|_| "the lockfile task failed".to_string())?
        .map_err(|error| format!("{}: {error}", lock::FILE))
}

/// The CDN an install fetches from.
struct Cdn {
    /// The base, without a trailing `/`.
    text: String,
    /// The base as a URL, whose origin a redirect may not leave.
    origin: Url,
}

impl Cdn {
    /// The URL of the CDN path `path`, which starts with `/`.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.text)
    }
}

/// One install's requests, with the byte budget they share.
struct Fetcher<'a> {
    /// The client the caller built, which follows no redirect itself.
    client: &'a reqwest::Client,
    /// The CDN the requests go to.
    cdn: Cdn,
    /// The bytes every response held so far.
    used: u64,
}

impl<'a> Fetcher<'a> {
    /// A fetcher over `base`.
    fn new(client: &'a reqwest::Client, base: Url) -> Self {
        let text = base.as_str().trim_end_matches('/').to_owned();
        Self {
            client,
            cdn: Cdn { text, origin: base },
            used: 0,
        }
    }

    /// GETs `url`, following a redirect while it stays on the CDN, and answers
    /// the URL it ends on with the body.
    async fn get(&mut self, url: &str) -> Result<(Url, String), String> {
        let mut current = Url::parse(url).map_err(|error| format!("{url}: {error}"))?;
        for _ in 0..=REDIRECTS {
            let (status, location, body) = self.once(&current).await?;
            if status.is_redirection() {
                let location =
                    location.ok_or_else(|| format!("{current}: a redirect with no location"))?;
                let next = current
                    .join(&location)
                    .map_err(|error| format!("{current}: {location}: {error}"))?;
                if next.origin() != self.cdn.origin.origin() {
                    return Err(format!(
                        "{current} redirects to {next}, which is not the plugin CDN"
                    ));
                }
                current = next;
                continue;
            }
            if !status.is_success() {
                return Err(format!("{url}: {status}"));
            }
            return Ok((current, body));
        }
        Err(format!("{url}: more than {REDIRECTS} redirects"))
    }

    /// One request: its status, the location it names and its body.
    async fn once(&mut self, url: &Url) -> Result<(StatusCode, Option<String>, String), String> {
        let client = self.client;
        let used = &mut self.used;
        let work = async move {
            let mut response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|error| format!("{url}: {error}"))?;
            let status = response.status();
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if status.is_redirection() || !status.is_success() {
                return Ok((status, location, String::new()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| format!("{url}: {error}"))?
            {
                *used += chunk.len() as u64;
                if *used > TOTAL_LIMIT {
                    return Err(format!("the install fetched more than {TOTAL_LIMIT} bytes"));
                }
                if body.len() as u64 + chunk.len() as u64 > RESPONSE_LIMIT {
                    return Err(format!("{url}: more than {RESPONSE_LIMIT} bytes"));
                }
                body.extend_from_slice(&chunk);
            }
            let body =
                String::from_utf8(body).map_err(|_| format!("{url}: the body is not UTF-8"))?;
            Ok((status, location, body))
        };
        tokio::time::timeout(REQUEST_TIMEOUT, work)
            .await
            .map_err(|_| format!("{url}: no answer in {} s", REQUEST_TIMEOUT.as_secs()))?
    }
}

/// The CDN the install fetches from: `REAGENT_PLUGIN_CDN`, else esm.sh.
fn cdn() -> Result<Url, String> {
    cdn_from(std::env::var("REAGENT_PLUGIN_CDN").ok().as_deref())
}

/// The CDN `value` names, which must be https, or a loopback http server.
fn cdn_from(value: Option<&str>) -> Result<Url, String> {
    let text = value.unwrap_or("https://esm.sh");
    let url = Url::parse(text).map_err(|_| format!("REAGENT_PLUGIN_CDN is not a URL: {text}"))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if url.host_str().is_none()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(format!(
            "REAGENT_PLUGIN_CDN must be https://, or http://127.0.0.1:<port> for a test, not {text}"
        ));
    }
    Ok(url)
}

/// The package name and the range of an `npm:` source.
fn parse_npm(spec: &str) -> Result<(String, String), String> {
    const SHAPE: &str = "a plugin source is npm:<name>[@<range>]";
    let rest = spec
        .strip_prefix("npm:")
        .ok_or_else(|| format!("{spec}: {SHAPE}"))?;
    // The last `@` after the first character splits the name from the range: a
    // scoped name starts with one.
    let (name, range) = match rest.rfind('@').filter(|at| *at > 0) {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (rest, "latest"),
    };
    let (name, range) = (name.trim(), range.trim());
    if name.is_empty() || range.is_empty() {
        return Err(format!("{spec}: {SHAPE}"));
    }
    let scoped = name.starts_with('@');
    let parts: Vec<&str> = name.split('/').collect();
    let shaped = if scoped {
        parts.len() == 2 && parts[0].len() > 1 && !parts[1].is_empty()
    } else {
        parts.len() == 1
    };
    if !shaped {
        return Err(format!(
            "{spec}: {}",
            if scoped {
                "a scoped package is @scope/name"
            } else {
                "a package name holds no /"
            }
        ));
    }
    Ok((name.to_owned(), range.to_owned()))
}

/// The plugin folder a package name installs into: its name without a scope.
fn folder_of(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_owned()
}

/// Refuses a name that is not one folder under the plugins root.
fn check_folder(folder: &str) -> Result<(), String> {
    if folder.is_empty() || folder.starts_with('.') || folder.contains('/') || folder.contains('\\')
    {
        return Err(format!("{folder:?} is not a plugin folder name"));
    }
    Ok(())
}

/// Whether a specifier names a file of the package itself.
fn is_relative(specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
}

/// The package path `specifier` names from the file `from`, or the reason it
/// leaves the package.
fn resolve_relative(from: &str, specifier: &str) -> Result<String, String> {
    let specifier = strip_query(specifier);
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(format!("the import {specifier} leaves the plugin folder"));
                }
            }
            part => parts.push(part),
        }
    }
    let path = parts.join("/");
    if !lock::inside(&path) {
        return Err(format!("the import {specifier} leaves the plugin folder"));
    }
    Ok(path)
}

/// The package name of a bare specifier, with its subpath when it names one.
fn split_bare(specifier: &str) -> (&str, Option<&str>) {
    let scope = if let Some(stripped) = specifier.strip_prefix('@') {
        match stripped.find('/') {
            Some(at) => at + 2,
            None => return (specifier, None),
        }
    } else {
        0
    };
    match specifier[scope..].find('/') {
        Some(at) if at > 0 => {
            let cut = scope + at;
            let subpath = &specifier[cut + 1..];
            (&specifier[..cut], (!subpath.is_empty()).then_some(subpath))
        }
        _ => (specifier, None),
    }
}

/// The saved path of the dependency file the CDN path `path` names.
fn dep_saved(path: &str) -> Result<String, String> {
    let path = strip_query(path).trim_start_matches('/');
    let saved = format!("deps/{path}");
    if !lock::inside(&saved) {
        return Err(format!(
            "the dependency path {path:?} leaves the plugin folder"
        ));
    }
    Ok(saved)
}

/// The CDN path an absolute specifier names, with its query, or `None` when the
/// specifier names no file of the CDN.
fn cdn_path(specifier: &str, base: &Url) -> Option<String> {
    if specifier.starts_with('/') {
        return Some(specifier.to_owned());
    }
    let url = Url::parse(specifier).ok()?;
    if url.origin() != base.origin() {
        return None;
    }
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    Some(path)
}

/// `specifier` without its query or its fragment.
fn strip_query(specifier: &str) -> &str {
    specifier.split(['?', '#']).next().unwrap_or(specifier)
}

/// The relative path of the saved file `to`, written from the saved file
/// `from`, so a rewritten specifier starts with `./` or `../`.
fn relative_to(from: &str, to: &str) -> String {
    let from_parts: Vec<&str> = from.split('/').collect();
    let to_parts: Vec<&str> = to.split('/').collect();
    let from_dir = from_parts.len().saturating_sub(1);
    let mut same = 0;
    while same < from_dir && same + 1 < to_parts.len() && from_parts[same] == to_parts[same] {
        same += 1;
    }
    let mut out = String::new();
    for _ in same..from_dir {
        out.push_str("../");
    }
    if same == from_dir {
        out.push_str("./");
    }
    out.push_str(&to_parts[same..].join("/"));
    out
}

/// The exact version in a `/<name>@<version>[/...]` CDN path.
/// The exact version of `name` a bundle request resolved: from the URL it ended
/// on when a redirect named one, else from the absolute paths its stub imports
/// (esm.sh answers a range there with no redirect).
fn exact_version(url: &Url, stub: &str, name: &str, base: &Url) -> Option<String> {
    let exact = |version: &String| {
        version.starts_with(|first: char| first.is_ascii_digit())
            && !version.contains(['^', '~', '<', '>', '=', '*', '|', ' ', '%'])
    };
    if let Some(version) = version_of(url, name).filter(exact) {
        return Some(version);
    }
    specifiers(stub).into_iter().find_map(|spec| {
        let path = cdn_path(spec.text(stub), base)?;
        let url = base.join(&path).ok()?;
        version_of(&url, name).filter(exact)
    })
}

fn version_of(url: &Url, name: &str) -> Option<String> {
    let rest = url
        .path()
        .strip_prefix('/')?
        .strip_prefix(name)?
        .strip_prefix('@')?;
    rest.split('/')
        .next()
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
}

/// `text` with every specifier `replace` names written as it says.
fn rewrite(text: &str, replace: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for spec in specifiers(text) {
        let Some(replacement) = replace.get(spec.text(text)) else {
            continue;
        };
        out.push_str(&text[at..spec.start]);
        out.push_str(replacement);
        at = spec.end;
    }
    out.push_str(&text[at..]);
    out
}

/// One string a module uses as a specifier, by the byte range of its contents,
/// without the quotes.
#[derive(Debug, PartialEq, Eq)]
struct Spec {
    /// The byte index of the first byte inside the quotes.
    start: usize,
    /// The byte index of the closing quote.
    end: usize,
}

impl Spec {
    /// The specifier itself.
    fn text<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }
}

/// The string specifiers of a module: `import ... from "x"`, `import "x"`,
/// `export ... from "x"` and `import("x")`, in either quote.
///
/// Comments and strings outside those clauses are skipped, so a mention in
/// prose is not a specifier. It does not parse JS: a template literal ends at
/// the next backtick, and a clause ends at the first word it cannot use.
fn specifiers(text: &str) -> Vec<Spec> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index = line_end(bytes, index);
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index = block_end(bytes, index);
            continue;
        }
        if is_quote(byte) || byte == b'`' {
            index = string_end(bytes, index);
            continue;
        }
        if !is_ident_start(byte) {
            index += 1;
            continue;
        }
        let end = word_end(bytes, index);
        index = match &text[index..end] {
            // `import.meta` names no module.
            "import" => clause(bytes, end, true, &mut out),
            "export" => clause(bytes, end, false, &mut out),
            _ => end,
        };
    }
    out
}

/// The specifier an `import` or `export` clause names, pushed onto `out`.
/// Answers the index the scan goes on from.
fn clause(bytes: &[u8], index: usize, import: bool, out: &mut Vec<Spec>) -> usize {
    let mut at = skip_space(bytes, index);
    if import {
        if at < bytes.len() && bytes[at] == b'(' {
            // `import("x")`: the module of a dynamic import is its string.
            at = skip_space(bytes, at + 1);
            return match at < bytes.len() && is_quote(bytes[at]) {
                true => push_string(bytes, at, out),
                false => at,
            };
        }
        if at < bytes.len() && is_quote(bytes[at]) {
            // `import "x"`: a side-effect import.
            return push_string(bytes, at, out);
        }
        return from_clause(bytes, at, out, false);
    }
    if at < bytes.len() && bytes[at] == b'{' {
        at = skip_space(bytes, brace_end(bytes, at));
    } else if at < bytes.len() && bytes[at] == b'*' {
        at = skip_space(bytes, at + 1);
        // `as name`, which is not a module name; `from` follows it.
        if word_at(bytes, at) == b"as" {
            let name = skip_space(bytes, at + 2);
            at = skip_space(bytes, word_end(bytes, name));
        }
    } else {
        return index;
    }
    from_clause(bytes, at, out, true)
}

/// The `from "x"` of a clause, pushed onto `out`. With `strict`, the first word
/// must be `from`: an `export` that binds a name names no module.
fn from_clause(bytes: &[u8], mut at: usize, out: &mut Vec<Spec>, strict: bool) -> usize {
    loop {
        at = skip_space(bytes, at);
        if at >= bytes.len() {
            return at;
        }
        match bytes[at] {
            b'{' => at = brace_end(bytes, at),
            b',' | b'*' => at += 1,
            byte if is_ident_start(byte) => {
                let end = word_end(bytes, at);
                if &bytes[at..end] != b"from" {
                    if strict {
                        return at;
                    }
                    at = end;
                    continue;
                }
                let next = skip_space(bytes, end);
                if next < bytes.len() && is_quote(bytes[next]) {
                    return push_string(bytes, next, out);
                }
                return next;
            }
            _ => return at,
        }
    }
}

/// Records the string at `at` as a specifier, and answers the index after it.
fn push_string(bytes: &[u8], at: usize, out: &mut Vec<Spec>) -> usize {
    let end = string_end(bytes, at);
    if end > at + 1 && end <= bytes.len() && bytes[end - 1] == bytes[at] {
        out.push(Spec {
            start: at + 1,
            end: end - 1,
        });
    }
    end
}

/// The index of the first byte after the comment that starts at `index`.
fn line_end(bytes: &[u8], index: usize) -> usize {
    match bytes[index..].iter().position(|byte| *byte == b'\n') {
        Some(at) => index + at,
        None => bytes.len(),
    }
}

/// The index of the first byte after the block comment that starts at `index`.
fn block_end(bytes: &[u8], index: usize) -> usize {
    let mut at = index + 2;
    while at + 1 < bytes.len() {
        if bytes[at] == b'*' && bytes[at + 1] == b'/' {
            return at + 2;
        }
        at += 1;
    }
    bytes.len()
}

/// The index of the first byte after the string that starts at `index`.
fn string_end(bytes: &[u8], index: usize) -> usize {
    let quote = bytes[index];
    let mut at = index + 1;
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            at += 2;
            continue;
        }
        if bytes[at] == quote {
            return at + 1;
        }
        at += 1;
    }
    bytes.len()
}

/// The index of the first byte after the `{...}` that starts at `index`.
fn brace_end(bytes: &[u8], index: usize) -> usize {
    let mut depth = 0usize;
    let mut at = index;
    while at < bytes.len() {
        match bytes[at] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}

/// The word at `index`, which is empty when no word starts there.
fn word_at(bytes: &[u8], index: usize) -> &[u8] {
    bytes.get(index..word_end(bytes, index)).unwrap_or_default()
}

/// The index of the first byte after the word that starts at `index`.
fn word_end(bytes: &[u8], index: usize) -> usize {
    let mut at = index;
    while at < bytes.len() && is_ident(bytes[at]) {
        at += 1;
    }
    at
}

/// The index of the first byte after the whitespace and comments of `index`.
fn skip_space(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index + 1 < bytes.len() && bytes[index] == b'/' {
            match bytes[index + 1] {
                b'/' => {
                    index = line_end(bytes, index);
                    continue;
                }
                b'*' => {
                    index = block_end(bytes, index);
                    continue;
                }
                _ => {}
            }
        }
        return index;
    }
}

/// Whether `byte` starts or is part of a name.
fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

/// Whether `byte` is part of a name.
fn is_ident(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

/// Whether `byte` opens a string that names a module.
fn is_quote(byte: u8) -> bool {
    byte == b'"' || byte == b'\''
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reqwest::Url;

    use super::*;

    /// The specifier texts of `text`, in order.
    fn found(text: &str) -> Vec<&str> {
        let specs = specifiers(text);
        specs.iter().map(|spec| spec.text(text)).collect()
    }

    #[test]
    fn install_scanner_finds_every_import_form() {
        let text = concat!(
            "import a from \"one\";\n",
            "import { b, c } from 'two';\n",
            "import * as d from \"three\";\n",
            "import \"four\";\n",
            "export * from \"five\";\n",
            "export { e } from 'six';\n",
            "export * as g from \"seven\";\n",
        );
        assert_eq!(
            found(text),
            vec!["one", "two", "three", "four", "five", "six", "seven"]
        );
    }

    #[test]
    fn install_scanner_finds_a_dynamic_import() {
        let text = "const m = await import(\"./lazy.js\");\nconst n = import('other');\n";
        assert_eq!(found(text), vec!["./lazy.js", "other"]);
    }

    #[test]
    fn install_scanner_ignores_comments_and_strings() {
        let text = concat!(
            "// import a from \"line\";\n",
            "/* import b from \"block\"; */\n",
            "const text = `import c from \"template\"`;\n",
            "const quote = \"import d from 'quoted'\";\n",
            "export const x = 1;\n",
        );
        assert_eq!(found(text), Vec::<&str>::new());
    }

    #[test]
    fn install_scanner_reads_an_escaped_quote_inside_a_comment() {
        let text = "// it's fine\nimport a from \"real\";\nconst s = \"a \\\" b\";\n";
        assert_eq!(found(text), vec!["real"]);
    }

    #[test]
    fn install_split_bare_keeps_the_scope_and_the_subpath() {
        assert_eq!(split_bare("zod"), ("zod", None));
        assert_eq!(split_bare("zod/v4"), ("zod", Some("v4")));
        assert_eq!(split_bare("zod/"), ("zod", None));
        assert_eq!(split_bare("@scope/pkg"), ("@scope/pkg", None));
        assert_eq!(
            split_bare("@scope/pkg/sub/deep"),
            ("@scope/pkg", Some("sub/deep"))
        );
    }

    #[test]
    fn install_spec_splits_the_name_and_the_range() {
        assert_eq!(
            parse_npm("npm:zod").expect("a name"),
            ("zod".to_owned(), "latest".to_owned())
        );
        assert_eq!(
            parse_npm("npm:zod@^4.1").expect("a range"),
            ("zod".to_owned(), "^4.1".to_owned())
        );
        assert_eq!(
            parse_npm("npm:@scope/pkg").expect("a scoped name"),
            ("@scope/pkg".to_owned(), "latest".to_owned())
        );
        assert_eq!(
            parse_npm("npm:@scope/pkg@1.2.3").expect("a scoped range"),
            ("@scope/pkg".to_owned(), "1.2.3".to_owned())
        );
        assert!(parse_npm("zod").unwrap_err().contains("npm:"));
        assert!(parse_npm("npm:").is_err());
        assert!(parse_npm("npm:@scope").is_err());
        assert!(parse_npm("npm:@/pkg").is_err());
        assert!(parse_npm("npm:zod@").is_err());
    }

    #[test]
    fn install_folder_name_drops_the_scope() {
        assert_eq!(folder_of("@scope/pkg"), "pkg");
        assert_eq!(folder_of("zod"), "zod");
    }

    #[test]
    fn install_relative_path_between_two_saved_files() {
        assert_eq!(
            relative_to(
                "deps/tiny@1.0.2/index.entry.mjs",
                "deps/tiny@1.0.2/node/tiny.bundle.mjs"
            ),
            "./node/tiny.bundle.mjs"
        );
        assert_eq!(relative_to("deps/a/b.mjs", "deps/c.mjs"), "../c.mjs");
        assert_eq!(relative_to("index.js", "deps/x.mjs"), "./deps/x.mjs");
        assert_eq!(relative_to("deps/x.mjs", "deps/x.mjs"), "./x.mjs");
    }

    #[test]
    fn install_rewrite_makes_absolute_specifiers_relative() {
        let text = concat!(
            "/* esm.sh - tiny@1.0.2 */\n",
            "export * from \"/tiny@1.0.2/node/tiny.bundle.mjs\";\n",
            "import { readFileSync } from \"node:fs\";\n",
            "export { default } from \"/tiny@1.0.2/node/tiny.bundle.mjs\";\n",
        );
        let mut replace = BTreeMap::new();
        replace.insert(
            "/tiny@1.0.2/node/tiny.bundle.mjs".to_owned(),
            "./node/tiny.bundle.mjs".to_owned(),
        );
        assert_eq!(
            rewrite(text, &replace),
            concat!(
                "/* esm.sh - tiny@1.0.2 */\n",
                "export * from \"./node/tiny.bundle.mjs\";\n",
                "import { readFileSync } from \"node:fs\";\n",
                "export { default } from \"./node/tiny.bundle.mjs\";\n",
            )
        );
    }

    #[test]
    fn install_absolute_specifier_keeps_its_path_and_query() {
        let base = Url::parse("https://esm.sh").expect("a URL");
        assert_eq!(
            cdn_path("/a@1/node/x.mjs", &base),
            Some("/a@1/node/x.mjs".to_owned())
        );
        assert_eq!(
            cdn_path("https://esm.sh/a@1/x.mjs?target=node", &base),
            Some("/a@1/x.mjs?target=node".to_owned())
        );
        assert_eq!(cdn_path("https://other.example/a.js", &base), None);
        assert_eq!(cdn_path("node:fs", &base), None);
        assert_eq!(cdn_path("./x.js", &base), None);
    }

    #[test]
    fn install_crawled_path_must_stay_inside() {
        assert_eq!(
            resolve_relative("index.js", "./lib/util.js").expect("a path"),
            "lib/util.js"
        );
        assert_eq!(
            resolve_relative("lib/a.js", "../b.js").expect("a path"),
            "b.js"
        );
        let error = resolve_relative("index.js", "../../x.js").unwrap_err();
        assert!(error.contains("leaves the plugin folder"), "{error}");
        assert!(resolve_relative("index.js", "./").is_err());
        assert_eq!(
            dep_saved("/tiny@1.0.2/node/x.mjs?target=node").expect("a path"),
            "deps/tiny@1.0.2/node/x.mjs"
        );
        assert!(dep_saved("/tiny@1.0.2/../../x.mjs").is_err());
        assert!(dep_saved("/").is_err());
    }

    #[test]
    fn install_cdn_refuses_an_insecure_origin() {
        assert!(cdn_from(None).is_ok());
        assert!(cdn_from(Some("https://esm.sh")).is_ok());
        assert_eq!(
            cdn_from(Some("http://127.0.0.1:8080/"))
                .expect("a loopback CDN")
                .as_str(),
            "http://127.0.0.1:8080/"
        );
        assert!(cdn_from(Some("http://localhost:9")).is_ok());
        assert!(cdn_from(Some("http://example.com")).is_err());
        assert!(cdn_from(Some("ftp://esm.sh")).is_err());
        assert!(cdn_from(Some("nonsense")).is_err());
    }

    #[test]
    fn install_version_comes_from_the_stub_when_the_url_names_a_range() {
        let base = Url::parse("https://esm.sh").expect("a URL");
        let ranged = Url::parse("https://esm.sh/nanoid@^5?bundle&target=node").expect("a URL");
        let stub = "/* esm.sh - nanoid@5.1.16 */\nexport * from \"/nanoid@5.1.16/node/nanoid.bundle.mjs\";\n";
        assert_eq!(
            exact_version(&ranged, stub, "nanoid", &base),
            Some("5.1.16".to_owned())
        );
        let exact = Url::parse("https://esm.sh/nanoid@5.1.0?bundle&target=node").expect("a URL");
        assert_eq!(
            exact_version(&exact, "", "nanoid", &base),
            Some("5.1.0".to_owned())
        );
        assert_eq!(
            exact_version(&ranged, "export const x = 1;", "nanoid", &base),
            None
        );
    }

    #[test]
    fn install_version_comes_from_the_url_path() {
        let url = Url::parse("https://esm.sh/demo-plugin@1.0.0/package.json?raw").expect("a URL");
        assert_eq!(version_of(&url, "demo-plugin"), Some("1.0.0".to_owned()));
        let url = Url::parse("https://esm.sh/@scope/pkg@2.1.0/package.json").expect("a URL");
        assert_eq!(version_of(&url, "@scope/pkg"), Some("2.1.0".to_owned()));
        let url = Url::parse("https://esm.sh/other@1.0.0/x.js").expect("a URL");
        assert_eq!(version_of(&url, "demo-plugin"), None);
    }
}
