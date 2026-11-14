//! End-to-end tests of `reagent plugin add|list|remove|update`: every test
//! runs the binary against a fake esm.sh on a loopback port.
#![cfg(feature = "testing")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// One route of the fake CDN.
#[derive(Clone)]
enum Route {
    /// A body answered with 200.
    Body(String),
    /// A 302 to the location.
    Redirect(String),
}

/// A fake esm.sh: it answers by request target from a map the test can change.
struct Cdn {
    /// The loopback port the server listens on.
    port: u16,
    /// The routes, by request target.
    routes: Arc<Mutex<BTreeMap<String, Route>>>,
}

impl Cdn {
    /// Starts a server that answers by `routes`, plus the routes every test
    /// needs for a plugin called `demo-plugin`.
    async fn start(routes: BTreeMap<String, Route>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port");
        let port = listener.local_addr().expect("its address").port();
        let routes = Arc::new(Mutex::new(routes));
        let shared = Arc::clone(&routes);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let routes = Arc::clone(&shared);
                tokio::spawn(serve(stream, routes));
            }
        });
        Self { port, routes }
    }

    /// The URL the installer is told to use.
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Answers `target` with `route`.
    fn set(&self, target: &str, route: Route) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.insert(target.to_owned(), route);
        }
    }

    /// Answers `target` with a 302 to `to`, a path of this server.
    fn redirect(&self, target: &str, to: &str) {
        self.set(target, Route::Redirect(format!("{}{to}", self.url())));
    }

    /// Answers `target` with `body`.
    fn body(&self, target: &str, body: impl Into<String>) {
        self.set(target, Route::Body(body.into()));
    }

    /// Points a range at the version `version` of `demo-plugin` and serves it.
    fn demo(&self, version: &str) {
        let package = format!(
            r#"{{
  "name": "demo-plugin",
  "version": "{version}",
  "reagent": {{
    "entry": "index.js",
    "permissions": {{ "fs": "read" }}
  }},
  "dependencies": {{ "tiny": "^1" }}
}}"#
        );
        if version == "1.0.0" {
            self.redirect(
                "/demo-plugin@^1/package.json?raw",
                &at(version, "package.json?raw"),
            );
        }
        self.body(&at(version, "package.json?raw"), package);
        self.body(
            &at(version, "index.js?raw"),
            format!(
                "// {version}\nimport {{ util }} from \"./lib/util.js\";\nimport tiny from \"tiny\";\nexport default () => [util, tiny];\n"
            ),
        );
        self.body(&at(version, "lib/util.js?raw"), "export const util = 1;\n");
    }
}

/// The CDN path of one file of `demo-plugin` at `version`.
fn at(version: &str, path: &str) -> String {
    format!("/demo-plugin@{version}/{path}")
}

