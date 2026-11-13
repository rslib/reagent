//! The Node modules a plugin may import: LLRT's own, behind a check that runs
//! in every call that touches files, the environment or the process.
//!
//! A plugin names a Node module by its Node name and gets a wrapper module of
//! its own, `reagent:node/<name>?plugin=<index>`, so a check knows which plugin
//! calls. The wrapper imports the raw module, `reagent:raw/<name>`, which only
//! a wrapper may import, and the guard module of its plugin, which holds the one
//! host function a check calls: a plugin has no name for either.
//!
//! Without the `node` feature every one of these names refuses the plugin at
//! load, with a note that says this build has no Node modules.

use rquickjs::{Ctx, Exception, Result as JsResult};

#[cfg(feature = "node")]
use std::cell::RefCell;
#[cfg(feature = "node")]
use std::ffi::OsString;
use std::path::Path;

#[cfg(feature = "node")]
use std::path::PathBuf;

#[cfg(feature = "node")]
use rquickjs::function::Opt;
#[cfg(feature = "node")]
use rquickjs::module::{Declarations, Declared, Exports, ModuleDef};
#[cfg(feature = "node")]
use rquickjs::object::Property;
#[cfg(feature = "node")]
use rquickjs::{Function, Module, Object, Persistent, Undefined, Value};
#[cfg(feature = "node")]
use sha2::{Digest as _, Sha256};

use super::manifest::Manifest;

#[cfg(feature = "node")]
mod child;

#[cfg(feature = "node")]
mod net;

#[cfg(feature = "node")]
use super::manifest::{Env, Fs, Host, Root};

/// The Node modules a plugin may import, without the `node:` prefix.
const MODULES: [&str; 16] = [
    "path",
    "buffer",
    "events",
    "url",
    "timers",
    "timers/promises",
    "util",
    "util/types",
    "os",
    "crypto",
    "process",
    "fs",
    "fs/promises",
    "child_process",
    "constants",
    "diagnostics_channel",
];

/// The Node modules this build never gives a plugin: the ones that would leave
/// the sandbox or end the worker.
const OTHERS: &[&str] = &[
    "assert",
    "async_hooks",
    "cluster",
    "console",
    "dgram",
    "dns",
    "domain",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "perf_hooks",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "sea",
    "sqlite",
    "stream",
    "string_decoder",
    "sys",
    "test",
    "tls",
    "trace_events",
    "tty",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

/// The prefix of the wrapper of one plugin's module, then `<name>?plugin=<i>`.
#[cfg(feature = "node")]
const WRAPPER: &str = "reagent:node/";

/// The prefix of the raw module a wrapper wraps.
#[cfg(feature = "node")]
const RAW: &str = "reagent:raw/";

/// The guard module of one plugin, then `?plugin=<index>`: the only thing a
/// wrapper imports besides its raw modules.
#[cfg(feature = "node")]
const GUARD: &str = "reagent:raw/guard";

/// What a guard module imports: the host functions the wrappers call.
#[cfg(feature = "node")]
const CHECK: &str = "reagent:raw/check";

/// The module `name` a plugin's own file imports, or the reason it may not have
/// it.
///
/// `None` leaves a relative path and a bare name of another kind to the rule of
/// the plugin folder.
pub(super) fn resolve(ctx: &Ctx<'_>, base: &str, name: &str) -> Option<JsResult<String>> {
    if let Some(found) = wrapped(ctx, base, name) {
        return Some(found);
    }
    if name.starts_with("reagent:") {
        return Some(Err(unavailable(ctx, name)));
    }
    let module = named(name)?;
    wrapper(ctx, base, name, module)
}

/// The raw module a wrapper imports, and the sibling wrapper it wraps, and
/// nothing else.
#[cfg(feature = "node")]
fn wrapped(ctx: &Ctx<'_>, base: &str, name: &str) -> Option<JsResult<String>> {
    if let Some(rest) = base.strip_prefix(WRAPPER) {
        let (_, index) = rest.split_once("?plugin=")?;
        if let Some(sibling) = name.strip_prefix(WRAPPER) {
            return Some(match source(sibling) {
                Some(_) => Ok(format!("{WRAPPER}{sibling}?plugin={index}")),
                None => Err(unavailable(ctx, name)),
            });
        }
        if name == GUARD {
            return Some(Ok(format!("{GUARD}?plugin={index}")));
        }
        if let Some(module) = name.strip_prefix(RAW)
            && (MODULES.contains(&module) || module == "check")
        {
            return Some(Ok(name.to_string()));
        }
        if name == child::CHILD_HOST {
            return Some(Ok(name.to_string()));
        }
        return Some(Err(unavailable(ctx, name)));
    }
    if base.starts_with(GUARD) {
        return Some(if name == CHECK {
            Ok(CHECK.to_string())
        } else {
            Err(unavailable(ctx, name))
        });
    }
    if base == net::NET {
        return Some(if name == net::NET_HOST {
            Ok(net::NET_HOST.to_string())
        } else {
            Err(unavailable(ctx, name))
        });
    }
    None
}

/// A build without the Node modules serves no wrapper and no raw module.
#[cfg(not(feature = "node"))]
fn wrapped(_ctx: &Ctx<'_>, _base: &str, _name: &str) -> Option<JsResult<String>> {
    None
}

/// The wrapper of the Node module `name` names, or the reason a plugin may not
/// have it.
#[cfg(feature = "node")]
fn wrapper(ctx: &Ctx<'_>, base: &str, name: &str, module: &str) -> Option<JsResult<String>> {
    if !MODULES.contains(&module) {
        return Some(Err(unavailable(ctx, name)));
    }
    Some(match index_of(base) {
        Some(index) => Ok(format!("{WRAPPER}{module}?plugin={index}")),
        None => Err(unavailable(ctx, name)),
    })
}

/// A build without the Node modules refuses every one of these names.
#[cfg(not(feature = "node"))]
fn wrapper(ctx: &Ctx<'_>, _base: &str, name: &str, _module: &str) -> Option<JsResult<String>> {
    let reason =
        format!("{name} needs a build with the node modules: this build has no Node modules");
    Some(Err(Exception::throw_message(ctx, &reason)))
}

/// The Node module `name` names, without its `node:` prefix, when it names one.
fn named(name: &str) -> Option<&str> {
    let bare = name.strip_prefix("node:").unwrap_or(name);
    let node = name.starts_with("node:") || MODULES.contains(&bare) || OTHERS.contains(&bare);
    node.then_some(bare)
}

/// The error of a name that may not be imported at all.
fn unavailable(ctx: &Ctx<'_>, name: &str) -> rquickjs::Error {
    Exception::throw_message(ctx, &format!("{name} is not available to plugins"))
}

/// The index of the plugin whose folder holds `base`, the module that imports.
#[cfg(feature = "node")]
fn index_of(base: &str) -> Option<usize> {
    let base = Path::new(base);
    NODE.with(|node| {
        let node = node.borrow();
        node.plugins
            .iter()
            .enumerate()
            .filter_map(|(index, policy)| Some((index, policy.as_ref()?)))
            .filter(|(_, policy)| base.starts_with(&policy.own))
            .max_by_key(|(_, policy)| policy.own.as_os_str().len())
            .map(|(index, _)| index)
    })
}

/// The declared module of one `reagent:` name, when the name is one of ours.
#[cfg(feature = "node")]
pub(super) fn load<'js>(ctx: &Ctx<'js>, name: &str) -> Option<JsResult<Module<'js, Declared>>> {
    if let Some(rest) = name.strip_prefix(WRAPPER) {
        let (module, _) = rest.split_once("?plugin=")?;
        if !sealed() {
            return Some(Err(Exception::throw_message(
                ctx,
                "the node modules did not start",
            )));
        }
        let source = source(module)?;
        return Some(Module::declare(ctx.clone(), name, source));
    }
    if let Some(index) = name
        .strip_prefix(GUARD)
        .and_then(|rest| rest.strip_prefix("?plugin="))
    {
        let source = format!(
            "import {{ check, checkLast, checkOpen, checkTemp, chownHandle, deny, env, envHas, envNames, opensWrite }} from \"{CHECK}\";\n\
             export {{ check, checkLast, checkOpen, checkTemp, chownHandle, deny, env, envHas, envNames, opensWrite }};\n\
             export const index = {index};\n"
        );
        return Some(Module::declare(ctx.clone(), name, source));
    }
    if name == CHECK {
        return Some(Module::declare_def::<Checks, _>(ctx.clone(), name));
    }
    if name == net::NET_HOST {
        return Some(Module::declare_def::<net::NetHost, _>(ctx.clone(), name));
    }
    if name == child::CHILD_HOST {
        return Some(Module::declare_def::<child::ChildHost, _>(
            ctx.clone(),
            name,
        ));
    }
    raw(ctx, name, name.strip_prefix(RAW)?)
}

