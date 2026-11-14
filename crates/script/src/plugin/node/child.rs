//! The child processes of a plugin's `node:child_process`: the program check,
//! the sandbox helper every child starts through, the pipes and the signals.
//!
//! The JS half is the wrapper `child_process.js`, which calls the host
//! functions of [`ChildHost`]; a plugin has no name for that module, so nothing
//! it patches in JS can decide which program runs, whose child a call touches or
//! whose permission a start spends. Every start goes through reagent's sandbox
//! helper, and every message of a refusal names the plugin and the program.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rquickjs::function::Opt;
use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::promise::Promised;
use rquickjs::{
    Array, Ctx, Exception, Function, IntoJs, Object, Result as JsResult, TypedArray, Value,
};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;

use super::super::manifest::{Host, Program};
use super::{Access, Kept, NODE, Policy, Wanted};

/// What the wrapper imports: the host functions below.
pub(super) const CHILD_HOST: &str = "reagent:raw/child-host";

/// The most child processes one plugin may hold at once.
const LIMIT: usize = 16;

/// The most bytes one pull of a pipe answers.
const CHUNK: usize = 16 * 1024;

/// The marker a pull answers for a child of another plugin.
const FOREIGN: &str = "that child process belongs to another plugin";

/// The programs a `run` permission names, and, when one of them is a shell, the
/// note that says so.
pub(super) struct Runs {
    /// Every entry, with the program it resolves to.
    pub runs: Vec<Run>,
    /// The note a `run` list with a shell raises.
    pub note: Option<String>,
}

/// One entry of a `run` permission, resolved once at register.
pub(super) struct Run {
    /// The entry as the plugin wrote it.
    entry: String,
    /// Whether the entry is a bare name, which a plugin may ask for by name.
    named: bool,
    /// The program the entry resolves to, canonical.
    program: PathBuf,
}

/// Resolves every entry of a `run` permission, or names the one that does not
/// resolve, which refuses the plugin.
pub(super) fn runs(list: &[Program], name: &str) -> Result<Runs, String> {
    let mut runs = Vec::with_capacity(list.len());
    for program in list {
        runs.push(resolved_run(program)?);
    }
    let note = shell_note(name, &runs);
    Ok(Runs { runs, note })
}

/// One entry of a `run` list, resolved: a name on reagent's `PATH`, or the path
/// as given, with every link of it followed.
fn resolved_run(program: &Program) -> Result<Run, String> {
    match program {
        Program::Name(name) => match on_path(name) {
            Some(program) => Ok(Run {
                entry: name.clone(),
                named: true,
                program,
            }),
            None => Err(format!(
                "permission `run` entry \"{name}\" names no program on PATH"
            )),
        },
        Program::Path(path) => match canonical(path) {
            Some(program) => Ok(Run {
                entry: path.to_string_lossy().into_owned(),
                named: false,
                program,
            }),
            None => Err(format!(
                "permission `run` entry \"{}\" names no file",
                path.display()
            )),
        },
    }
}

/// The note a plugin that lists a shell raises: with one, it can run any
/// program the shell can.
fn shell_note(name: &str, runs: &[Run]) -> Option<String> {
    let shell = runs.iter().find_map(|run| {
        let file = run.program.file_name()?.to_string_lossy();
        ["sh", "bash", "zsh", "dash"]
            .contains(&file.as_ref())
            .then(|| run.entry.clone())
    })?;
    Some(format!(
        "plugin {name} lists the shell {shell}: it can then run any program"
    ))
}

/// The program the plugin asks for, when its `run` permission names it.
fn program(policy: &Policy, text: &str) -> Result<PathBuf, String> {
    if policy.run.is_empty() {
        return Err("it has no `run` permission".into());
    }
    if !text.contains('/') {
        return policy
            .run
            .iter()
            .filter(|run| run.named && run.entry == text)
            .map(|run| run.program.clone())
            .next()
            .ok_or_else(|| "it is not in the plugin's `run` permission".to_string());
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(
            "a relative path never runs; a `run` entry is a bare name or an absolute path".into(),
        );
    }
    let found = canonical(path).ok_or_else(|| "it is not a path that exists".to_string())?;
    policy
        .run
        .iter()
        .filter(|run| run.program == found)
        .map(|run| run.program.clone())
        .next()
        .ok_or_else(|| "it is not in the plugin's `run` permission".to_string())
}

