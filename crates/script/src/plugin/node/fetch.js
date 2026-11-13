// The web `fetch` of the plugins: the JS half of the net host.
//
// Every decision that matters — which host a plugin may reach, which redirect
// hop is allowed, whether a body is too large, whether a response matches its
// integrity metadata, which cookies a request carries — is made in Rust, in the
// module this one imports. A plugin has no name for that module, so nothing it
// patches here can change one of those decisions.
import * as net from "reagent:raw/net-host";

// The constructors and helpers taken once: a plugin that swaps a global later
// changes nothing here.
const FAILURE = TypeError;
const ABORT = DOMException;
const FROM = Array.from;
const ENCODER = new TextEncoder();
const DECODER = new TextDecoder();
const RANDOM = Math.random;

// A request body must be a string, bytes, a form, a blob or a stream; a JSON
// body is a string a plugin encodes itself.
const ABORTED = "the fetch was aborted";
const OPEN = "too many open responses: read one to its end or cancel it";
const TYPES =
  "a body must be a string, an ArrayBuffer, a typed array, URLSearchParams, a FormData, a Blob or a ReadableStream";

// The values the request options accept, and the standard referrer policies.
const CACHES = "default, no-store, reload, no-cache, force-cache, only-if-cached";
const MODES = "cors, no-cors, same-origin";
const CREDENTIALS = "omit, same-origin, include";

// The state of a `Headers` and a `FormData`. The state of a `Request` and a
// `Response` is a private field of its class, so no lookup a plugin can patch
// reaches it.
const STATE = new WeakMap();

// The characters a header name may carry, the ones a value may not, and the
// HTTP whitespace a value is trimmed of.
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const BREAK = /[\u0000\r\n]/;
const EDGE = /^[ \t]+|[ \t]+$/g;

function failure(message) {
  return new FAILURE(message);
}

function aborted() {
  return new ABORT(ABORTED, "AbortError");
}

/// Throws unless `name` is a header name and `value` a header value, naming the
/// header the standard refuses.
function valid(name, as, value) {
  if (!TOKEN.test(name)) {
    throw failure(`the header name ${as} is not a valid header name`);
  }
  if (BREAK.test(value)) {
    throw failure(`the header value of ${as} is not a valid header value`);
  }
}

/// The signal of a fetch, read and checked before any slot is reserved: a
/// signal that is no `AbortSignal`, or that throws while its state is read,
/// rejects the fetch and leaks nothing.
function watch(signal) {
  if (signal === null || signal === undefined) return null;
  let aborted;
  let listening;
  try {
    aborted = signal.aborted;
    listening = signal.addEventListener;
  } catch {
    throw failure("signal must be an AbortSignal");
  }
  if (typeof aborted !== "boolean" || typeof listening !== "function") {
    throw failure("signal must be an AbortSignal");
  }
  return signal;
}

/// The name a header is stored under: header names are case-insensitive.
function key(name) {
  return String(name).toLowerCase();
}

function isBlob(value) {
  return typeof Blob === "function" && value instanceof Blob;
}

function isFile(value) {
  return typeof File === "function" && value instanceof File;
}

function isStream(value) {
  return typeof ReadableStream === "function" && value instanceof ReadableStream;
}

/// A fresh multipart boundary: it names one body, so it need not be secret.
function boundary() {
  const alphabet =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let text = "----reagent";
  for (let i = 0; i < 24; i += 1) text += alphabet[(RANDOM() * 64) | 0];
  return text;
}

/// The bytes of `chunks`, in order.
function join(chunks) {
  let total = 0;
  for (let i = 0; i < chunks.length; i += 1) total += chunks[i].byteLength;
  const out = new Uint8Array(total);
  let at = 0;
  for (let i = 0; i < chunks.length; i += 1) {
    out.set(chunks[i], at);
    at += chunks[i].byteLength;
  }
  return out;
}

/// `text` as a part of a multipart header, with the characters a quoted name
/// cannot carry escaped the way the spec does.
function quoted(text) {
  let out = "";
  for (let i = 0; i < text.length; i += 1) {
    const char = text[i];
    if (char === '"') out += "%22";
    else if (char === "\r") out += "%0D";
    else if (char === "\n") out += "%0A";
    else out += char;
  }
  return out;
}