/// The raw module of one Node module: LLRT's own, unchanged, and `process`,
/// whose exports LLRT reads out of a global.
#[cfg(feature = "node")]
fn raw<'js>(ctx: &Ctx<'js>, name: &str, module: &str) -> Option<JsResult<Module<'js, Declared>>> {
    Some(match module {
        "path" => declare::<llrt_modules::path::PathModule>(ctx, name),
        "buffer" => declare::<llrt_modules::buffer::BufferModule>(ctx, name),
        "events" => declare::<llrt_modules::events::EventsModule>(ctx, name),
        "url" => declare::<llrt_modules::url::UrlModule>(ctx, name),
        "util" => declare::<llrt_modules::util::UtilModule>(ctx, name),
        "os" => declare::<llrt_modules::os::OsModule>(ctx, name),
        "crypto" => declare::<llrt_modules::crypto::CryptoModule>(ctx, name),
        "process" => declare::<Process>(ctx, name),
        "fs" => declare::<llrt_modules::fs::FsModule>(ctx, name),
        "fs/promises" => declare::<llrt_modules::fs::FsPromisesModule>(ctx, name),
        _ => return None,
    })
}

/// One raw module, declared but not evaluated.
#[cfg(feature = "node")]
fn declare<'js, D: ModuleDef>(ctx: &Ctx<'js>, name: &str) -> JsResult<Module<'js, Declared>> {
    Module::declare_def::<D, _>(ctx.clone(), name)
}

/// The wrapper source of one module.
#[cfg(feature = "node")]
fn source(module: &str) -> Option<&'static str> {
    Some(match module {
        "path" => include_str!("path.js"),
        "buffer" => include_str!("buffer.js"),
        "events" => include_str!("events.js"),
        "url" => include_str!("url.js"),
        "timers" => include_str!("timers.js"),
        "timers/promises" => include_str!("shims/timers-promises.js"),
        "util" => include_str!("util.js"),
        "util/types" => include_str!("shims/util-types.js"),
        "os" => include_str!("os.js"),
        "crypto" => include_str!("crypto.js"),
        "process" => include_str!("process.js"),
        "fs" => include_str!("fs.js"),
        "fs/promises" => include_str!("fs-promises.js"),
        "child_process" => include_str!("child_process.js"),
        "constants" => include_str!("shims/constants.js"),
        "diagnostics_channel" => include_str!("shims/diagnostics-channel.js"),
        _ => return None,
    })
}