/// Answers one connection from the routes.
async fn serve(mut stream: tokio::net::TcpStream, routes: Arc<Mutex<BTreeMap<String, Route>>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 64 * 1024 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(decode)
        .unwrap_or_default();
    let route = routes
        .lock()
        .ok()
        .and_then(|routes| routes.get(&target).cloned());
    let response = match route {
        Some(Route::Body(body)) => format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/javascript\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
        Some(Route::Redirect(to)) => format!(
            "HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        ),
        None => format!(
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n{target}: no route"
        ),
    };
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// `target` with every `%XX` decoded, so a route reads as it was written.
fn decode(target: &str) -> String {
    let bytes = target.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&target[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One test's own config folder and fake CDN.
struct Home {
    root: tempfile::TempDir,
    cdn: Cdn,
}

impl Home {
    /// A home whose CDN answers the demo plugin at 1.0.0 and the whole tiny
    /// dependency chain.
    async fn new() -> Self {
        let cdn = Cdn::start(BTreeMap::new()).await;
        cdn.demo("1.0.0");
        cdn.redirect(
            "/tiny@^1?bundle&target=node",
            "/tiny@1.0.2?bundle&target=node",
        );
        cdn.body(
            "/tiny@1.0.2?bundle&target=node",
            concat!(
                "/* esm.sh - tiny@1.0.2 */\n",
                "export * from \"/tiny@1.0.2/node/tiny.bundle.mjs\";\n",
                "export { default } from \"/tiny@1.0.2/node/tiny.bundle.mjs\";\n"
            ),
        );
        cdn.body(
            "/tiny@1.0.2/node/tiny.bundle.mjs",
            "import { readFileSync } from \"node:fs\";\nexport default 42;\n",
        );
        let root = tempfile::Builder::new()
            .prefix("ri")
            .tempdir()
            .expect("a temp folder");
        Self { root, cdn }
    }

    /// The config folder of the run.
    fn config(&self) -> PathBuf {
        self.root.path().join("config")
    }

    /// The plugins folder of the run's config: `<config dir>/plugins`.
    fn plugins(&self) -> PathBuf {
        self.config().join("reagent").join("plugins")
    }

    /// Runs the binary with the fake CDN as the plugin source.
    async fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[]).await
    }

    /// Runs the binary with extra environment variables.
    async fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(BIN);
        command
            .env("XDG_CONFIG_HOME", self.config())
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("REAGENT_PLUGIN_CDN", self.cdn.url())
            .env("REAGENT_MODELS_DEV_URL", "http://127.0.0.1:1")
            .env("HOME", self.root.path())
            .env_remove("REAGENT_PROFILE")
            .args(args)
            .current_dir(self.root.path());
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().await.expect("the binary runs")
    }

    /// Writes a plugin folder the user made by hand, with no lockfile.
    async fn handmade(&self, folder: &str) {
        let dir = self.plugins().join(folder);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("the plugin folder");
        tokio::fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name": "{folder}", "version": "0.2.0", "reagent": {{"entry": "index.js"}}}}"#
            ),
        )
        .await
        .expect("the manifest");
        tokio::fs::write(dir.join("index.js"), "export default () => 1;\n")
            .await
            .expect("the entry");
    }

    /// The names in the plugins folder, sorted.
    async fn folders(&self) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(mut dir) = tokio::fs::read_dir(self.plugins()).await else {
            return names;
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        names
    }

    /// The staging folders a failed or a finished install left behind.
    async fn staging(&self) -> Vec<String> {
        self.folders()
            .await
            .into_iter()
            .filter(|name| name.starts_with(".staging-"))
            .collect()
    }
}