/// One entry of a `FormData`: a string or a `File` under a name.
function entry(name, value, filename) {
  const held = String(name);
  if (isBlob(value)) {
    if (filename === undefined && isFile(value)) {
      return { name: held, file: true, value };
    }
    const named = filename === undefined ? "blob" : String(filename);
    const type = value.type === undefined ? "" : value.type;
    return { name: held, file: true, value: new File([value], named, { type }) };
  }
  return { name: held, file: false, value: String(value) };
}

/// The multipart body of one `FormData`'s entries, one part per entry.
async function multipart(entries, mark) {
  const chunks = [];
  for (let i = 0; i < entries.length; i += 1) {
    const held = entries[i];
    let head = `--${mark}\r\ncontent-disposition: form-data; name="${quoted(held.name)}"`;
    let type = null;
    let body;
    if (held.file) {
      head += `; filename="${quoted(held.value.name)}"`;
      type =
        held.value.type === "" ? "application/octet-stream" : held.value.type;
      body = new Uint8Array(await held.value.arrayBuffer());
    } else {
      body = ENCODER.encode(held.value);
    }
    if (type !== null) head += `\r\ncontent-type: ${type}`;
    head += "\r\n\r\n";
    chunks.push(ENCODER.encode(head), body, ENCODER.encode("\r\n"));
  }
  chunks.push(ENCODER.encode(`--${mark}--\r\n`));
  return join(chunks);
}

/// The body a `Request` or a `Response` carries, kept as the thing the plugin
/// gave, and the content type it sets when the headers name none.
function sourceOf(body, headers) {
  if (body === undefined || body === null) return null;
  if (typeof body === "string") return { kind: "bytes", bytes: ENCODER.encode(body) };
  if (body instanceof ArrayBuffer) return { kind: "bytes", bytes: new Uint8Array(body) };
  if (ArrayBuffer.isView(body)) {
    return {
      kind: "bytes",
      bytes: new Uint8Array(body.buffer, body.byteOffset, body.byteLength),
    };
  }
  if (typeof URLSearchParams === "function" && body instanceof URLSearchParams) {
    if (!headers.has("content-type")) {
      headers.set("content-type", "application/x-www-form-urlencoded;charset=UTF-8");
    }
    return { kind: "bytes", bytes: ENCODER.encode(body.toString()) };
  }
  if (body instanceof FormData) {
    // The boundary of the body is the boundary of the header: an explicit one
    // would not name the parts the body carries.
    const mark = boundary();
    headers.set("content-type", `multipart/form-data; boundary=${mark}`);
    return { kind: "form", entries: ENTRIES(body), boundary: mark };
  }
  if (isBlob(body)) {
    if (body.type !== "" && !headers.has("content-type")) {
      headers.set("content-type", body.type);
    }
    return { kind: "blob", blob: body };
  }
  if (isStream(body)) return { kind: "stream", stream: body };
  throw failure(TYPES);
}

/// The whole body of a source, gathered in memory.
async function bytesOfSource(source) {
  if (source === null || source === undefined) return null;
  if (source.kind === "bytes") return source.bytes;
  if (source.kind === "blob") return new Uint8Array(await source.blob.arrayBuffer());
  if (source.kind === "form") return multipart(source.entries, source.boundary);
  const reader = source.stream.getReader();
  const chunks = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(typeof value === "string" ? ENCODER.encode(value) : value);
  }
  return join(chunks);
}

function empty() {
  return new Uint8Array(0);
}

/// Stops listening for the abort of the request this body came from.
function detach(state) {
  const signal = state.signal;
  if (signal === null || signal === undefined) return;
  const off = state.off;
  state.off = undefined;
  if (off !== undefined) signal.removeEventListener("abort", off);
}

/// One chunk of a body read from Rust, or the end of it.
async function pullFromNet(state, controller) {
  if (state.ended) {
    controller.close();
    return;
  }
  const chunk = await net.pull(state.slot);
  if (chunk === null) {
    state.ended = true;
    detach(state);
    controller.close();
    return;
  }
  if (chunk instanceof Uint8Array) {
    controller.enqueue(chunk);
    return;
  }
  state.ended = true;
  detach(state);
  if (chunk.abort === true) controller.error(aborted());
  else controller.error(failure(chunk.error));
}