/// LLRT's `process` module, whose exports it reads out of the `process` global:
/// the object goes back into the globals only while this module fills its
/// exports, and out of them again before any plugin code runs.
#[cfg(feature = "node")]
struct Process;

#[cfg(feature = "node")]
impl ModuleDef for Process {
    fn declare(declare: &Declarations) -> JsResult<()> {
        <llrt_modules::process::ProcessModule as ModuleDef>::declare(declare)
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> JsResult<()> {
        let Some(process) = held(ctx) else {
            return Err(Exception::throw_message(ctx, "the process object is gone"));
        };
        ctx.globals().set("process", process)?;
        let out = llrt_modules::process::ProcessModule::evaluate(ctx, exports);
        let removed = ctx.globals().remove("process");
        out?;
        removed
    }
}

/// The `process` object LLRT built, out of the globals and out of the reach of
/// plugin code.
#[cfg(feature = "node")]
fn held<'js>(ctx: &Ctx<'js>) -> Option<Object<'js>> {
    NODE.with(|node| {
        let node = node.borrow();
        node.process.as_ref()?.clone().restore(ctx).ok()
    })
}

/// The host functions the wrappers call: the one place a file access, an
/// environment read and a refused process call are decided. A plugin has no
/// name for this module, so it cannot call them.
#[cfg(feature = "node")]
struct Checks;

#[cfg(feature = "node")]
impl ModuleDef for Checks {
    fn declare(declare: &Declarations) -> JsResult<()> {
        for name in [
            "check",
            "checkLast",
            "checkOpen",
            "checkTemp",
            "chownHandle",
            "deny",
            "env",
            "envHas",
            "envNames",
            "opensWrite",
        ] {
            declare.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> JsResult<()> {
        exports.export("check", Function::new(ctx.clone(), fs_check)?)?;
        exports.export("checkLast", Function::new(ctx.clone(), fs_check_last)?)?;
        exports.export("checkOpen", Function::new(ctx.clone(), fs_check_open)?)?;
        exports.export("checkTemp", Function::new(ctx.clone(), fs_check_temp)?)?;
        exports.export("chownHandle", Function::new(ctx.clone(), chown_handle)?)?;
        exports.export("deny", Function::new(ctx.clone(), deny_call)?)?;
        exports.export("env", Function::new(ctx.clone(), env_check)?)?;
        exports.export("envHas", Function::new(ctx.clone(), env_has)?)?;
        exports.export("envNames", Function::new(ctx.clone(), env_names)?)?;
        exports.export("opensWrite", Function::new(ctx.clone(), opens_write)?)?;
        Ok(())
    }
}

/// `check(index, access, path)`: what a wrapper calls before it touches a file.
/// It answers the path to use, with every link it has resolved, so the raw
/// function touches what it checked.
#[cfg(feature = "node")]
fn fs_check(ctx: Ctx<'_>, index: u32, access: String, path: String) -> JsResult<String> {
    check_call(&ctx, index, Wanted::of(&access), &path, Kept::Followed)
}

/// `checkLast(index, access, path)`: the same check for a call that works on the
/// entry itself, so only the folder resolves and the last name stays as given.
#[cfg(feature = "node")]
fn fs_check_last(ctx: Ctx<'_>, index: u32, access: String, path: String) -> JsResult<String> {
    check_call(&ctx, index, Wanted::of(&access), &path, Kept::Entry)
}

/// `checkOpen(index, path, flags)`: an `open` needs write for every flag but the
/// read-only ones, and any flag this does not know needs write.
#[cfg(feature = "node")]
fn fs_check_open<'js>(
    ctx: Ctx<'js>,
    index: u32,
    path: String,
    flags: Opt<Value<'js>>,
) -> JsResult<String> {
    let wanted = match writes(flags.0) {
        true => Wanted::Write,
        false => Wanted::Read,
    };
    check_call(&ctx, index, wanted, &path, Kept::Followed)
}

/// `opensWrite(flags)`: whether the flags of an `open` name a write, which is
/// also which handle methods a plugin gets.
#[cfg(feature = "node")]
fn opens_write(flags: Opt<Value<'_>>) -> bool {
    writes(flags.0)
}

/// `checkTemp(index, prefix)`: `mkdtemp` makes a folder next to the prefix, named
/// after its last name and a random suffix of its own, so the check names that
/// folder and answers the plain prefix, which the raw call adds its own suffix
/// to.
#[cfg(feature = "node")]
fn fs_check_temp(ctx: Ctx<'_>, index: u32, prefix: String) -> JsResult<String> {
    let wanted = Wanted::Write;
    let allowed = NODE.with(|node| {
        let node = node.borrow();
        let policy = node.get(index as usize)?;
        let folded = policy.expand(&prefix)?;
        let (parent, name) = match (folded.file_name(), folded.parent()) {
            (Some(name), Some(parent)) => (parent.to_path_buf(), name.to_os_string()),
            // A prefix with no last name makes the folder beside itself.
            _ => (folded, OsString::new()),
        };
        let settled = resolved(&parent)?;
        let mut checked = settled.clone();
        checked.push(format!("{},{SUFFIX}", name.to_string_lossy()));
        if !policy.allows(wanted, &checked) {
            return None;
        }
        let mut plain = settled;
        if !name.is_empty() {
            plain.push(name);
        }
        Some(plain)
    });
    match allowed {
        Some(path) => Ok(path.to_string_lossy().into_owned()),
        None => Err(Exception::throw_message(
            &ctx,
            &refused(index, wanted, &prefix),
        )),
    }
}

/// `env(index, name)`: what the `env` proxy reads a name with: `undefined` for a
/// name every object answers, and the value of every other name the plugin may
/// read.
#[cfg(feature = "node")]
fn env_check(ctx: Ctx<'_>, index: u32, name: String) -> JsResult<Option<String>> {
    match env_value(index, &name) {
        Ok(value) => Ok(value),
        Err(message) => Err(Exception::throw_message(&ctx, &message)),
    }
}

/// `envHas(index, name)`: whether the plugin may read `name` and the process has
/// it set.
#[cfg(feature = "node")]
fn env_has(ctx: Ctx<'_>, index: u32, name: String) -> JsResult<bool> {
    match env_value(index, &name) {
        Ok(value) => Ok(value.is_some()),
        Err(message) => Err(Exception::throw_message(&ctx, &message)),
    }
}

/// `envNames(index)`: the names of the permission the process has set.
#[cfg(feature = "node")]
fn env_names(index: u32) -> Vec<String> {
    NODE.with(|node| {
        let node = node.borrow();
        node.get(index as usize)
            .map(Policy::names)
            .unwrap_or_default()
    })
}

/// `deny(index, what)`: the throw of a process call a plugin may not make.
#[cfg(feature = "node")]
fn deny_call(ctx: Ctx<'_>, index: u32, what: String) -> JsResult<()> {
    let reason = format!("plugin {} may not call {what}", name_of(index));
    Err(Exception::throw_message(&ctx, &reason))
}

/// The path a raw function must use, and the message a refused access throws.
#[cfg(feature = "node")]
fn check_call(
    ctx: &Ctx<'_>,
    index: u32,
    wanted: Wanted,
    path: &str,
    kept: Kept,
) -> JsResult<String> {
    match checked(index, wanted, path, kept) {
        Ok(path) => Ok(path),
        Err(message) => Err(Exception::throw_message(ctx, &message)),
    }
}

/// The path a raw function must use, or the message a refused access throws.
#[cfg(feature = "node")]
fn checked(index: u32, wanted: Wanted, path: &str, kept: Kept) -> Result<String, String> {
    let allowed = NODE.with(|node| {
        let node = node.borrow();
        let policy = node.get(index as usize);
        let path = policy.and_then(|policy| policy.path(path, kept))?;
        policy
            .filter(|policy| policy.allows(wanted, &path))
            .map(|_| path)
    });
    match allowed {
        Some(path) => Ok(path.to_string_lossy().into_owned()),
        None => Err(refused(index, wanted, path)),
    }
}

/// The message a refused file access throws.
#[cfg(feature = "node")]
fn refused(index: u32, wanted: Wanted, path: &str) -> String {
    format!(
        "plugin {} has no fs {} permission for {path}",
        name_of(index),
        wanted.as_str()
    )
}

/// The value of the environment variable `name`, `None` when it is a name every
/// object answers or the process does not set it, and the message of a refusal.
#[cfg(feature = "node")]
fn env_value(index: u32, name: &str) -> Result<Option<String>, String> {
    if PROTOTYPE.contains(&name) {
        return Ok(None);
    }
    let plugin = name_of(index);
    let allowed = NODE.with(|node| {
        let node = node.borrow();
        node.get(index as usize)
            .is_some_and(|policy| policy.reads(name))
    });
    if !allowed {
        return Err(format!("plugin {plugin} has no env permission for {name}"));
    }
    Ok(std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()))
}

