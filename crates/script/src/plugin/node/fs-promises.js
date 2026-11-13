// The `fs/promises` module of one plugin: every call checks the plugin's file
// permission before the raw function touches the file, and `open` answers a
// facade of the raw handle instead of the handle itself.
//
// Every decision is the guard's: this file only names the guard function a call
// takes and hands the path it answers to the raw function, so a plugin that
// patches a builtin of its own cannot move a check.
import * as raw from "reagent:raw/fs/promises";
import {
  check,
  checkLast,
  checkOpen,
  checkTemp,
  chownHandle,
  index,
  opensWrite,
} from "reagent:raw/guard";

// The path a call names: a `URL` or a `Buffer` becomes its string first. The
// string is all the guard sees, so this only decides how a value reads.
function pathOf(value) {
  if (typeof value === "string") return value;
  if (value instanceof URL) {
    return value.protocol === "file:"
      ? decodeURIComponent(value.pathname)
      : String(value);
  }
  if (typeof Buffer !== "undefined" && Buffer.isBuffer(value)) {
    return value.toString();
  }
  return String(value);
}

// The path the raw function must use: the guard answers it, absolute and with
// every link it has resolved, so nothing is read from a path it did not see.
const reading = (value) => check(index, "read", pathOf(value));

// The same for a call that works on the entry itself: only the folder resolves,
// so `lstat`, `rm`, `rmdir` and `rename` see a link and not its target.
const readingEntry = (value) => checkLast(index, "read", pathOf(value));
const writingEntry = (value) => checkLast(index, "write", pathOf(value));

const writing = (value) => check(index, "write", pathOf(value));

// What `open` answers with: the read methods always, the write methods only when
// the file was opened for writing, and `chmod` and `chown` behind a write check
// of the opened path. The raw handle lives in this closure, so no property of
// the facade names it and the handle's own `chmod` stays out of reach.
function opened(handle, path, writable) {
  const out = {
    read: (...rest) => handle.read(...rest),
    readFile: (...rest) => handle.readFile(...rest),
    stat: (...rest) => handle.stat(...rest),
    close: (...rest) => handle.close(...rest),
    get fd() {
      return handle.fd;
    },
    chmod: (mode, ...rest) => raw.chmod(writing(path), mode, ...rest),
    chown: (uid, gid) => {
      writing(path);
      return chownHandle(handle, uid, gid);
    },
  };
  if (writable) {
    out.write = (...rest) => handle.write(...rest);
    out.writeFile = (...rest) => handle.writeFile(...rest);
    out.truncate = (...rest) => handle.truncate(...rest);
    out.sync = (...rest) => handle.sync(...rest);
    out.datasync = (...rest) => handle.datasync(...rest);
  }
  return out;
}

// Whether the flags of an `open` name a write is the guard's answer, so the
// methods a plugin gets and the permission it needs never disagree.
export const open = async (path, flags, ...rest) => {
  const checked = checkOpen(index, pathOf(path), flags);
  const handle = await (flags === undefined
    ? raw.open(checked)
    : raw.open(checked, flags, ...rest));
  return opened(handle, checked, opensWrite(flags));
};

export const access = (path, ...rest) => raw.access(reading(path), ...rest);
export const chmod = (path, mode, ...rest) =>
  raw.chmod(writing(path), mode, ...rest);
export const lstat = (path, ...rest) =>
  raw.lstat(readingEntry(path), ...rest);
export const mkdir = (path, ...rest) => raw.mkdir(writing(path), ...rest);
export const mkdtemp = (prefix, ...rest) =>
  raw.mkdtemp(checkTemp(index, pathOf(prefix)), ...rest);
export const readFile = (path, ...rest) =>
  raw.readFile(reading(path), ...rest);
export const readdir = (path, ...rest) =>
  raw.readdir(reading(path), ...rest);
export const rename = (oldPath, newPath, ...rest) =>
  raw.rename(writingEntry(oldPath), writingEntry(newPath), ...rest);
export const rm = (path, ...rest) => raw.rm(writingEntry(path), ...rest);
export const rmdir = (path, ...rest) => raw.rmdir(writingEntry(path), ...rest);
export const stat = (path, ...rest) => raw.stat(reading(path), ...rest);
export const symlink = (target, path, ...rest) =>
  raw.symlink(writing(target), writingEntry(path), ...rest);
export const writeFile = (path, data, ...rest) =>
  raw.writeFile(writing(path), data, ...rest);
export const constants = raw.constants;

export default {
  access,
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  readdir,
  rename,
  rm,
  rmdir,
  stat,
  symlink,
  writeFile,
  constants,
};