/// The `ReadableStream` of a body, made at the first look at it.
function streamOf(state) {
  if (state.stream !== undefined) return state.stream;
  state.used = true;
  if (state.kind !== "net") {
    if (state.source !== null && state.source.kind === "stream") {
      state.stream = state.source.stream;
      return state.stream;
    }
    const stream = new ReadableStream({
      pull: async (controller) => {
        const bytes = await bytesOfSource(state.source);
        state.source = null;
        if (bytes !== null && bytes.byteLength > 0) controller.enqueue(bytes);
        controller.close();
      },
      cancel: () => {
        state.used = true;
        state.ended = true;
      },
    });
    state.stream = stream;
    return stream;
  }
  const stream = new ReadableStream({
    pull: (controller) => pullFromNet(state, controller),
    cancel: () => {
      state.used = true;
      state.ended = true;
      detach(state);
      net.cancel(state.slot);
    },
  });
  state.stream = stream;
  return stream;
}

/// The whole body, gathered in Rust, which refuses one over 32 MiB and one
/// that does not match its integrity metadata.
async function gather(state) {
  if (state.used) throw failure("the body of this Request or Response is used");
  state.used = true;
  if (state.kind !== "net") {
    const source = state.source;
    state.source = null;
    const bytes = await bytesOfSource(source);
    return bytes === null ? empty() : bytes;
  }
  const out = await net.gather(state.slot);
  state.ended = true;
  detach(state);
  if (out instanceof Uint8Array) return out;
  if (out.abort === true) throw aborted();
  throw failure(out.error);
}

function textOf(state) {
  return (async () => DECODER.decode(await gather(state)))();
}

function jsonOf(state) {
  return (async () => JSON.parse(DECODER.decode(await gather(state))))();
}

function arrayBufferOf(state) {
  return (async () => {
    const bytes = await gather(state);
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  })();
}

function bytesCopyOf(state) {
  return (async () => new Uint8Array(await gather(state)))();
}

/// A `Blob` of the whole body, typed as its content type.
function blobOf(state) {
  return (async () => {
    const type = contentTypeOf(state);
    const bytes = await gather(state);
    return type === "" ? new Blob([bytes]) : new Blob([bytes], { type });
  })();
}

function contentTypeOf(state) {
  const held = state.headers.get("content-type");
  return held === null ? "" : held;
}

/// Where `needle` first is in `hay`, from `from`, or -1.
function findBytes(hay, needle, from) {
  for (let at = from; at + needle.length <= hay.length; at += 1) {
    let same = true;
    for (let i = 0; i < needle.length; i += 1) {
      if (hay[at + i] !== needle[i]) {
        same = false;
        break;
      }
    }
    if (same) return at;
  }
  return -1;
}

/// The value of `key` in a `content-disposition` line, or null.
function parameter(line, name) {
  const needle = `${name}="`;
  for (let at = line.indexOf(needle); at >= 0; at = line.indexOf(needle, at + 1)) {
    const before = at === 0 ? "" : line[at - 1];
    if (before !== ";" && before !== " " && before !== ":") continue;
    const start = at + needle.length;
    const end = line.indexOf('"', start);
    return end < 0 ? line.slice(start) : line.slice(start, end);
  }
  return null;
}

function percent(text) {
  try {
    return decodeURIComponent(text.replace(/\+/g, " "));
  } catch {
    return text;
  }
}

/// The entries of a `multipart/form-data` body.
function parseMultipart(bytes, mark) {
  const form = new FormData();
  const dash = ENCODER.encode(`--${mark}`);
  const split = ENCODER.encode("\r\n\r\n");
  const next = ENCODER.encode(`\r\n--${mark}`);
  let at = findBytes(bytes, dash, 0);
  while (at >= 0) {
    at += dash.length;
    if (bytes[at] === 0x2d && bytes[at + 1] === 0x2d) break;
    if (bytes[at] === 0x0d && bytes[at + 1] === 0x0a) at += 2;
    const head = findBytes(bytes, split, at);
    if (head < 0) break;
    const text = DECODER.decode(bytes.subarray(at, head));
    let name = null;
    let filename = null;
    let type = null;
    const lines = text.split("\r\n");
    for (let i = 0; i < lines.length; i += 1) {
      const line = lines[i];
      const lower = line.toLowerCase();
      if (lower.startsWith("content-disposition:")) {
        name = parameter(line, "name");
        filename = parameter(line, "filename");
      } else if (lower.startsWith("content-type:")) {
        type = line.slice(line.indexOf(":") + 1).trim();
      }
    }
    const start = head + split.length;
    const end = findBytes(bytes, next, start);
    const body = bytes.subarray(start, end < 0 ? bytes.length : end);
    if (name !== null) {
      if (filename !== null) {
        form.append(
          name,
          type === null
            ? new File([body], filename)
            : new File([body], filename, { type }),
        );
      } else {
        form.append(name, DECODER.decode(body));
      }
    }
    if (end < 0) break;
    at = findBytes(bytes, dash, end + 2);
  }
  return form;
}