/// The name of the plugin at `index`, for the messages of its checks.
#[cfg(feature = "node")]
fn name_of(index: u32) -> String {
    NODE.with(|node| {
        let node = node.borrow();
        node.get(index as usize)
            .map_or_else(|| index.to_string(), |policy| policy.name.clone())
    })
}

/// Sets the globals the Node modules need, shuts the lookup a promise makes
/// before it hands over a value, and takes the `process` object back out of the
/// globals so no plugin code sees one.
#[cfg(feature = "node")]
pub(super) async fn setup<'js>(ctx: &Ctx<'js>) -> JsResult<Held> {
    // Made first, so a failed step below still frees what it saved.
    let held = Held;
    no_then(ctx)?;
    llrt_modules::buffer::init(ctx)?;
    llrt_modules::url::init(ctx)?;
    llrt_modules::events::init(ctx)?;
    llrt_modules::util::init(ctx)?;
    llrt_modules::crypto::init(ctx)?;
    llrt_modules::process::init(ctx)?;
    // The `fetch` of the plugins: the abort signal and the stream of a body it
    // reads, then the web objects of the wrapper itself.
    llrt_modules::exceptions::init(ctx)?;
    llrt_modules::abort::init(ctx)?;
    llrt_modules::stream_web::init(ctx)?;
    fetch(ctx).await?;
    let process: Object = ctx.globals().get("process")?;
    ctx.globals().remove("process")?;
    NODE.with(|node| node.borrow_mut().process = Some(Persistent::save(ctx, process)));
    seal_handles(ctx).await?;
    Ok(held)
}

/// Sets the `fetch`, `Headers`, `Request` and `Response` globals of the
/// context, from the wrapper module the net host backs.
#[cfg(feature = "node")]
async fn fetch(ctx: &Ctx<'_>) -> JsResult<()> {
    let module = Module::declare(ctx.clone(), net::NET, include_str!("fetch.js"))?;
    let (module, done) = module.eval()?;
    done.into_future::<()>().await?;
    let globals = ctx.globals();
    for name in ["FormData", "Headers", "Request", "Response", "fetch"] {
        globals.set(name, module.get::<_, Value>(name)?)?;
    }
    Ok(())
}