/// The shell a call asks for with `shell: true` or `exec`: the plugin's `run`
/// list must hold `sh`, `/bin/sh`, or the program `sh` resolves to, and that
/// program runs. The wrapper never names a path here, so a plugin that lists a
/// shell of its own cannot point this at another program.
fn shell_program(policy: &Policy) -> Result<PathBuf, String> {
    if policy.run.is_empty() {
        return Err("it has no `run` permission".into());
    }
    let mut shells: Vec<PathBuf> = Vec::new();
    if let Some(found) = on_path("sh") {
        shells.push(found);
    }
    if let Some(found) = canonical(Path::new("/bin/sh")) {
        shells.push(found);
    }
    policy
        .run
        .iter()
        .filter(|run| shells.contains(&run.program))
        .map(|run| run.program.clone())
        .next()
        .ok_or_else(|| {
            "its `run` permission names no shell: a shell needs sh or /bin/sh".to_string()
        })
}

/// The `PATH` of reagent's environment, searched for `name` as an executable
/// file, which is then canonical.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate))
        .and_then(|candidate| canonical(&candidate))
}

/// Whether `path` is a file with an executable bit.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the plugin thread, off the async runtime"
)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The folders a child may write: the write roots of the plugin's `fs`
/// permission that are still the folders the load resolved, and the temporary
/// folder.
fn folders(policy: &Policy) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    match &policy.fs {
        Access::WriteAll => out.push(PathBuf::from("/")),
        Access::Lists { write, .. } => {
            for root in write {
                // A plugin may replace a root with a link to another folder
                // after the load: only the folder the load resolved stays a
                // root, and a link to anything else is no root at all.
                match canonical(root) {
                    Some(now) if now == *root => out.push(now),
                    _ => {}
                }
            }
        }
        Access::None | Access::ReadAll => {}
    }
    if let Some(temp) = canonical(&std::env::temp_dir())
        && !out.contains(&temp)
    {
        out.push(temp);
    }
    out
}

/// The host functions the wrapper calls. A plugin has no name for this module,
/// so it cannot call one.
pub(super) struct ChildHost;