/// The stdout of a run, as text.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Reads one file of the plugins folder of a home.
async fn read(home: &Home, path: &str) -> String {
    tokio::fs::read_to_string(home.plugins().join(path))
        .await
        .unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The lockfile of a plugin folder under the plugins folder.
async fn lock(home: &Home, folder: &str) -> Value {
    let text = read(home, &format!("{folder}/reagent-lock.json")).await;
    serde_json::from_str(&text).expect("the lockfile is JSON")
}

/// Whether a path holds anything.
async fn exists(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok()
}

#[tokio::test]
async fn install_add_writes_the_folder_and_its_lock() {
    let home = Home::new().await;
    let output = home
        .run(&["plugin", "add", "npm:demo-plugin@^1", "--yes"])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let folders = home.folders().await;
    assert_eq!(folders, vec!["demo-plugin".to_owned()], "{folders:?}");
    // The plugin's own files, at the same relative paths.
    assert!(
        read(&home, "demo-plugin/package.json")
            .await
            .contains("\"tiny\""),
        "package.json is the fetched one"
    );
    let entry = read(&home, "demo-plugin/index.js").await;
    assert!(entry.contains("lib/util.js"), "{entry}");
    assert!(entry.contains("\"tiny\""), "{entry}");
    assert_eq!(
        read(&home, "demo-plugin/lib/util.js").await,
        "export const util = 1;\n"
    );
    // The stub, with its absolute import rewritten to a relative one.
    let stub = read(&home, "demo-plugin/deps/tiny@1.0.2/index.entry.mjs").await;
    assert!(stub.contains("\"./node/tiny.bundle.mjs\""), "{stub}");
    assert!(!stub.contains("\"/tiny@1.0.2"), "{stub}");
    // The bundle the stub named, fetched too.
    let bundle = read(&home, "demo-plugin/deps/tiny@1.0.2/node/tiny.bundle.mjs").await;
    assert!(bundle.contains("node:fs"), "{bundle}");
    // The lock.
    let lock = lock(&home, "demo-plugin").await;
    assert_eq!(lock["requested"], "npm:demo-plugin@^1", "{lock}");
    assert_eq!(lock["source"], "npm:demo-plugin@1.0.0", "{lock}");
    assert_eq!(
        lock["imports"]["tiny"], "deps/tiny@1.0.2/index.entry.mjs",
        "{lock}"
    );
    for path in [
        "package.json",
        "index.js",
        "lib/util.js",
        "deps/tiny@1.0.2/index.entry.mjs",
        "deps/tiny@1.0.2/node/tiny.bundle.mjs",
    ] {
        let bytes = tokio::fs::read(home.plugins().join("demo-plugin").join(path))
            .await
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(
            lock["files"][path],
            reagent_script::plugin::lock::hash(&bytes),
            "{path} in {lock}"
        );
    }
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_add_without_yes_and_no_terminal_refuses() {
    let home = Home::new().await;
    let output = home.run(&["plugin", "add", "npm:demo-plugin@^1"]).await;
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("not a terminal: pass --yes to install without a question"),
        "{}",
        text(&output.stderr)
    );
    assert!(!exists(&home.plugins().join("demo-plugin")).await);
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_add_without_an_entry_refuses() {
    let home = Home::new().await;
    home.cdn.redirect(
        "/plain@^1/package.json?raw",
        "/plain@0.1.0/package.json?raw",
    );
    home.cdn.body(
        "/plain@0.1.0/package.json?raw",
        r#"{"name": "plain", "version": "0.1.0"}"#,
    );
    let output = home.run(&["plugin", "add", "npm:plain@^1", "--yes"]).await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("plain@0.1.0 is not a reagent plugin"),
        "{stderr}"
    );
    assert!(stderr.contains("no reagent.entry"), "{stderr}");
    assert!(!exists(&home.plugins().join("plain")).await);
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_add_refuses_a_bare_import_that_is_no_dependency() {
    let home = Home::new().await;
    home.cdn.redirect(
        "/lonely@^1/package.json?raw",
        "/lonely@1.0.0/package.json?raw",
    );
    home.cdn.body(
        "/lonely@1.0.0/package.json?raw",
        r#"{"name": "lonely", "version": "1.0.0", "reagent": {"entry": "index.js"}}"#,
    );
    home.cdn.body(
        "/lonely@1.0.0/index.js?raw",
        "import nothere from \"nothere\";\nexport default nothere;\n",
    );
    let output = home.run(&["plugin", "add", "npm:lonely@^1", "--yes"]).await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("nothere"), "{stderr}");
    assert!(!exists(&home.plugins().join("lonely")).await);
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_add_refuses_a_cdn_that_is_not_https() {
    let home = Home::new().await;
    let output = home
        .run_env(
            &["plugin", "add", "npm:demo-plugin@^1", "--yes"],
            &[("REAGENT_PLUGIN_CDN", "http://example.com")],
        )
        .await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("REAGENT_PLUGIN_CDN"), "{stderr}");
    assert!(!exists(&home.plugins().join("demo-plugin")).await);
}

#[tokio::test]
async fn install_git_add_refuses_a_missing_git_command() {
    let home = Home::new().await;
    let output = home
        .run_env(
            &["plugin", "add", "git:github.com/owner/repo@v1", "--yes"],
            &[("PATH", "/nonexistent")],
        )
        .await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("git sources need the git command on PATH"),
        "{stderr}"
    );
    assert_eq!(home.folders().await, Vec::<String>::new());
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_git_add_refuses_a_plain_http_source() {
    let home = Home::new().await;
    let output = home
        .run(&["plugin", "add", "git:http://example.com/x", "--yes"])
        .await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("http://example.com/x"), "{stderr}");
    assert!(stderr.contains("not a scheme"), "{stderr}");
    assert_eq!(home.folders().await, Vec::<String>::new());
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_list_names_installed_and_handmade_plugins() {
    let home = Home::new().await;
    let output = home
        .run(&["plugin", "add", "npm:demo-plugin@^1", "--yes"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    home.handmade("handmade").await;
    let output = home.run(&["plugin", "list"]).await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "demo-plugin  1.0.0  npm:demo-plugin@1.0.0",
            "handmade  0.2.0  local",
        ],
        "{stdout}"
    );
}

#[tokio::test]
async fn install_remove_takes_the_installed_plugin_and_refuses_a_handmade_one() {
    let home = Home::new().await;
    let output = home
        .run(&["plugin", "add", "npm:demo-plugin@^1", "--yes"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    home.handmade("handmade").await;
    let output = home.run(&["plugin", "remove", "handmade", "--yes"]).await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("handmade was not installed by reagent plugin add"),
        "{stderr}"
    );
    assert!(exists(&home.plugins().join("handmade")).await);
    let output = home
        .run(&["plugin", "remove", "demo-plugin", "--yes"])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    assert!(!exists(&home.plugins().join("demo-plugin")).await);
}

#[tokio::test]
async fn install_update_says_up_to_date_then_moves_to_the_new_version() {
    let home = Home::new().await;
    let output = home
        .run(&["plugin", "add", "npm:demo-plugin@^1", "--yes"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = home
        .run(&["plugin", "update", "demo-plugin", "--yes"])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    assert!(
        text(&output.stdout).contains("demo-plugin is up to date"),
        "{}",
        text(&output.stdout)
    );
    assert_eq!(home.staging().await, Vec::<String>::new());
    // The CDN serves 1.0.1 for `^1` now.
    home.cdn.demo("1.0.1");
    home.cdn.redirect(
        "/demo-plugin@^1/package.json?raw",
        &at("1.0.1", "package.json?raw"),
    );
    let output = home
        .run(&["plugin", "update", "demo-plugin", "--yes"])
        .await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    assert!(
        read(&home, "demo-plugin/index.js")
            .await
            .contains("// 1.0.1"),
        "the folder holds the new entry"
    );
    let lock = lock(&home, "demo-plugin").await;
    assert_eq!(lock["source"], "npm:demo-plugin@1.0.1", "{lock}");
    assert_eq!(lock["requested"], "npm:demo-plugin@^1", "{lock}");
    let folders = home.folders().await;
    assert_eq!(folders, vec!["demo-plugin".to_owned()], "{folders:?}");
}

/// A git repository of a plugin, in a temp folder of its own.
struct Repo {
    /// The folder the repository stands in.
    root: tempfile::TempDir,
}

impl Repo {
    /// A repository of the plugin `git-plugin` at 0.1.0: its manifest, an entry
    /// that imports a relative file and `tiny`, and that file, committed and
    /// tagged `v1`.
    async fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("rg")
            .tempdir()
            .expect("a temp folder");
        let repo = Self { root };
        repo.write(
            "package.json",
            concat!(
                "{\n",
                "  \"name\": \"git-plugin\",\n",
                "  \"version\": \"0.1.0\",\n",
                "  \"reagent\": {\"entry\": \"index.js\", \"permissions\": {\"fs\": \"read\"}},\n",
                "  \"dependencies\": {\"tiny\": \"^1\"}\n",
                "}\n",
            ),
        )
        .await;
        repo.write(
            "index.js",
            concat!(
                "import { util } from \"./lib/util.js\";\n",
                "import tiny from \"tiny\";\n",
                "export default () => [util, tiny];\n",
            ),
        )
        .await;
        repo.write("lib/util.js", "export const util = 1;\n").await;
        repo.git(&["init", "-q"]).await;
        repo.commit("the first version").await;
        repo.git(&["tag", "v1"]).await;
        repo
    }

    /// The folder the repository stands in.
    fn path(&self) -> &Path {
        self.root.path()
    }

    /// The `file://` source of the repository, without a ref.
    fn url(&self) -> String {
        format!("file://{}", self.path().display())
    }

    /// The source of the repository at `git_ref`.
    fn source(&self, git_ref: &str) -> String {
        format!("git:{}@{git_ref}", self.url())
    }

    /// Runs one git command in the repository and answers its stdout, with the
    /// identity and the branch a machine's own config would change.
    async fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(self.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=reagent",
                "-c",
                "user.email=reagent@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .await
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// Writes one file of the repository, with its folders.
    async fn write(&self, path: &str, body: &str) {
        let full = self.path().join(path);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await.expect("the folder");
        }
        tokio::fs::write(&full, body).await.expect("the file");
    }

    /// Commits every change and answers the commit's full hash.
    async fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]).await;
        self.git(&["commit", "-q", "-m", message]).await;
        self.git(&["rev-parse", "HEAD"]).await.trim().to_owned()
    }
}

