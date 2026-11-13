// The `path` module of one plugin: LLRT's own, with the `posix` alias Node has.
//
// reagent runs on unix only, so `posix` is the same set of functions as the
// module itself, and `posix.posix` points back at it.
import * as raw from "reagent:raw/path";

export * from "reagent:raw/path";

export const sep = "/";
export const delimiter = ":";

const posix = {
  basename: raw.basename,
  delimiter,
  dirname: raw.dirname,
  extname: raw.extname,
  format: raw.format,
  isAbsolute: raw.isAbsolute,
  join: raw.join,
  normalize: raw.normalize,
  parse: raw.parse,
  relative: raw.relative,
  resolve: raw.resolve,
  sep,
};
posix.posix = posix;

export { posix };

const path = {
  basename: raw.basename,
  delimiter,
  dirname: raw.dirname,
  extname: raw.extname,
  format: raw.format,
  isAbsolute: raw.isAbsolute,
  join: raw.join,
  normalize: raw.normalize,
  parse: raw.parse,
  posix,
  relative: raw.relative,
  resolve: raw.resolve,
  sep,
};

export default path;