impl ModuleDef for ChildHost {
    fn declare(declare: &Declarations) -> JsResult<()> {
        for name in ["end", "kill", "owner", "pull", "spawn", "wait", "write"] {
            declare.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> JsResult<()> {
        exports.export("owner", Function::new(ctx.clone(), owner)?)?;
        exports.export("spawn", Function::new(ctx.clone(), spawn)?)?;
        exports.export("pull", Function::new(ctx.clone(), pull)?)?;
        exports.export("write", Function::new(ctx.clone(), write)?)?;
        exports.export("end", Function::new(ctx.clone(), end)?)?;
        exports.export("kill", Function::new(ctx.clone(), kill)?)?;
        exports.export("wait", Function::new(ctx.clone(), wait)?)?;
        Ok(())
    }
}

/// The index of the plugin whose runtime runs this call: a runtime holds one
/// plugin's code and no other's, so every frame of it acts for that plugin,
/// `eval` code and the body of a `new Function` included.
fn caller(ctx: &Ctx<'_>) -> Option<usize> {
    super::index_of(ctx)
}

/// `owner()`: the index of the plugin whose runtime runs this call.
fn owner(ctx: Ctx<'_>) -> Option<u32> {
    u32::try_from(caller(&ctx)?).ok()
}

/// The plugin a call of this host may act for: the runtime's plugin, when the
/// child is that plugin's.
fn mine(ctx: &Ctx<'_>, id: u64) -> Option<usize> {
    let found = caller(ctx)?;
    CHILDREN.with(|children| {
        let children = children.borrow();
        let child = children.slots.get(&id)?;
        (child.plugin == found).then_some(found)
    })
}

/// `spawn(spec)`: starts one child through the sandbox helper, or answers the
/// reason it did not start. It answers `{ok: true, id, pid}`, or
/// `{ok: false, message}`, with `type: true` when the plugin's own option is
/// what is wrong and its wrapper throws a `TypeError`.
fn spawn<'js>(ctx: Ctx<'js>, spec: Object<'js>) -> JsResult<Value<'js>> {
    let answer = Object::new(ctx.clone())?;
    match start(&ctx, &spec) {
        Ok((id, pid)) => {
            answer.set("ok", true)?;
            answer.set("id", id)?;
            answer.set("pid", pid)?;
        }
        Err(refusal) => {
            answer.set("ok", false)?;
            if refusal.typed {
                answer.set("type", true)?;
            }
            answer.set("message", refusal.message)?;
        }
    }
    Ok(answer.into_value())
}

/// Why a start did not happen: the message, and whether it is one of the
/// plugin's own options that is wrong, which the wrapper throws as a
/// `TypeError`.
struct Refusal {
    /// The message the wrapper throws.
    message: String,
    /// Whether the wrapper throws it as a `TypeError`.
    typed: bool,
}

impl Refusal {
    /// A refusal of a call the plugin may not make.
    fn plain(message: String) -> Self {
        Self {
            message,
            typed: false,
        }
    }

    /// A refusal of an option the plugin itself wrote.
    fn typed(message: String) -> Self {
        Self {
            message,
            typed: true,
        }
    }
}

impl From<String> for Refusal {
    fn from(message: String) -> Self {
        Self::plain(message)
    }
}

/// Whether a child may be given `name` in its environment: a loader variable
/// runs code, in the helper itself before the sandbox is on, and a name with a
/// `=` or a NUL in it is no name at all.
fn env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('=')
        && !name.contains('\0')
        && !name.starts_with("LD_")
        && !name.starts_with("DYLD_")
        && name != "GCONV_PATH"
}

/// What one start needs, gathered under one borrow of the plugin's checks.
struct Plan {
    /// The sandbox helper.
    helper: PathBuf,
    /// The program, canonical.
    program: PathBuf,
    /// The arguments it takes.
    args: Vec<String>,
    /// The folder it runs in.
    cwd: PathBuf,
    /// The environment it starts with.
    env: Vec<(String, String)>,
    /// Whether it may reach the network.
    network: bool,
    /// The folders it may write.
    writes: Vec<PathBuf>,
    /// The plugin it runs for.
    index: usize,
    /// The plugin's name, for the messages.
    name: String,
}

/// The one child a start makes: its slot, or the reason there is none.
fn start(ctx: &Ctx<'_>, spec: &Object<'_>) -> Result<(u64, u32), Refusal> {
    let Some(index) = owner(ctx.clone()).map(|index| index as usize) else {
        return Err(Refusal::plain(
            "node:child_process runs inside a plugin's own module".into(),
        ));
    };
    let name = super::name_of(index as u32);
    // The wrapper hands over an object with no prototype, so everything read
    // here is its own: a property the plugin put on `Object.prototype` is not
    // seen, and nothing else can stand in for the spec.
    if spec.get_prototype().is_some() {
        return Err(Refusal::typed(format!(
            "plugin {name} sends a start that is no plain object"
        )));
    }
    let wanted: String = spec
        .get("program")
        .map_err(|_| format!("plugin {name} needs a program to run"))?;
    let args: Option<Vec<String>> = spec.get("args").unwrap_or_default();
    let cwd: Option<String> = spec.get("cwd").unwrap_or_default();
    let shell: bool = spec.get("shell").unwrap_or_default();
    let given: Option<Array<'_>> = spec.get("env").unwrap_or_default();
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(given) = given {
        for pair in given.iter::<Array<'_>>() {
            let pair = pair.map_err(|_| format!("plugin {name} sends an env that is no list"))?;
            let key: String = pair
                .get(0)
                .map_err(|_| format!("plugin {name} sends an env name that is no string"))?;
            let value: String = pair
                .get(1)
                .map_err(|_| format!("plugin {name} sends an env value that is no string"))?;
            if !env_name(&key) {
                return Err(Refusal::typed(format!(
                    "plugin {name} may not set the environment variable {key}: a loader variable runs code in the sandbox helper"
                )));
            }
            pairs.push((key, value));
        }
    }
    let plan = NODE.with(|node| {
        let node = node.borrow();
        let policy = node
            .get(index)
            .ok_or_else(|| format!("plugin {name} has no node state"))?;
        let helper = node.helper.clone().ok_or_else(|| {
            format!("plugin {name} may not run {wanted}: no sandbox is on in this run")
        })?;
        let program = match shell {
            true => shell_program(policy),
            false => program(policy, &wanted),
        }
        .map_err(|reason| format!("plugin {name} may not run {wanted}: {reason}"))?;
        let cwd = match &cwd {
            None => policy.session.clone(),
            Some(text) => policy
                .path(text, Kept::Followed)
                .filter(|path| policy.allows(Wanted::Read, path))
                .ok_or_else(|| format!("plugin {name} has no fs read permission for {text}"))?,
        };
        let mut env: Vec<(String, String)> = Vec::with_capacity(pairs.len() + 8);
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                env.push((key.to_string(), value.to_string_lossy().into_owned()));
            }
        }
        // A variable reagent itself holds that the helper would refuse is not
        // handed on: what a plugin lists is checked above, and what is here is
        // reagent's own environment.
        for key in policy.names().into_iter().filter(|key| env_name(key)) {
            if let Some(value) = std::env::var_os(key.as_str()) {
                env.push((key, value.to_string_lossy().into_owned()));
            }
        }
        for (key, value) in &pairs {
            env.push((key.clone(), value.clone()));
        }
        Ok::<Plan, String>(Plan {
            writes: folders(policy),
            network: policy.net.iter().any(|host| matches!(host, Host::Any)),
            helper,
            program,
            args: args.unwrap_or_default(),
            cwd,
            env,
            index,
            name: name.clone(),
        })
    })?;
    let live = CHILDREN.with(|children| children.borrow().live(plan.index));
    if live >= LIMIT {
        return Err(Refusal::plain(format!(
            "plugin {} may not run {wanted}: it holds the {LIMIT} child processes it may",
            plan.name
        )));
    }
    build(&plan)
}