#[tokio::test]
async fn install_git_add_fetches_the_tag_and_writes_the_lock() {
    let home = Home::new().await;
    let repo = Repo::new().await;
    let commit = repo.git(&["rev-parse", "v1"]).await.trim().to_owned();
    let source = repo.source("v1");
    let output = home.run(&["plugin", "add", &source, "--yes"]).await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let stderr = text(&output.stderr);
    assert!(stderr.contains(&source), "{stderr}");
    assert!(
        stderr.contains(&format!("commit: {}", &commit[..12])),
        "{stderr}"
    );
    assert!(stderr.contains("dependencies: tiny@1.0.2"), "{stderr}");
    // The tracked files, at the same relative paths, and the `tiny` files.
    let folders = home.folders().await;
    assert_eq!(folders, vec!["git-plugin".to_owned()], "{folders:?}");
    let entry = read(&home, "git-plugin/index.js").await;
    assert!(entry.contains("./lib/util.js"), "{entry}");
    assert!(entry.contains("\"tiny\""), "{entry}");
    assert_eq!(
        read(&home, "git-plugin/lib/util.js").await,
        "export const util = 1;\n"
    );
    assert!(
        exists(
            &home
                .plugins()
                .join("git-plugin/deps/tiny@1.0.2/index.entry.mjs")
        )
        .await
    );
    let lock = lock(&home, "git-plugin").await;
    assert_eq!(lock["requested"], source.as_str(), "{lock}");
    assert_eq!(lock["source"], source.as_str(), "{lock}");
    assert_eq!(lock["commit"], commit.as_str(), "{lock}");
    assert_eq!(
        lock["imports"]["tiny"], "deps/tiny@1.0.2/index.entry.mjs",
        "{lock}"
    );
    for path in [
        "package.json",
        "index.js",
        "lib/util.js",
        "deps/tiny@1.0.2/index.entry.mjs",
        "deps/tiny@1.0.2/node/tiny.bundle.mjs",
    ] {
        let bytes = tokio::fs::read(home.plugins().join("git-plugin").join(path))
            .await
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(
            lock["files"][path],
            reagent_script::plugin::lock::hash(&bytes),
            "{path} in {lock}"
        );
    }
    // The clone does not stay behind.
    assert!(!exists(&home.plugins().join("git-plugin/.git-fetch")).await);
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_git_add_refuses_a_symbolic_link() {
    let home = Home::new().await;
    let repo = Repo::new().await;
    tokio::fs::symlink("index.js", repo.path().join("lib/link.js"))
        .await
        .expect("the link");
    repo.commit("adds a link").await;
    repo.git(&["tag", "link"]).await;
    let output = home
        .run(&["plugin", "add", &repo.source("link"), "--yes"])
        .await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("lib/link.js"), "{stderr}");
    assert!(stderr.contains("symbolic link"), "{stderr}");
    assert_eq!(home.folders().await, Vec::<String>::new());
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_git_add_refuses_a_manifest_without_an_entry() {
    let home = Home::new().await;
    let repo = Repo::new().await;
    repo.write(
        "package.json",
        "{\"name\": \"git-plugin\", \"version\": \"0.1.0\"}\n",
    )
    .await;
    repo.commit("takes the entry away").await;
    repo.git(&["tag", "plain"]).await;
    let source = repo.source("plain");
    let output = home.run(&["plugin", "add", &source, "--yes"]).await;
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let stderr = text(&output.stderr);
    assert!(stderr.contains(&source), "{stderr}");
    assert!(stderr.contains("is not a reagent plugin"), "{stderr}");
    assert!(stderr.contains("no reagent.entry"), "{stderr}");
    assert_eq!(home.folders().await, Vec::<String>::new());
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_git_update_keeps_a_plugin_added_at_a_tag() {
    let home = Home::new().await;
    let repo = Repo::new().await;
    let first = repo.git(&["rev-parse", "v1"]).await.trim().to_owned();
    let output = home
        .run(&["plugin", "add", &repo.source("v1"), "--yes"])
        .await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = home.run(&["plugin", "update", "git-plugin", "--yes"]).await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    assert!(
        text(&output.stdout).contains("git-plugin is up to date"),
        "{}",
        text(&output.stdout)
    );
    // A new commit and a `v2` tag: `v1` still names what the folder holds.
    repo.write("lib/util.js", "export const util = 2;\n").await;
    let second = repo.commit("the second version").await;
    assert_ne!(first, second);
    repo.git(&["tag", "v2"]).await;
    let output = home.run(&["plugin", "update", "git-plugin", "--yes"]).await;
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("git-plugin is up to date"),
        "{}",
        text(&output.stdout)
    );
    assert_eq!(
        read(&home, "git-plugin/lib/util.js").await,
        "export const util = 1;\n"
    );
    let lock = lock(&home, "git-plugin").await;
    assert_eq!(lock["commit"], first.as_str(), "{lock}");
    assert_eq!(home.staging().await, Vec::<String>::new());
}

#[tokio::test]
async fn install_git_update_moves_the_default_branch_to_a_new_commit() {
    let home = Home::new().await;
    let repo = Repo::new().await;
    let first = repo.git(&["rev-parse", "HEAD"]).await.trim().to_owned();
    let source = format!("git:{}", repo.url());
    let output = home.run(&["plugin", "add", &source, "--yes"]).await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    let first_lock = lock(&home, "git-plugin").await;
    assert_eq!(first_lock["requested"], source.as_str(), "{first_lock}");
    assert_eq!(
        first_lock["source"],
        format!("{source}@main").as_str(),
        "{first_lock}"
    );
    assert_eq!(first_lock["commit"], first.as_str(), "{first_lock}");
    // A new commit on the default branch.
    repo.write("lib/util.js", "export const util = 2;\n").await;
    let second = repo.commit("the second version").await;
    let output = home.run(&["plugin", "update", "git-plugin", "--yes"]).await;
    assert!(
        output.status.success(),
        "exit {} stderr {}",
        output.status,
        text(&output.stderr)
    );
    assert!(
        text(&output.stdout).contains("updated git-plugin"),
        "{}",
        text(&output.stdout)
    );
    assert_eq!(
        read(&home, "git-plugin/lib/util.js").await,
        "export const util = 2;\n"
    );
    let moved = lock(&home, "git-plugin").await;
    assert_eq!(moved["commit"], second.as_str(), "{moved}");
    assert_eq!(
        moved["source"],
        format!("{source}@main").as_str(),
        "{moved}"
    );
    let folders = home.folders().await;
    assert_eq!(folders, vec!["git-plugin".to_owned()], "{folders:?}");
    assert_eq!(home.staging().await, Vec::<String>::new());
}