/// The entries of an `application/x-www-form-urlencoded` body.
function parseUrlencoded(text) {
  const form = new FormData();
  const pairs = text.split("&");
  for (let i = 0; i < pairs.length; i += 1) {
    if (pairs[i] === "") continue;
    const at = pairs[i].indexOf("=");
    const name = percent(at < 0 ? pairs[i] : pairs[i].slice(0, at));
    const value = at < 0 ? "" : percent(pairs[i].slice(at + 1));
    form.append(name, value);
  }
  return form;
}

/// The body as a `FormData`: a multipart body, or a urlencoded one.
function formDataOf(state) {
  return (async () => {
    const type = contentTypeOf(state);
    const lower = type.toLowerCase();
    const bytes = await gather(state);
    if (lower.startsWith("multipart/form-data")) {
      const at = lower.indexOf("boundary=");
      if (at < 0) throw failure("the multipart body of this Response names no boundary");
      let mark = type.slice(at + "boundary=".length);
      const end = mark.indexOf(";");
      if (end >= 0) mark = mark.slice(0, end);
      mark = mark.trim();
      if (mark.length > 1 && mark[0] === '"' && mark[mark.length - 1] === '"') {
        mark = mark.slice(1, -1);
      }
      return parseMultipart(bytes, mark);
    }
    if (lower.startsWith("application/x-www-form-urlencoded")) {
      return parseUrlencoded(DECODER.decode(bytes));
    }
    throw failure("the body of this Response is not a form");
  })();
}

/// The headers of a request as the pairs Rust builds its request from.
function pairsOf(headers) {
  const pairs = [];
  for (const [name, value] of headers) pairs.push([name, value]);
  return pairs;
}

class Headers {
  constructor(init) {
    STATE.set(this, new Map());
    if (init === undefined || init === null) return;
    if (init instanceof Headers) {
      for (const [name, value] of init) this.append(name, value);
      return;
    }
    if (typeof init !== "object") {
      throw failure("Headers must be built from an object, an array of pairs or a Headers");
    }
    if (typeof init[Symbol.iterator] === "function") {
      for (const pair of init) {
        if (pair === null || typeof pair !== "object") {
          throw failure("a header pair must name a header and its value");
        }
        this.append(pair[0], pair[1]);
      }
      return;
    }
    for (const name of Object.keys(init)) this.append(name, init[name]);
  }

  append(name, value) {
    const held = key(name);
    const text = String(value).replace(EDGE, "");
    valid(held, String(name), text);
    const map = STATE.get(this);
    const known = map.get(held);
    if (known === undefined) map.set(held, text);
    else map.set(held, `${known}, ${text}`);
  }

  delete(name) {
    STATE.get(this).delete(key(name));
  }

  get(name) {
    const value = STATE.get(this).get(key(name));
    return value === undefined ? null : value;
  }

  has(name) {
    return STATE.get(this).has(key(name));
  }

  set(name, value) {
    const held = key(name);
    const text = String(value).replace(EDGE, "");
    valid(held, String(name), text);
    STATE.get(this).set(held, text);
  }

  forEach(callback, thisArg) {
    for (const [name, value] of this) callback.call(thisArg, value, name, this);
  }

  entries() {
    return STATE.get(this).entries();
  }

  keys() {
    return STATE.get(this).keys();
  }

  values() {
    return STATE.get(this).values();
  }

  get size() {
    return STATE.get(this).size;
  }

  [Symbol.iterator]() {
    return STATE.get(this).entries();
  }
}

/// The form a plugin builds for a fetch body or reads out of a response.
class FormData {
  #entries;

  /// Reads the entries of one form. The module takes this once at load and
  /// removes it, so nothing in JS reads the entries of a form it did not build.
  static get entriesOf() {
    return (self) => self.#entries;
  }

  constructor() {
    this.#entries = [];
  }

  append(name, value, filename) {
    ENTRIES(this).push(entry(name, value, filename));
  }