/// Builds one command through the sandbox helper and starts it: its three
/// pipes, its own process group, and the future that reaps it.
///
/// The helper starts with no environment of its own and no variable is given
/// to it: what the child gets reaches it through `--env-clear` and `--env`,
/// which the helper applies after the sandbox is on. So a plugin `env` of
/// `DYLD_INSERT_LIBRARIES` or `LD_PRELOAD` runs no code inside the helper.
fn build(plan: &Plan) -> Result<(u64, u32), Refusal> {
    let mut command = Command::new(&plan.helper);
    command.arg("--sandbox-exec");
    if plan.network {
        command.arg("--network");
    }
    command.arg("--keep-group");
    for folder in &plan.writes {
        command.arg("--write-resolved");
        command.arg(folder);
    }
    command.arg("--env-clear");
    for (key, value) in &plan.env {
        command.arg("--env");
        command.arg(format!("{key}={value}"));
    }
    command.arg("--");
    command.arg(&plan.program);
    for arg in &plan.args {
        command.arg(arg);
    }
    command.current_dir(&plan.cwd);
    command.env_clear();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("plugin {} could not start the sandbox: {error}", plan.name))?;
    let pid = child.id().unwrap_or_default();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let end = Rc::new(End::new());
    let reaper = Rc::clone(&end);
    let slot = Child {
        plugin: plan.index,
        pid,
        started: Instant::now(),
        stdin,
        stdin_closed: false,
        stdout,
        stderr,
        out_end: false,
        err_end: false,
        end,
    };
    let id = CHILDREN.with(|children| children.borrow_mut().add(slot));
    // The wait of the process runs beside the plugin: it reaps the child the
    // moment it ends, so `wait` only waits for this future to fill the answer.
    tokio::task::spawn_local(async move {
        let status = child.wait().await.ok();
        // The group goes with its program: a process it left behind would
        // outlive every later stop, which skips a child that has ended. While
        // a member is left the system cannot hand the group's id to another.
        kill_group(pid, Signal::SIGKILL);
        reaper.finish(status);
        closed(id);
    });
    Ok((id, pid))
}