/// Makes a raw `FileHandle` harmless in a plugin's hands, before any plugin
/// code runs.
///
/// `open` resolves a raw handle through a promise, and a plugin that patches
/// `Promise.prototype` (or anything else a resolve looks up) sees the value
/// before the facade wraps it. So the raw handle itself must give nothing a
/// check guards: `chmod` and `chown` leave its prototype (the facade checks the
/// path and calls the saved `chown`), the prototype gets an own `then` of
/// `undefined`, and it is frozen. What is left reads or writes through the
/// descriptor, and the OS refuses a write on one opened for reading.
#[cfg(feature = "node")]
async fn seal_handles(ctx: &Ctx<'_>) -> JsResult<()> {
    let module = Module::declare_def::<llrt_modules::fs::FsPromisesModule, _>(
        ctx.clone(),
        "reagent:seal/fs/promises",
    )?;
    let (module, done) = module.eval()?;
    done.into_future::<()>().await?;
    let open: Function = module.get("open")?;
    // LLRT names no handle class a host can reach, so one real handle hands
    // over the prototype every later handle shares.
    let opened: rquickjs::Promise = open.call(("/dev/null",))?;
    let handle: Object = opened.into_future().await?;
    let prototype = handle
        .get_prototype()
        .ok_or_else(|| Exception::throw_message(ctx, "a file handle has no prototype"))?;
    let close: Function = prototype.get("close")?;
    let closed: rquickjs::Promise = close.call((rquickjs::function::This(handle),))?;
    closed.into_future::<()>().await?;
    let chown: Function = prototype.get("chown")?;
    for name in ["chmod", "chown"] {
        let refused = Function::new(ctx.clone(), move |ctx: Ctx<'_>| -> JsResult<()> {
            Err(Exception::throw_message(
                &ctx,
                &format!("a plugin calls {name} through the fs module or its open facade"),
            ))
        })?;
        prototype.prop(name, Property::from(refused))?;
    }
    prototype.prop("then", Property::from(Undefined))?;
    let object: Object = ctx.globals().get("Object")?;
    let freeze: Function = object.get("freeze")?;
    freeze.call::<_, ()>((prototype,))?;
    NODE.with(|node| node.borrow_mut().chown = Some(Persistent::save(ctx, chown)));
    Ok(())
}

/// Whether the file handles are sealed, without which no wrapper loads.
#[cfg(feature = "node")]
fn sealed() -> bool {
    NODE.with(|node| node.borrow().chown.is_some())
}

/// `chownHandle(handle, uid, gid)`: LLRT's own `chown` on a facade's handle,
/// which the facade calls after its write check.
#[cfg(feature = "node")]
fn chown_handle<'js>(
    ctx: Ctx<'js>,
    handle: Object<'js>,
    uid: Value<'js>,
    gid: Value<'js>,
) -> JsResult<Value<'js>> {
    let chown = NODE
        .with(|node| node.borrow().chown.clone())
        .ok_or_else(|| Exception::throw_message(&ctx, "the file handles are not sealed"))?
        .restore(&ctx)?;
    chown.call((rquickjs::function::This(handle), uid, gid))
}

/// Gives `Object.prototype` a `then` of its own, `undefined`, not writable and
/// not configurable.
///
/// A promise that resolves with one of LLRT's class instances hands it over
/// through the value's `then`: `open` resolves a `FileHandle`, `stat` a `Stats`,
/// `readdir` of a folder a `Dirent`, and a plugin that defines
/// `Object.prototype.then` as a getter gets the raw instance as the `this` of
/// the function it answers, however the wrapper awaits it. The classes are
/// LLRT's own and out of reach here, and their prototype's chain ends at
/// `Object.prototype`, whose own prototype cannot be set, so this one property
/// shuts the lookup for every value a wrapper awaits. Every other object that
/// inherits its `then` from `Object.prototype` finds none, which is what an
/// object without a `then` of its own has always been.
#[cfg(feature = "node")]
fn no_then(ctx: &Ctx<'_>) -> JsResult<()> {
    let prototype = Object::new(ctx.clone())?
        .get_prototype()
        .ok_or_else(|| Exception::throw_message(ctx, "Object.prototype is out of reach"))?;
    prototype.prop("then", Property::from(Undefined))
}

/// The Node state of one context, freed before the context ends: a `Persistent`
/// that outlives its runtime aborts the process.
#[cfg(feature = "node")]
#[must_use = "the state is freed when this drops, which must be before its context"]
pub(super) struct Held;

#[cfg(feature = "node")]
impl Drop for Held {
    fn drop(&mut self) {
        child::free();
        net::free();
        NODE.with(|node| *node.borrow_mut() = State::new());
    }
}

/// Sets the folder a relative path of a plugin resolves against.
#[cfg(feature = "node")]
pub(super) fn session(path: &Path) {
    let session = settled(path);
    NODE.with(|node| node.borrow_mut().session = Some(session));
}

/// A build without the Node modules resolves no path of a plugin.
#[cfg(not(feature = "node"))]
pub(super) fn session(_path: &Path) {}

/// Sets the state folder the sessions use, where a plugin's cookies are saved.
#[cfg(feature = "node")]
pub(super) fn state(path: &Path) {
    let state = path.to_path_buf();
    NODE.with(|node| node.borrow_mut().state = Some(state));
}

/// A build without the Node modules saves no cookie.
#[cfg(not(feature = "node"))]
pub(super) fn state(_path: &Path) {}

