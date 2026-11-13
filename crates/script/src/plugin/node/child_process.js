// The `child_process` module of one plugin: `spawn`, `execFile` and `exec`.
//
// Every decision that matters — which program a plugin may run, the sandbox
// helper it starts through, the folder it runs in, the environment it gets, the
// signals it may send and whose child a call touches — is made in Rust, in the
// module this one imports. A plugin has no name for that module, so nothing it
// patches here can change one of those decisions, and the plugin a call acts
// for comes from the module on the stack and never from an argument.
import { EventEmitter } from "reagent:node/events";
import * as util from "reagent:node/util";
import * as host from "reagent:raw/child-host";

// The constructors and helpers taken once: a plugin that swaps a global later
// changes nothing here.
const FAILURE = Error;
const TYPE = TypeError;
const IS_ARRAY = Array.isArray;
const BUFFER = Buffer;
const SET = setTimeout;
const CLEAR = clearTimeout;
const KEYS = Object.keys;
const CREATE = Object.create;
const APPLY = Reflect.apply;
const HAS = Object.prototype.hasOwnProperty;
const QUEUE = queueMicrotask;

// An options object of no prototype, for a call that passed none.
const NONE = CREATE(null);

// How the helper's stderr starts when the sandbox could not start
// (`crates/cli/src/sandbox.rs`).
const UNAVAILABLE = "reagent: sandbox unavailable:";

// The signal a kill sends and a timeout sends.
const SIGNAL = "SIGTERM";

// The shell `exec` and `shell: true` run, which the plugin's `run` permission
// must name like any other program.
const SHELL = "sh";

// The output one gathered call keeps before it gives up: 1 MiB, as Node does.
const MAX = 1024 * 1024;

// The stdio every child gets, which is the only stdio this build starts.
const PIPE = "pipe";

// The options this build does not take, and the ones it does.
const REFUSED = "stdio other than three pipes, detached, uid, gid, argv0 and a shell that is no boolean are not supported";

// The state of one child, one of its pipes and one gathered output. It lives
// here, keyed by the objects a plugin holds, so no lookup a plugin patches
// reaches it.
const STATE = new WeakMap();

// The bytes of one chunk as text, in the encoding a stream was set to.
function asText(chunk, encoding) {
  return BUFFER.from(chunk).toString(encoding === null ? "utf8" : encoding);
}

// The value of one chunk as a stream emits it: the bytes or their text.
function valueOf(chunk, encoding) {
  if (encoding === null) return BUFFER.from(chunk);
  return BUFFER.from(chunk).toString(encoding);
}

// The bytes of what a plugin writes to a stdin: rquickjs hands a typed array to
// Rust only as an array of numbers, so the copy is made here.
function bytesOf(chunk) {
  const bytes = [];
  if (typeof chunk === "string") {
    const encoded = BUFFER.from(chunk, "utf8");
    for (let i = 0; i < encoded.length; i += 1) bytes.push(encoded[i]);
    return bytes;
  }
  for (let i = 0; i < chunk.length; i += 1) bytes.push(chunk[i]);
  return bytes;
}

// A TypeError for an option this build refuses.
function refused(name, value) {
  return new TYPE(`child_process: ${name} (${String(value)}) is not supported; ${REFUSED}`);
}

// Whether a stdio option is the three pipes a plugin always gets.
function piped(stdio) {
  if (stdio === undefined || stdio === null) return true;
  if (stdio === PIPE) return true;
  if (!IS_ARRAY(stdio) || own(stdio, "length") !== 3) return false;
  return (
    own(stdio, 0) === PIPE && own(stdio, 1) === PIPE && own(stdio, 2) === PIPE
  );
}

// The value of one own property of `given`, and `undefined` when `given` does
// not have it itself: a plugin that sets `Object.prototype.shell` changes no
// call that did not pass a shell.
function own(given, name) {
  if (given === null || given === undefined) return undefined;
  if (APPLY(HAS, given, [name]) !== true) return undefined;
  return given[name];
}

// The length of a list, when it is its own.
function lengthOf(list) {
  const length = own(list, "length");
  return typeof length === "number" && length > 0 ? length : 0;
}