/// The end of one process: its stdin goes at once, and its slot goes once both
/// its pipes have ended too, so its descriptors close with it.
fn closed(id: u64) {
    CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        if let Some(child) = children.slots.get_mut(&id) {
            child.stdin = None;
            child.stdin_closed = true;
        }
        children.release(id);
    });
}

/// Marks one pipe of a child at its end: the pipe is dropped here, and the
/// slot goes when the process and both pipes have ended as well.
fn end_pipe(id: u64, index: usize, which: u32) {
    CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        {
            let Some(child) = children
                .slots
                .get_mut(&id)
                .filter(|child| child.plugin == index)
            else {
                return;
            };
            match stdout(which) {
                true => {
                    child.stdout = None;
                    child.out_end = true;
                }
                false => {
                    child.stderr = None;
                    child.err_end = true;
                }
            }
        }
        children.release(id);
    });
}

/// `pull(id, which)`: one chunk of the child's stdout (`1`) or stderr (`2`), or
/// `null` at the end of the pipe, or the reason the pipe could not be read.
fn pull<'js>(ctx: Ctx<'js>, id: u64, which: u32) -> JsResult<Value<'js>> {
    let Some(index) = mine(&ctx, id) else {
        return Ok(rquickjs::String::from_str(ctx.clone(), FOREIGN)?.into_value());
    };
    let held = ctx.clone();
    let read = async move {
        let ctx = held;
        let Some(mut pipe) = take_pipe(id, index, which) else {
            return Ok::<Value<'_>, rquickjs::Error>(Value::new_null(ctx.clone()));
        };
        let mut buf = vec![0_u8; CHUNK];
        let read = pipe.read(&mut buf).await;
        match read {
            // The end of the pipe: it is dropped here, and the slot goes with
            // it once the process has ended too.
            Ok(0) => {
                end_pipe(id, index, which);
                Ok(Value::new_null(ctx.clone()))
            }
            Ok(bytes) => {
                put_pipe(id, index, which, pipe);
                buf.truncate(bytes);
                Ok(TypedArray::new_copy(ctx.clone(), buf.as_slice())?.into_value())
            }
            Err(error) => {
                put_pipe(id, index, which, pipe);
                Ok(rquickjs::String::from_str(
                    ctx.clone(),
                    &format!("the pipe of the child could not be read: {error}"),
                )?
                .into_value())
            }
        }
    };
    Promised(read).into_js(&ctx)
}

/// `write(id, chunk)`: one chunk to the child's stdin; `false` once it is
/// closed or its process stopped reading.
fn write<'js>(ctx: Ctx<'js>, id: u64, chunk: Vec<u8>) -> JsResult<Value<'js>> {
    let Some(index) = mine(&ctx, id) else {
        return Ok(Value::new_bool(ctx.clone(), false));
    };
    let sent = async move {
        let Some(mut pipe) = take_stdin(id, index) else {
            return false;
        };
        let out = match pipe.write_all(&chunk).await {
            Ok(()) => pipe.flush().await.is_ok(),
            Err(_) => false,
        };
        put_stdin(id, index, pipe);
        out
    };
    Promised(sent).into_js(&ctx)
}

/// `end(id)`: the end of the child's stdin.
fn end(ctx: Ctx<'_>, id: u64) {
    let Some(index) = mine(&ctx, id) else {
        return;
    };
    let stdin = CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        let child = children
            .slots
            .get_mut(&id)
            .filter(|child| child.plugin == index)?;
        child.stdin_closed = true;
        child.stdin.take()
    });
    drop(stdin);
}