/// Sets the sandbox helper every child process of a plugin starts through, or
/// clears it when this run has no sandbox.
#[cfg(feature = "node")]
pub(super) fn sandbox(path: Option<PathBuf>) {
    NODE.with(|node| node.borrow_mut().helper = path);
}

/// A build without the Node modules starts no child process.
#[cfg(not(feature = "node"))]
pub(super) fn sandbox(_path: Option<std::path::PathBuf>) {}

/// Kills every child process the plugin at `index` started since `since`: a
/// call the watchdog stopped leaves no process behind.
#[cfg(feature = "node")]
pub(super) fn stopped(index: usize, since: std::time::Instant) {
    child::stopped(index, since);
}

/// A build without the Node modules starts no child process.
#[cfg(not(feature = "node"))]
pub(super) fn stopped(_index: usize, _since: std::time::Instant) {}

/// Registers the checks of the plugin at `index`, resolving its permission
/// roots and the programs of its `run` list once. An entry that names no
/// program is the reason the plugin is refused; the notes are the ones its
/// permissions raise.
#[cfg(feature = "node")]
pub(super) fn register(index: usize, manifest: &Manifest) -> Result<Vec<String>, String> {
    let home = home();
    // Every entry of the `run` permission is resolved once, here: an entry
    // that resolves to no program refuses the plugin.
    let runs = child::runs(&manifest.permissions.run, &manifest.name)?;
    let mut lines = NODE.with(|node| {
        let mut node = node.borrow_mut();
        let Some(session) = node.session.clone() else {
            return Ok::<Vec<String>, String>(Vec::new());
        };
        let fs = access(&manifest.permissions.fs, &session, home.as_deref());
        let env = match &manifest.permissions.env {
            Env::None => Vars::None,
            Env::All => Vars::All,
            Env::Names(names) => Vars::Names(names.clone()),
        };
        let policy = Policy {
            name: manifest.name.clone(),
            own: manifest.dir.clone(),
            fs,
            env,
            net: manifest.permissions.net.clone(),
            run: runs.runs,
            session,
            home,
        };
        if node.plugins.len() <= index {
            node.plugins.resize_with(index + 1, || None);
        }
        node.plugins[index] = Some(policy);
        Ok(runs.note.into_iter().collect::<Vec<String>>())
    })?;
    if let Some(note) = open_jar(index) {
        lines.push(note);
    }
    Ok(lines)
}

/// Opens the jar of the plugin at `index`, at `<state>/plugins/<safe name>-<hash>`
/// of its folder, and answers the note a file it cannot read raised.
#[cfg(feature = "node")]
fn open_jar(index: usize) -> Option<String> {
    let folder = NODE.with(|node| {
        let node = node.borrow();
        let state = node.state.clone()?;
        let policy = node.get(index)?;
        let name = jar_name(&policy.name, &policy.own);
        Some(state.join("plugins").join(name))
    })?;
    net::open_jar(index, &folder)
}

/// The folder of one plugin's jar: the name the plugin chose, cut down to the
/// letters of a file name, and the fingerprint of the folder the plugin was
/// loaded from.
///
/// The name is the plugin's own, so it is never a path: only `[A-Za-z0-9._-]`
/// stay, 40 at most, and a name that is nothing but dots stands in as `plugin`,
/// so no name reaches out of `<state>/plugins/`. The tag of the folder tells two
/// plugins of one name apart, so two plugins never share a jar.
#[cfg(feature = "node")]
fn jar_name(name: &str, own: &Path) -> String {
    let mut safe = String::with_capacity(name.len());
    for letter in name.chars() {
        safe.push(match letter {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => letter,
            _ => '_',
        });
    }
    safe.truncate(40);
    if safe.chars().all(|letter| letter == '.') {
        safe = "plugin".to_string();
    }
    let digest = Sha256::digest(own.to_string_lossy().as_bytes());
    let mut tag = String::with_capacity(16);
    for byte in &digest[..8] {
        tag.push_str(&format!("{byte:02x}"));
    }
    format!("{safe}-{tag}")
}

/// A build without the Node modules has no check to register.
#[cfg(not(feature = "node"))]
pub(super) fn register(_index: usize, _manifest: &Manifest) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

