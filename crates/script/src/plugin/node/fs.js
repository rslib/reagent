// The `fs` module of one plugin: every synchronous call checks the plugin's
// file permission before the raw function touches the file, and `promises` is
// the checked `fs/promises` of the same plugin.
//
// Every decision is the guard's: this file only names the guard function a call
// takes and hands the path it answers to the raw function, so a plugin that
// patches a builtin of its own cannot move a check.
import * as raw from "reagent:raw/fs";
import promises from "reagent:node/fs/promises";
import { check, checkLast, checkTemp, index } from "reagent:raw/guard";

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

export const accessSync = (path, ...rest) =>
  raw.accessSync(reading(path), ...rest);
export const chmodSync = (path, mode, ...rest) =>
  raw.chmodSync(writing(path), mode, ...rest);
export const lstatSync = (path, ...rest) =>
  raw.lstatSync(readingEntry(path), ...rest);
export const mkdirSync = (path, ...rest) =>
  raw.mkdirSync(writing(path), ...rest);
export const mkdtempSync = (prefix, ...rest) =>
  raw.mkdtempSync(checkTemp(index, pathOf(prefix)), ...rest);
export const readFileSync = (path, ...rest) =>
  raw.readFileSync(reading(path), ...rest);
export const readdirSync = (path, ...rest) =>
  raw.readdirSync(reading(path), ...rest);
export const renameSync = (oldPath, newPath, ...rest) =>
  raw.renameSync(writingEntry(oldPath), writingEntry(newPath), ...rest);
export const rmdirSync = (path, ...rest) =>
  raw.rmdirSync(writingEntry(path), ...rest);
export const rmSync = (path, ...rest) =>
  raw.rmSync(writingEntry(path), ...rest);
export const statSync = (path, ...rest) =>
  raw.statSync(reading(path), ...rest);
export const symlinkSync = (target, path, ...rest) =>
  raw.symlinkSync(writing(target), writingEntry(path), ...rest);
export const writeFileSync = (path, data, ...rest) =>
  raw.writeFileSync(writing(path), data, ...rest);
export const constants = raw.constants;
export { promises };

export default {
  promises,
  accessSync,
  chmodSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmdirSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
  constants,
};