/// `kill(id, signal)`: one signal for the child's whole process group.
fn kill(ctx: Ctx<'_>, id: u64, signal: Opt<Value<'_>>) -> JsResult<bool> {
    let Some(index) = mine(&ctx, id) else {
        return Ok(false);
    };
    let signal = match signal.0 {
        None => Signal::SIGTERM,
        Some(value) if value.is_undefined() || value.is_null() => Signal::SIGTERM,
        Some(value) => match signal_of(&value) {
            Some(signal) => signal,
            None => {
                return Err(Exception::throw_type(
                    &ctx,
                    "a signal is SIGTERM, SIGKILL, SIGINT or SIGHUP, by name or by number",
                ));
            }
        },
    };
    let pid = CHILDREN.with(|children| {
        let children = children.borrow();
        children
            .slots
            .get(&id)
            .filter(|child| child.plugin == index && !child.ended())
            .map(|child| child.pid)
    });
    let Some(pid) = pid else {
        return Ok(false);
    };
    match killpg(Pid::from_raw(pid as i32), signal) {
        Ok(()) => Ok(true),
        // A child that is gone needs no signal, and a start that refused
        // before its pid existed has none.
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(Exception::throw_message(
            &ctx,
            &format!("the child process could not be killed: {error}"),
        )),
    }
}

/// `wait(id)`: the exit of the child, as `{code, signal}`, and `undefined` for
/// a child of another plugin.
fn wait<'js>(ctx: Ctx<'js>, id: u64) -> JsResult<Value<'js>> {
    let Some(index) = mine(&ctx, id) else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    let end = CHILDREN.with(|children| {
        let children = children.borrow();
        children
            .slots
            .get(&id)
            .filter(|child| child.plugin == index)
            .map(|child| Rc::clone(&child.end))
    });
    let Some(end) = end else {
        return Ok(Value::new_undefined(ctx.clone()));
    };
    let held = ctx.clone();
    let waited = async move {
        let code = end.wait().await;
        ended(&held, code)
    };
    Promised(waited).into_js(&ctx)
}

/// The answer of a `wait`: `{code, signal}` with the name of the signal, or
/// `null` when the child ended without one.
fn ended<'js>(ctx: &Ctx<'js>, done: Done) -> JsResult<Value<'js>> {
    let answer = Object::new(ctx.clone())?;
    match done.code {
        Some(code) => answer.set("code", code)?,
        None => answer.set("code", Value::new_null(ctx.clone()))?,
    }
    match done.signal.and_then(signal_name) {
        Some(name) => answer.set("signal", name)?,
        None => answer.set("signal", Value::new_null(ctx.clone()))?,
    }
    Ok(answer.into_value())
}

/// The signal a plugin names, by name or by number.
fn signal_of(value: &Value<'_>) -> Option<Signal> {
    if let Some(number) = value.as_int() {
        return signal_by_number(number);
    }
    let name = value.as_string()?.to_string().ok()?;
    match name.to_uppercase().as_str() {
        "SIGHUP" => Some(Signal::SIGHUP),
        "SIGINT" => Some(Signal::SIGINT),
        "SIGKILL" => Some(Signal::SIGKILL),
        "SIGTERM" => Some(Signal::SIGTERM),
        _ => None,
    }
}

/// The signal a number names.
fn signal_by_number(number: i32) -> Option<Signal> {
    match number {
        1 => Some(Signal::SIGHUP),
        2 => Some(Signal::SIGINT),
        9 => Some(Signal::SIGKILL),
        15 => Some(Signal::SIGTERM),
        _ => None,
    }
}

/// The name of a signal number, as `exit` and `close` spell it.
fn signal_name(number: i32) -> Option<String> {
    match number {
        1 => Some("SIGHUP".into()),
        2 => Some("SIGINT".into()),
        9 => Some("SIGKILL".into()),
        15 => Some("SIGTERM".into()),
        other => Some(format!("SIG{other}")),
    }
}

/// One pipe of a child, taken out of its slot while a pull reads it.
enum Pipe {
    /// The standard output.
    Out(ChildStdout),
    /// The standard error.
    Err(ChildStderr),
}

impl Pipe {
    /// Reads one chunk, `0` at the end of the pipe.
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Pipe::Out(pipe) => pipe.read(buf).await,
            Pipe::Err(pipe) => pipe.read(buf).await,
        }
    }
}