// The options of one call, checked: the ones that would change what a child is
// are a TypeError, the rest are read as they are. Every one is read only when
// the plugin passed it itself.
function optionsOf(options) {
  const given = options === undefined || options === null ? NONE : options;
  const stdio = own(given, "stdio");
  if (!piped(stdio)) throw refused("stdio", stdio);
  const detached = own(given, "detached");
  if (detached !== undefined && detached !== null && detached !== false) {
    throw refused("detached", detached);
  }
  const uid = own(given, "uid");
  if (uid !== undefined && uid !== null) throw refused("uid", uid);
  const gid = own(given, "gid");
  if (gid !== undefined && gid !== null) throw refused("gid", gid);
  const argv0 = own(given, "argv0");
  if (argv0 !== undefined && argv0 !== null) throw refused("argv0", argv0);
  const shell = own(given, "shell");
  if (shell !== undefined && shell !== null && shell !== true && shell !== false) {
    throw refused("shell", shell);
  }
  const cwd = own(given, "cwd");
  const timeout = own(given, "timeout");
  const killSignal = own(given, "killSignal");
  const maxBuffer = own(given, "maxBuffer");
  const encoding = own(given, "encoding");
  return {
    cwd: typeof cwd === "string" ? cwd : null,
    env: pairsOf(own(given, "env")),
    shell: shell === true,
    timeout: typeof timeout === "number" && timeout > 0 ? timeout : 0,
    killSignal:
      killSignal === undefined || killSignal === null ? SIGNAL : killSignal,
    maxBuffer:
      typeof maxBuffer === "number" && maxBuffer > 0 ? maxBuffer : MAX,
    encoding: encoding === undefined || encoding === null ? null : encoding,
    signal: own(given, "signal"),
  };
}

// The pairs of an `env` option: only its own enumerable string values reach the
// child.
function pairsOf(env) {
  const pairs = [];
  if (env === undefined || env === null) return pairs;
  const names = KEYS(env);
  for (let i = 0; i < names.length; i += 1) {
    const name = names[i];
    if (typeof name !== "string") continue;
    const value = own(env, name);
    if (typeof value === "string") pairs.push([name, value]);
  }
  return pairs;
}

// The line a shell runs for a program and its arguments, joined as Node joins
// them.
function lineOf(program, args) {
  let line = typeof program === "string" ? program : String(program);
  const length = lengthOf(args);
  for (let i = 0; i < length; i += 1) line = `${line} ${String(own(args, i))}`;
  return line;
}

// The arguments of one start, as strings.
function argsOf(args) {
  const out = [];
  const length = lengthOf(args);
  for (let i = 0; i < length; i += 1) out.push(String(own(args, i)));
  return out;
}

// The pipe of one child: the events a plugin waits on, and, for a stdin, the
// calls that feed it.
class EventStream extends EventEmitter {
  constructor(which, id) {
    super();
    // The writes of one stdin run in the order the plugin made them, so an
    // `end` after a `write` carries that write with it.
    STATE.set(this, { which, id, encoding: null, open: true, queue: Promise.resolve() });
  }

  setEncoding(encoding) {
    STATE.get(this).encoding = encoding === undefined || encoding === null ? "utf8" : encoding;
    return this;
  }

  write(chunk, callback) {
    const state = STATE.get(this);
    if (state.which !== 0) return false;
    const bytes = bytesOf(chunk);
    state.queue = state.queue.then(() => host.write(state.id, bytes));
    state.queue.then(
      (ok) => {
        if (typeof callback === "function") {
          callback(ok ? null : new FAILURE("the stdin of the child process is closed"));
        }
      },
      (error) => {
        if (typeof callback === "function") callback(error);
      },
    );
    return true;
  }

  end(chunk) {
    const state = STATE.get(this);
    if (state.which !== 0 || !state.open) return this;
    if (chunk !== undefined && chunk !== null) this.write(chunk);
    state.open = false;
    state.queue = state.queue.then(() => host.end(state.id));
    return this;
  }
}

// One child process: its own events, its three pipes and the kills a plugin may
// send it.
class ChildProcess extends EventEmitter {
  constructor(id, pid) {
    super();
    this.pid = pid;
    this.exitCode = null;
    this.signalCode = null;
    this.killed = false;
    this.stdin = null;
    this.stdout = null;
    this.stderr = null;
    STATE.set(this, {
      id,
      exited: false,
      out: false,
      err: false,
      closed: false,
      timedOut: false,
      sandbox: false,
      stderrText: "",
      line: "",
    });
  }

  kill(signal) {
    const state = STATE.get(this);
    if (host.kill(state.id, signal === undefined ? SIGNAL : signal)) {
      this.killed = true;
      return true;
    }
    return false;
  }

  ref() {
    return this;
  }

  unref() {
    return this;
  }
}

// Emits one event of a child, and never throws for an `error` no listener waits
// on: a pipe that fails is not a plugin's exception.
function emit(child, name, first, second) {
  if (name === "error" && child.listenerCount("error") === 0) return false;
  return child.emit(name, first, second);
}

// The one `close` event of a child: after its exit and the end of both pipes.
function maybeClose(child, state) {
  if (state.closed || !state.exited || !state.out || !state.err) return;
  state.closed = true;
  // The helper exits 125 with its own line on stderr when the sandbox could not
  // start; a program may exit 125 by itself, so only both together mean the
  // program never ran. Decided here, once all of stderr has arrived.
  if (child.exitCode === 125 && state.stderrText.startsWith(UNAVAILABLE)) {
    state.sandbox = true;
    emit(child, "error", sandboxError(state, state.program));
  }
  emit(child, "close", child.exitCode, child.signalCode);
}