  set(name, value, filename) {
    const entries = ENTRIES(this);
    const held = entry(name, value, filename);
    let kept = 0;
    let placed = false;
    for (let i = 0; i < entries.length; i += 1) {
      if (entries[i].name === held.name) {
        if (!placed) {
          entries[kept] = held;
          kept += 1;
          placed = true;
        }
      } else {
        entries[kept] = entries[i];
        kept += 1;
      }
    }
    if (!placed) {
      entries[kept] = held;
      kept += 1;
    }
    entries.length = kept;
  }

  get(name) {
    const entries = ENTRIES(this);
    const held = String(name);
    for (let i = 0; i < entries.length; i += 1) {
      if (entries[i].name === held) return entries[i].value;
    }
    return null;
  }

  getAll(name) {
    const entries = ENTRIES(this);
    const held = String(name);
    const out = [];
    for (let i = 0; i < entries.length; i += 1) {
      if (entries[i].name === held) out.push(entries[i].value);
    }
    return out;
  }

  has(name) {
    return this.get(name) !== null;
  }

  delete(name) {
    const entries = ENTRIES(this);
    const held = String(name);
    let kept = 0;
    for (let i = 0; i < entries.length; i += 1) {
      if (entries[i].name !== held) {
        entries[kept] = entries[i];
        kept += 1;
      }
    }
    entries.length = kept;
  }

  forEach(callback, thisArg) {
    const entries = ENTRIES(this);
    for (let i = 0; i < entries.length; i += 1) {
      callback.call(thisArg, entries[i].value, entries[i].name, this);
    }
  }

  entries() {
    const entries = ENTRIES(this);
    const pairs = [];
    for (let i = 0; i < entries.length; i += 1) {
      pairs.push([entries[i].name, entries[i].value]);
    }
    return pairs[Symbol.iterator]();
  }

  keys() {
    const entries = ENTRIES(this);
    const names = [];
    for (let i = 0; i < entries.length; i += 1) names.push(entries[i].name);
    return names[Symbol.iterator]();
  }

  values() {
    const entries = ENTRIES(this);
    const values = [];
    for (let i = 0; i < entries.length; i += 1) values.push(entries[i].value);
    return values[Symbol.iterator]();
  }

  [Symbol.iterator]() {
    return this.entries();
  }
}

class Response {
  #state;