/// Whether `which` names the standard output.
fn stdout(which: u32) -> bool {
    which == 1
}

/// Takes the pipe a pull reads out of its slot, so it may wait without holding
/// the table.
fn take_pipe(id: u64, index: usize, which: u32) -> Option<Pipe> {
    CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        let child = children
            .slots
            .get_mut(&id)
            .filter(|child| child.plugin == index)?;
        match stdout(which) {
            true => child.stdout.take().map(Pipe::Out),
            false => child.stderr.take().map(Pipe::Err),
        }
    })
}

/// Puts the pipe back, so the next pull reads where this one stopped.
fn put_pipe(id: u64, index: usize, which: u32, pipe: Pipe) {
    CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        let Some(child) = children
            .slots
            .get_mut(&id)
            .filter(|child| child.plugin == index)
        else {
            return;
        };
        match (stdout(which), pipe) {
            (true, Pipe::Out(pipe)) => child.stdout = Some(pipe),
            (false, Pipe::Err(pipe)) => child.stderr = Some(pipe),
            _ => {}
        }
    });
}

/// Takes the stdin of a child out of its slot while a write runs.
fn take_stdin(id: u64, index: usize) -> Option<ChildStdin> {
    CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        let child = children
            .slots
            .get_mut(&id)
            .filter(|child| child.plugin == index && !child.stdin_closed)?;
        child.stdin.take()
    })
}

/// Puts the stdin back, unless an `end` closed it meanwhile.
fn put_stdin(id: u64, index: usize, pipe: ChildStdin) {
    CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        let Some(child) = children
            .slots
            .get_mut(&id)
            .filter(|child| child.plugin == index && !child.stdin_closed)
        else {
            return;
        };
        child.stdin = Some(pipe);
    });
}

/// One child process of a plugin, with the pipes its wrapper pulls.
struct Child {
    /// The plugin it runs for.
    plugin: usize,
    /// The pid of the helper, which is the pid of the program.
    pid: u32,
    /// When it started, which a stopped call compares.
    started: Instant,
    /// What the plugin writes to it.
    stdin: Option<ChildStdin>,
    /// Whether the plugin ended the stdin.
    stdin_closed: bool,
    /// What it prints.
    stdout: Option<ChildStdout>,
    /// What it complains with.
    stderr: Option<ChildStderr>,
    /// Whether its standard output reached its end.
    out_end: bool,
    /// Whether its standard error reached its end.
    err_end: bool,
    /// The exit the reaping future fills in.
    end: Rc<End>,
}

impl Child {
    /// Whether its process has exited and been reaped.
    fn ended(&self) -> bool {
        self.end.done().is_some()
    }
}

/// The end of one process, filled by the future that reaps it.
struct End {
    /// The exit, once the process has ended.
    done: RefCell<Option<Done>>,
    /// Wakes the `wait` of the wrapper.
    notify: Notify,
}

impl End {
    fn new() -> Self {
        Self {
            done: RefCell::new(None),
            notify: Notify::new(),
        }
    }

    /// The exit, when the process has ended.
    fn done(&self) -> Option<Done> {
        *self.done.borrow()
    }

    /// Records the exit of the process.
    fn finish(&self, status: Option<std::process::ExitStatus>) {
        use std::os::unix::process::ExitStatusExt;
        let done = match status {
            Some(status) => Done {
                code: status.code(),
                signal: status.signal(),
            },
            None => Done {
                code: None,
                signal: None,
            },
        };
        *self.done.borrow_mut() = Some(done);
        self.notify.notify_waiters();
    }

    /// Waits for the exit of the process.
    async fn wait(&self) -> Done {
        loop {
            if let Some(done) = self.done() {
                return done;
            }
            self.notify.notified().await;
        }
    }
}

/// How one process ended.
#[derive(Clone, Copy)]
struct Done {
    /// The exit code, `None` when a signal ended it.
    code: Option<i32>,
    /// The signal that ended it, `None` when it exited.
    signal: Option<i32>,
}