// Reads one pipe to its end, emitting its data as the chunks arrive.
async function pump(child, stream, state, which) {
  const pipe = STATE.get(stream);
  for (;;) {
    let chunk;
    try {
      chunk = await host.pull(pipe.id, which);
    } catch (error) {
      chunk = `the pipe of the child process could not be read: ${error}`;
    }
    if (chunk === null || chunk === undefined) break;
    if (typeof chunk === "string") {
      emit(child, "error", new FAILURE(chunk));
      break;
    }
    // The stderr is kept, cut short, only for the reason a sandbox that could
    // not start gives.
    if (which === 2 && state.stderrText.length < 4096) {
      state.stderrText = `${state.stderrText}${asText(chunk, null)}`;
    }
    stream.emit("data", valueOf(chunk, pipe.encoding));
  }
  if (which === 1) state.out = true;
  else state.err = true;
  maybeClose(child, state);
}

// The error of a child whose sandbox could not start: the helper exits 125
// before the program runs, and its stderr line after this prefix says why.
function sandboxError(state, program) {
  const reason = state.stderrText.slice(UNAVAILABLE.length).trim();
  if (reason.length > 0) return new FAILURE(`the sandbox could not start ${program}: ${reason}`);
  return new FAILURE(`the sandbox could not start ${program}`);
}

// Waits for the exit of a child and emits it, with the close that follows the
// end of its pipes.
async function watch(child, state) {
  let end;
  try {
    end = await host.wait(state.id);
  } catch (error) {
    emit(child, "error", new FAILURE(`the child process could not be waited for: ${error}`));
    return;
  }
  if (end === undefined || end === null) return;
  child.exitCode = end.code;
  child.signalCode = end.signal;
  state.exited = true;
  emit(child, "exit", end.code, end.signal);
  maybeClose(child, state);
}

// Starts one child: the host checks the program against the plugin's `run`
// permission, the sandbox starts it, and this wires its pipes and its exit.
function start(program, args, options, forced) {
  const index = host.owner();
  if (index === undefined || index === null) {
    throw new FAILURE("child_process runs inside a plugin's own module");
  }
  const checked = optionsOf(options);
  const shell = forced === true || checked.shell;
  const file = shell ? SHELL : String(program);
  const list = shell ? ["-c", lineOf(program, args)] : argsOf(args);
  // A spec of no prototype: the host reads only what is set here, so no
  // property a plugin put on `Object.prototype` reaches it. A shell is a flag,
  // never a path: which program runs is decided in Rust.
  const spec = CREATE(null);
  spec.program = file;
  spec.args = list;
  spec.cwd = checked.cwd;
  spec.env = checked.env;
  spec.shell = shell;
  const answer = host.spawn(spec);
  if (answer === undefined || answer === null) throw new FAILURE("child_process is gone");
  if (answer.ok !== true) {
    // The host says which of its refusals is a fault in the plugin's own
    // options, and that one is a TypeError, as Node's is.
    if (answer.type === true) throw new TYPE(answer.message);
    throw new FAILURE(answer.message);
  }
  const child = new ChildProcess(answer.id, answer.pid);
  const state = STATE.get(child);
  // The command a failure names is the line a shell runs, or the program and
  // its arguments.
  state.line = shell ? lineOf(program, args) : lineOf(file, list);
  state.program = file;
  child.stdin = new EventStream(0, answer.id);
  child.stdout = new EventStream(1, answer.id);
  child.stderr = new EventStream(2, answer.id);
  pump(child, child.stdout, state, 1).catch(() => {});
  pump(child, child.stderr, state, 2).catch(() => {});
  watch(child, state).catch(() => {});
  if (checked.timeout > 0) {
    // The timer ends the whole group, so a command that started children of its
    // own leaves none behind.
    state.timer = SET(() => {
      state.timedOut = true;
      child.kill(checked.killSignal);
    }, checked.timeout);
  }
  if (checked.signal !== undefined && checked.signal !== null) {
    const abort = () => child.kill(SIGNAL);
    state.abort = abort;
    state.abortSignal = checked.signal;
    if (checked.signal.aborted) abort();
    else checked.signal.addEventListener("abort", abort);
  }
  // After the call returns, so a plugin's own listener waits for it.
  QUEUE(() => emit(child, "spawn"));
  return child;
}

// The text of a gathered output, or its bytes when the encoding is `buffer`.
function textOf(list, encoding) {
  const bytes = BUFFER.concat(list);
  if (encoding === "buffer") return bytes;
  if (encoding === null) return bytes.toString("utf8");
  return bytes.toString(encoding);
}