  constructor(body, init) {
    const options = init === undefined || init === null ? {} : init;
    const headers = new Headers(options.headers);
    this.#state = {
      kind: "memory",
      source: sourceOf(body, headers),
      used: false,
      ended: false,
      status: options.status === undefined ? 200 : Number(options.status),
      statusText: options.statusText === undefined ? "" : String(options.statusText),
      headers,
      url: "",
      redirected: false,
      type: "default",
      slot: undefined,
      signal: null,
      off: undefined,
      stream: undefined,
    };
  }

  /// The response of a `fetch`: its head, and its body still in Rust under the
  /// slot it was reserved. The module takes this once at load and removes it,
  /// so no plugin builds a response for a slot it does not hold.
  static net(head) {
    const self = new Response();
    self.#state = {
      kind: "net",
      source: null,
      used: false,
      ended: false,
      status: head.status,
      statusText: head.statusText,
      headers: new Headers(head.headers),
      url: head.url,
      redirected: head.redirected,
      type: "basic",
      slot: head.slot,
      signal: head.signal,
      off: head.off,
      stream: undefined,
    };
    return self;
  }

  get status() {
    return this.#state.status;
  }

  get statusText() {
    return this.#state.statusText;
  }

  get ok() {
    const status = this.#state.status;
    return status >= 200 && status <= 299;
  }

  get headers() {
    return this.#state.headers;
  }

  get url() {
    return this.#state.url;
  }

  get redirected() {
    return this.#state.redirected;
  }

  get type() {
    return this.#state.type;
  }

  get body() {
    return streamOf(this.#state);
  }

  get bodyUsed() {
    return this.#state.used;
  }

  text() {
    return textOf(this.#state);
  }

  json() {
    return jsonOf(this.#state);
  }

  arrayBuffer() {
    return arrayBufferOf(this.#state);
  }

  bytes() {
    return bytesCopyOf(this.#state);
  }

  blob() {
    return blobOf(this.#state);
  }

  formData() {
    return formDataOf(this.#state);
  }
}

/// One of the values an option accepts, or `fallback` when it is not set.
function oneOf(value, fallback, allowed, message) {
  if (value === undefined || value === null) return fallback;
  const text = String(value);
  const listed = allowed.split(", ");
  for (let i = 0; i < listed.length; i += 1) {
    if (listed[i] === text) return text;
  }
  throw failure(`${message} must be one of ${allowed}`);
}

/// The referrer of a request: a URL, the empty string or `about:client`.
function referrerOf(value, fallback) {
  if (value === undefined || value === null) return fallback;
  const text = String(value);
  if (text === "" || text === "about:client") return text;
  try {
    return new URL(text).href;
  } catch {
    throw failure("referrer must be a URL, the empty string or about:client");
  }
}

const POLICIES =
  ", no-referrer, no-referrer-when-downgrade, same-origin, origin, strict-origin, origin-when-cross-origin, strict-origin-when-cross-origin, unsafe-url";

class Request {
  #state;

  /// Reads the state of a request. The module takes this once at load and
  /// removes it, so nothing in JS reads the state of a request it did not
  /// build.
  static get stateOf() {
    return (self) => self.#state;
  }

  constructor(input, init) {
    const options = init === undefined || init === null ? {} : init;
    const base = input instanceof Request ? input.#state : null;
    const url = base === null ? String(input) : base.url;
    const asked = options.method === undefined || options.method === null
      ? (base === null ? "GET" : base.method)
      : String(options.method);
    const method = asked.toUpperCase();
    const headers = new Headers(
      options.headers === undefined || options.headers === null
        ? (base === null ? undefined : base.headers)
        : options.headers,
    );
    const source = options.body === undefined || options.body === null
      ? (base === null ? null : base.source)
      : sourceOf(options.body, headers);
    if ((method === "GET" || method === "HEAD") && source !== null) {
      throw failure(`${method} requests carry no body`);
    }
    const duplex = options.duplex === undefined || options.duplex === null
      ? undefined
      : String(options.duplex);
    const keepalive = options.keepalive;
    if (keepalive !== undefined && typeof keepalive !== "boolean") {
      throw failure("keepalive must be a boolean");
    }
    this.#state = {
      kind: "memory",
      source,
      used: false,
      ended: false,
      url: new URL(url).href,
      method,
      headers,
      redirect: options.redirect === undefined
        ? (base === null ? "follow" : base.redirect)
        : String(options.redirect),
      signal: options.signal === undefined
        ? (base === null ? null : base.signal)
        : options.signal,
      cache: oneOf(
        options.cache,
        base === null ? "default" : base.cache,
        CACHES,
        "cache",
      ),
      mode: oneOf(options.mode, base === null ? "cors" : base.mode, MODES, "mode"),
      credentials: oneOf(
        options.credentials,
        base === null ? "same-origin" : base.credentials,
        CREDENTIALS,
        "credentials",
      ),
      referrer: referrerOf(
        options.referrer,
        base === null ? "about:client" : base.referrer,
      ),
      referrerPolicy: oneOf(
        options.referrerPolicy,
        base === null ? "" : base.referrerPolicy,
        POLICIES,
        "referrerPolicy",
      ),
      integrity: options.integrity === undefined || options.integrity === null
        ? (base === null ? "" : base.integrity)
        : String(options.integrity),
      keepalive: keepalive === undefined ? (base === null ? false : base.keepalive) : keepalive,
      duplex: duplex === undefined ? (base === null ? undefined : base.duplex) : duplex,
      status: 0,
      statusText: "",
      redirected: false,
      type: "default",
      slot: undefined,
      off: undefined,
      stream: undefined,
    };
  }

  get url() {
    return this.#state.url;
  }

  get method() {
    return this.#state.method;
  }

  get headers() {
    return this.#state.headers;
  }

  get redirect() {
    return this.#state.redirect;
  }

  get signal() {
    return this.#state.signal;
  }

  get cache() {
    return this.#state.cache;
  }

  get mode() {
    return this.#state.mode;
  }

  get credentials() {
    return this.#state.credentials;
  }

  get referrer() {
    return this.#state.referrer;
  }

  get referrerPolicy() {
    return this.#state.referrerPolicy;
  }

  get integrity() {
    return this.#state.integrity;
  }

  get keepalive() {
    return this.#state.keepalive;
  }

  get duplex() {
    return this.#state.duplex;
  }

  get body() {
    return streamOf(this.#state);
  }

  get bodyUsed() {
    return this.#state.used;
  }

  text() {
    return textOf(this.#state);
  }

  json() {
    return jsonOf(this.#state);
  }

  arrayBuffer() {
    return arrayBufferOf(this.#state);
  }

  bytes() {
    return bytesCopyOf(this.#state);
  }

  blob() {
    return blobOf(this.#state);
  }

  formData() {
    return formDataOf(this.#state);
  }
}

/// Feeds a stream body to Rust chunk by chunk, the next chunk only once the
/// request took the one before it. Answers the reason the body failed, or null.
async function feed(slot, stream) {
  const reader = stream.getReader();
  for (;;) {
    let read;
    try {
      read = await reader.read();
    } catch (error) {
      const reason = error === null || error === undefined ? "the body stream failed" : String(error.message || error);
      net.close(slot, reason);
      return reason;
    }
    if (read.done) {
      net.close(slot);
      return null;
    }
    const value = read.value;
    let bytes;
    if (typeof value === "string") bytes = ENCODER.encode(value);
    else if (value instanceof Uint8Array) bytes = value;
    else {
      const reason = "a stream body must yield a Uint8Array or a string";
      net.close(slot, reason);
      reader.cancel().catch(() => {});
      return reason;
    }
    const taken = await net.push(slot, FROM(bytes));
    if (taken !== true) return null;
  }
}

/// One `fetch` of a plugin: one request, one fresh slot and the response it
/// answers with. No part of the identity comes from an argument: which plugin is
/// asking, and which slot it may touch, are read here and checked in Rust.
async function open(input, init) {
  // Which plugin is asking, read from the module the JS stack is in: a plugin
  // cannot name another one.
  const index = net.owner();
  if (index === undefined || index === null) {
    throw failure("fetch runs inside a plugin's own module");
  }
  const request = new Request(input, init);
  const state = FIELDS(request);
  if (state.cache === "only-if-cached") {
    throw failure("only-if-cached needs a cache, and a plugin has none");
  }
  if (state.used) throw failure("the body of this request is used");
  const source = state.source;
  const streamed = source !== null && source.kind === "stream";
  if (streamed && state.duplex !== "half") {
    throw failure('a ReadableStream body needs duplex: "half"');
  }
  state.used = true;
  // The signal is read and checked before the slot is reserved, so a signal
  // that throws while it is read leaks no slot.
  const signal = watch(state.signal);
  const slot = net.reserve();
  if (slot === undefined || slot === null) throw failure(OPEN);
  let off = null;
  try {
    if (signal !== null) {
      // Read again inside the try: a signal whose getter starts throwing now
      // frees the slot on the way out.
      if (signal.aborted) throw aborted();
      off = () => net.cancel(slot);
      signal.addEventListener("abort", off);
    }
    let feeding = null;
    let body;
    if (!streamed) {
      const bytes = await bytesOfSource(source);
      // rquickjs hands a typed array to Rust only as an array of numbers, so a
      // request body is copied once on the way in.
      body = bytes === null ? undefined : FROM(bytes);
    }
    feeding = streamed ? feed(slot, source.stream) : null;
    const out = await net.send(index, slot, {
      method: state.method,
      url: state.url,
      headers: pairsOf(state.headers),
      body,
      stream: streamed,
      redirect: state.redirect,
      credentials: state.credentials,
      referrer: state.referrer,
      cache: state.cache,
      integrity: state.integrity,
    });
    if (out.ok !== true) {
      if (out.abort === true) throw aborted();
      throw failure(out.message);
    }
    if (feeding !== null) {
      // The upload goes on after the answer: a body stream that never ends
      // keeps no fetch pending once the head of the response has arrived, and a
      // failure after it takes the request down, never this answer.
      feeding.catch(() => {});
    }
    return RESPONSE_NET({
      status: out.status,
      statusText: out.statusText,
      headers: out.headers,
      url: out.url,
      redirected: out.redirected,
      slot,
      signal,
      off,
    });
  } catch (error) {
    net.cancel(slot);
    if (off !== null) signal.removeEventListener("abort", off);
    throw error;
  }
}

function fetch(input, init) {
  return open(input, init);
}

// The accessors the module takes once, then removes from the classes: a plugin
// that replaces one later changes nothing, and no plugin can call one.
const FIELDS = Request.stateOf;
const RESPONSE_NET = Response.net;
const ENTRIES = FormData.entriesOf;
delete Request.stateOf;
delete Response.net;
delete FormData.entriesOf;

export { fetch, FormData, Headers, Request, Response };