/// Every child of the plugin thread, by id. Rust data only, so nothing here is
/// reachable from JS.
struct ChildState {
    /// The children that have not been forgotten, by id: an id names one child
    /// for the life of the thread, so a late kill of an old one touches nothing.
    slots: HashMap<u64, Child>,
    /// The next id to hand out, never reused.
    next: u64,
}

impl ChildState {
    fn new() -> Self {
        Self {
            slots: HashMap::new(),
            next: 0,
        }
    }

    /// How many children of this plugin still hold a slot.
    fn live(&self, index: usize) -> usize {
        // A slot stays while its process runs or a pipe is still open: a
        // process that left its group (macOS `setsid`) holds the pipes after
        // the program ended, so each such slot still counts.
        self.slots
            .values()
            .filter(|child| child.plugin == index)
            .count()
    }

    /// Adds one child and answers its id.
    fn add(&mut self, child: Child) -> u64 {
        let id = self.next;
        self.next += 1;
        self.slots.insert(id, child);
        id
    }

    /// Drops the slot of a child whose process has ended and whose standard
    /// output and error have both reached their end, so the descriptors it held
    /// close. A slot that stays would keep its pipes open for the life of the
    /// thread; the id it was handed is never given to another child either.
    fn release(&mut self, id: u64) {
        let done = self
            .slots
            .get(&id)
            .is_some_and(|child| child.ended() && child.out_end && child.err_end);
        if done {
            self.slots.remove(&id);
        }
    }
}

thread_local! {
    /// The children of the plugin thread, one entry per plugin index.
    static CHILDREN: RefCell<ChildState> = RefCell::new(ChildState::new());
}

/// How many slots the plugin thread still holds, which a test reads to see that
/// a finished child gives its descriptors back.
#[cfg(test)]
pub(super) fn count() -> usize {
    CHILDREN.with(|children| children.borrow().slots.len())
}

/// Kills every child of a call that was stopped: the ones the plugin at
/// `index` started after the call began, with `SIGKILL` for the whole process
/// group. The index decides, never a name: two plugins may share one, and a
/// stop may not reach the child of another.
pub(super) fn stopped(index: usize, since: Instant) {
    let pids: Vec<u32> = CHILDREN.with(|children| {
        let children = children.borrow();
        children
            .slots
            .values()
            .filter(|child| child.plugin == index && child.started >= since && !child.ended())
            .map(|child| child.pid)
            .collect()
    });
    for pid in pids {
        kill_group(pid, Signal::SIGKILL);
    }
}

/// Kills every child of one plugin and reaps it: nothing of it may outlive its
/// context.
pub(super) fn free(index: usize) {
    let slots: Vec<Child> = CHILDREN.with(|children| {
        let mut children = children.borrow_mut();
        let ids: Vec<u64> = children
            .slots
            .iter()
            .filter(|(_, child)| child.plugin == index)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| children.slots.remove(&id))
            .collect()
    });
    for child in &slots {
        // A child that has ended has no pid of its own anymore: the system may
        // have given it to another process group, which this signal would hit
        // instead. The reaping future has already taken its exit.
        if child.ended() {
            continue;
        }
        kill_group(child.pid, Signal::SIGKILL);
        let _ = nix::sys::wait::waitpid(Pid::from_raw(child.pid as i32), None);
    }
}

/// Sends one signal to the whole process group of `pid`.
fn kill_group(pid: u32, signal: Signal) {
    let _ = killpg(Pid::from_raw(pid as i32), signal);
}

/// `path` with every link resolved, when it exists at all.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the plugin thread, off the async runtime"
)]
fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Run, shell_note};

    /// On Linux `sh` is often a link to `dash`: the note names what the plugin
    /// wrote, not the file it resolves to.
    #[test]
    fn shell_note_names_the_listed_entry() {
        let runs = [Run {
            entry: "sh".to_string(),
            named: true,
            program: PathBuf::from("/usr/bin/dash"),
        }];
        assert_eq!(
            shell_note("runs", &runs).as_deref(),
            Some("plugin runs lists the shell sh: it can then run any program")
        );
    }
}