// Collects the whole output of one child, which is what `execFile` and `exec`
// answer with.
function collect(file, args, options, shell, callback) {
  const promise = new Promise((resolve, reject) => {
    let child;
    let checked;
    try {
      checked = optionsOf(options);
      child = start(file, args, options, shell);
    } catch (error) {
      reject(error);
      return;
    }
    const state = STATE.get(child);
    const out = { stdout: [], stderr: [], out: 0, err: 0 };
    let settled = false;
    const end = () => {
      if (state.timer !== undefined && state.timer !== null) CLEAR(state.timer);
      if (state.abortSignal !== undefined) {
        state.abortSignal.removeEventListener("abort", state.abort);
      }
    };
    const finish = (error) => {
      if (settled) return;
      settled = true;
      end();
      const stdout = textOf(out.stdout, checked.encoding);
      const stderr = textOf(out.stderr, checked.encoding);
      if (error !== null) {
        error.stdout = stdout;
        error.stderr = stderr;
        error.cmd = state.line;
        reject(error);
        return;
      }
      resolve({ stdout, stderr });
    };
    const over = () => {
      const error = new FAILURE(
        `${state.line} printed more than ${checked.maxBuffer} bytes`,
      );
      error.code = "ERR_CHILD_PROCESS_STDIO_MAXBUFFER";
      error.killed = true;
      error.signal = null;
      child.kill("SIGKILL");
      finish(error);
    };
    child.stdout.on("data", (chunk) => {
      out.out += chunk.length;
      if (out.out > checked.maxBuffer) over();
      else out.stdout.push(chunk);
    });
    child.stderr.on("data", (chunk) => {
      out.err += chunk.length;
      if (out.err > checked.maxBuffer) over();
      else out.stderr.push(chunk);
    });
    child.on("error", (error) => {
      if (state.sandbox === true) finish(error);
    });
    child.on("close", (code, signal) => {
      if (state.sandbox === true) return;
      if (code !== 0 || signal !== null) {
        const error = new FAILURE(
          signal !== null
            ? `the command was killed by ${signal}`
            : `Command failed: ${state.line}`,
        );
        error.code = code;
        error.signal = signal;
        error.killed = child.killed;
        finish(error);
        return;
      }
      finish(null);
    });
    if (checked.signal !== undefined && checked.signal !== null) {
      const abort = () => {
        if (settled) return;
        settled = true;
        end();
        const error = new FAILURE("The operation was aborted");
        error.name = "AbortError";
        error.code = "ABORT_ERR";
        error.killed = true;
        error.signal = SIGNAL;
        error.stdout = textOf(out.stdout, checked.encoding);
        error.stderr = textOf(out.stderr, checked.encoding);
        reject(error);
      };
      if (checked.signal.aborted) abort();
      else checked.signal.addEventListener("abort", abort);
    }
  });
  if (typeof callback === "function") {
    promise.then(
      (answer) => callback(null, answer.stdout, answer.stderr),
      (error) => callback(error, error.stdout, error.stderr),
    );
    return undefined;
  }
  return promise;
}

// `spawn(command, args?, options?)`: one child process, started at once.
export function spawn(command, args, options) {
  let list = args;
  let given = options;
  if (args !== undefined && args !== null && !IS_ARRAY(args)) {
    given = args;
    list = [];
  }
  return start(command, list, given, false);
}

// `execFile(file, args?, options?, callback?)`: the whole output of one
// program, gathered up to `maxBuffer`.
export function execFile(file, args, options, callback) {
  let list = args;
  let given = options;
  let done = callback;
  if (typeof args === "function") {
    done = args;
    list = [];
    given = {};
  } else if (typeof options === "function") {
    done = options;
    given = {};
  }
  return collect(file, list, given, false, done);
}

// `exec(command, options?, callback?)`: one line a shell runs.
export function exec(command, options, callback) {
  let given = options;
  let done = callback;
  if (typeof options === "function") {
    done = options;
    given = {};
  }
  return collect(String(command), [], given, true, done);
}

// The promise `util.promisify` answers with, when this build has one: the same
// gathered output a callback call gets.
const CUSTOM = typeof util.promisify === "function" ? util.promisify.custom : undefined;
if (CUSTOM !== undefined && CUSTOM !== null) {
  Object.defineProperty(execFile, CUSTOM, {
    value: (file, args, options) =>
      new Promise((resolve, reject) => {
        execFile(file, args, options, (error, stdout, stderr) =>
          error === null || error === undefined ? resolve({ stdout, stderr }) : reject(error),
        );
      }),
  });
  Object.defineProperty(exec, CUSTOM, {
    value: (command, options) =>
      new Promise((resolve, reject) => {
        exec(command, options, (error, stdout, stderr) =>
          error === null || error === undefined ? resolve({ stdout, stderr }) : reject(error),
        );
      }),
  });
}

export default { exec, execFile, spawn };