/// The home folder a path starting with `~` resolves against.
#[cfg(feature = "node")]
fn home() -> Option<PathBuf> {
    #[cfg(test)]
    if let Ok(named) = TEST_HOME.lock()
        && let Some(home) = named.clone()
    {
        return Some(home);
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The home folder the tests name, so a `~` root resolves there without a change
/// to the environment every other test reads.
#[cfg(all(feature = "node", test))]
static TEST_HOME: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Names the home folder a `~` root resolves against, or clears the name.
#[cfg(all(feature = "node", test))]
pub(super) fn test_home(home: Option<PathBuf>) {
    if let Ok(mut named) = TEST_HOME.lock() {
        *named = home;
    }
}

/// A plugin's `fs` permission, with every root resolved.
#[cfg(feature = "node")]
fn access(fs: &Fs, session: &Path, home: Option<&Path>) -> Access {
    match fs {
        Fs::None => Access::None,
        Fs::ReadAll => Access::ReadAll,
        Fs::WriteAll => Access::WriteAll,
        Fs::Lists { read, write } => Access::Lists {
            read: roots(read, session, home),
            write: roots(write, session, home),
        },
    }
}

/// The resolved roots of one `fs` list.
#[cfg(feature = "node")]
fn roots(list: &[Root], session: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    list.iter()
        .filter_map(|root| match root {
            Root::Session(rest) => Some(settled(&session.join(rest))),
            Root::Home(rest) => home.map(|home| settled(&home.join(rest))),
            Root::Absolute(path) => Some(settled(path)),
        })
        .collect()
}

/// `path` as a root or the session folder: resolved, or its folded form when the
/// links loop, which a check of that path then refuses.
#[cfg(feature = "node")]
fn settled(path: &Path) -> PathBuf {
    resolved(path).unwrap_or_else(|| folded(path))
}

/// `path` as the checks see it: lexically without `.` or `..`, then with every
/// link resolved, so a link out of a root is caught.
///
/// A link whose target does not exist yet is the one case `canonicalize` cannot
/// answer, and it is the case that matters for a write: the call makes the
/// target, so the check must name where the link points to and not the link.
/// `None` when the links loop.
#[cfg(feature = "node")]
fn resolved(path: &Path) -> Option<PathBuf> {
    let lexical = folded(path);
    let mut tail: Vec<OsString> = Vec::new();
    let mut head = lexical.clone();
    let mut hops = 0;
    loop {
        if let Some(canon) = canonical(&head) {
            return Some(with_tail(canon, &tail));
        }
        if let Some(target) = link(&head) {
            hops += 1;
            if hops > HOPS {
                return None;
            }
            // A relative target is relative to the folder the link is in.
            let base = match target.is_absolute() {
                true => target,
                false => head.parent()?.join(target),
            };
            head = with_tail(folded(&base), &tail);
            tail.clear();
            continue;
        }
        match (head.parent(), head.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                head = parent.to_path_buf();
            }
            _ => return Some(lexical),
        }
    }
}

/// `path` with `tail`, the names after it from the last to the first, joined on.
#[cfg(feature = "node")]
fn with_tail(mut path: PathBuf, tail: &[OsString]) -> PathBuf {
    for part in tail.iter().rev() {
        path.push(part);
    }
    path
}

/// `path` without `.` or `..`: a `..` takes back the folder before it and stops
/// at the root, as the kernel reads it, so the result reads the same to a check
/// and to the file system.
#[cfg(feature = "node")]
fn folded(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with every link resolved, when it exists at all.
#[cfg(feature = "node")]
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the plugin thread, off the async runtime"
)]
fn canonical(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// The target of `path`, when `path` is a link, whether its target exists or not.
#[cfg(feature = "node")]
fn link(path: &Path) -> Option<PathBuf> {
    std::fs::read_link(path).ok()
}

#[cfg(feature = "node")]
/// What the Node layer keeps for the context of this thread: one check state per
/// plugin index and the `process` object LLRT built. Rust data only, so nothing
/// here is reachable from JS.
struct State {
    /// One entry per plugin, at the index its wrapper modules carry; a plugin
    /// that failed to load leaves `None`.
    plugins: Vec<Option<Policy>>,
    /// The folder a relative path of a plugin resolves against.
    session: Option<PathBuf>,
    /// The state folder the sessions use: a plugin's cookies are saved under
    /// its `plugins` folder.
    state: Option<PathBuf>,
    /// The sandbox helper every child process starts through, when this run
    /// has a sandbox at all.
    helper: Option<PathBuf>,
    /// The `process` object, out of the globals so no plugin code sees one.
    process: Option<Persistent<Object<'static>>>,
    /// LLRT's own `FileHandle.prototype.chown`, which [`seal_handles`] takes
    /// off the prototype; only a facade's `chown` calls it, after its check.
    chown: Option<Persistent<Function<'static>>>,
}

#[cfg(feature = "node")]
impl State {
    /// The empty state of a context that has not loaded a plugin.
    const fn new() -> Self {
        Self {
            plugins: Vec::new(),
            session: None,
            state: None,
            helper: None,
            process: None,
            chown: None,
        }
    }

    /// The checks of the plugin at `index`.
    fn get(&self, index: usize) -> Option<&Policy> {
        self.plugins.get(index)?.as_ref()
    }
}

#[cfg(feature = "node")]
thread_local! {
    /// The state of the one context of this thread, which is the plugin thread.
    static NODE: RefCell<State> = const { RefCell::new(State::new()) };
}

#[cfg(feature = "node")]
/// What one plugin's checks allow.
struct Policy {
    /// The plugin's name, for the messages.
    name: String,
    /// The plugin's own folder: always readable.
    own: PathBuf,
    /// The folder a relative path resolves against.
    session: PathBuf,
    /// The home folder a path starting with `~` resolves against.
    home: Option<PathBuf>,
    /// The file system permission, with every root resolved.
    fs: Access,
    /// The environment permission.
    env: Vars,
    /// The hosts the plugin may fetch from.
    net: Vec<Host>,
    /// The programs the plugin may run, every entry resolved once.
    run: Vec<child::Run>,
}

#[cfg(feature = "node")]
impl Policy {
    /// `text` as a path on disk, folded, or `None` when it names no path this
    /// plugin may even resolve.
    fn expand(&self, text: &str) -> Option<PathBuf> {
        if let Some(rest) = text.strip_prefix('~') {
            // `~`, `~/x` and nothing else: another user's home is not ours.
            if !rest.is_empty() && !rest.starts_with('/') {
                return None;
            }
            return Some(folded(
                &self.home.as_ref()?.join(rest.trim_start_matches('/')),
            ));
        }
        let path = Path::new(text);
        let full = match path.is_absolute() {
            true => path.to_path_buf(),
            false => self.session.join(path),
        };
        Some(folded(&full))
    }

    /// The path `text` names, resolved the way the call reads it, or `None` when
    /// it names no path this plugin may even resolve.
    fn path(&self, text: &str, kept: Kept) -> Option<PathBuf> {
        let folded = self.expand(text)?;
        match kept {
            Kept::Followed => resolved(&folded),
            // The folder resolves, the last name stays as given, so a call that
            // works on the entry itself sees the entry and not what it links to.
            Kept::Entry => match (folded.parent(), folded.file_name()) {
                (Some(parent), Some(name)) => Some(resolved(parent)?.join(name)),
                // The root has no last name to keep, so nothing can escape it.
                _ => resolved(&folded),
            },
        }
    }

    /// Whether the plugin may use the path this way.
    fn allows(&self, wanted: Wanted, path: &Path) -> bool {
        match (&self.fs, wanted) {
            (Access::None, Wanted::Read) => path.starts_with(&self.own),
            (Access::None, Wanted::Write) => false,
            (Access::ReadAll, Wanted::Read) => true,
            (Access::ReadAll, Wanted::Write) => false,
            (Access::WriteAll, _) => true,
            (Access::Lists { write, .. }, Wanted::Write) => inside(write, path),
            (Access::Lists { read, write }, Wanted::Read) => {
                path.starts_with(&self.own) || inside(read, path) || inside(write, path)
            }
        }
    }

    /// Whether the plugin may read the environment variable `name`.
    fn reads(&self, name: &str) -> bool {
        match &self.env {
            Vars::None => false,
            Vars::All => true,
            Vars::Names(names) => names.iter().any(|allowed| allowed == name),
        }
    }

    /// The names of this plugin's environment permission that are set.
    fn names(&self) -> Vec<String> {
        match &self.env {
            Vars::None => Vec::new(),
            Vars::All => std::env::vars_os()
                .filter_map(|(name, _)| name.into_string().ok())
                .collect(),
            Vars::Names(names) => names
                .iter()
                .filter(|name| std::env::var_os(name.as_str()).is_some())
                .cloned()
                .collect(),
        }
    }
}

#[cfg(feature = "node")]
/// A plugin's `fs` permission, with every root resolved.
enum Access {
    /// Nothing but the plugin's own folder.
    None,
    /// Read anywhere.
    ReadAll,
    /// Read and write anywhere.
    WriteAll,
    /// Just these roots.
    Lists {
        /// Every root it may read.
        read: Vec<PathBuf>,
        /// Every root it may write.
        write: Vec<PathBuf>,
    },
}

#[cfg(feature = "node")]
/// A plugin's `env` permission.
enum Vars {
    /// None of them.
    None,
    /// Every one.
    All,
    /// Just these names.
    Names(Vec<String>),
}

#[cfg(feature = "node")]
/// What one check asks of the file system.
#[derive(Clone, Copy)]
enum Wanted {
    /// Read a file or a folder.
    Read,
    /// Write a file or a folder.
    Write,
}

#[cfg(feature = "node")]
impl Wanted {
    /// The access one wrapper names.
    fn of(access: &str) -> Self {
        match access {
            "write" => Wanted::Write,
            _ => Wanted::Read,
        }
    }

    /// The access as the message spells it.
    fn as_str(self) -> &'static str {
        match self {
            Wanted::Read => "read",
            Wanted::Write => "write",
        }
    }
}

/// The names every object answers from `Object.prototype`: the `env` proxy
/// answers them `undefined`, so `JSON.stringify(process.env)` and
/// `String(process.env)` work under any permission.
#[cfg(feature = "node")]
const PROTOTYPE: [&str; 9] = [
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toJSON",
    "toLocaleString",
    "toString",
    "valueOf",
    "__proto__",
];

/// The number of links one path may follow before the check refuses it: a loop
/// of links is no path, and the kernel refuses it too.
#[cfg(feature = "node")]
const HOPS: u32 = 40;

/// The random part `mkdtemp` adds to its prefix, six characters long.
#[cfg(feature = "node")]
const SUFFIX: &str = "XXXXXX";

/// The bits of an `open` flag that write: `O_WRONLY`, `O_RDWR`, `O_CREAT`,
/// `O_TRUNC` and `O_APPEND`.
#[cfg(feature = "node")]
const WRITE_BITS: i32 = 0o1 | 0o2 | 0o100 | 0o1000 | 0o2000;

/// The flags of one `open`, which are a string or a number: every flag but the
/// read-only ones needs write, and so does a flag this does not know.
///
/// `undefined` and `null` mean `r`, and `r+`, `rs+` and their like read and
/// write both, so they need write.
#[cfg(feature = "node")]
fn writes(flags: Option<Value<'_>>) -> bool {
    let Some(flags) = flags else {
        return false;
    };
    if flags.is_undefined() || flags.is_null() {
        return false;
    }
    if let Some(text) = flags.as_string().and_then(|text| text.to_string().ok()) {
        return !matches!(text.as_str(), "r" | "rs" | "sr");
    }
    if let Some(bits) = flags.as_int() {
        return bits & WRITE_BITS != 0;
    }
    true
}

/// What one check does with the last name of a path.
#[cfg(feature = "node")]
#[derive(Clone, Copy)]
enum Kept {
    /// Resolve every link, the last one too: the call follows it.
    Followed,
    /// Resolve the folder, keep the last name: the call works on the entry.
    Entry,
}

#[cfg(feature = "node")]
/// Whether `path` is inside one of `roots`.
fn inside(roots: &[PathBuf], path: &Path) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(all(test, feature = "node"))]
mod child_tests;

#[cfg(all(test, feature = "node"))]
mod shim_tests;

#[cfg(all(test, feature = "node"))]
mod tests;
