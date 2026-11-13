// The `constants` module of one plugin: the merge Node exports, which is
// `os.constants` and `fs.constants` flattened into one object.
//
// The raw `fs` of LLRT has only the four modes of `access`, so the rest are the
// numbers of the platform this build runs on.
import * as raw from "reagent:raw/fs";
import os from "reagent:node/os";

// The flags of `open`, which differ between the platforms reagent runs on.
const flags =
  os.platform() === "darwin"
    ? {
        O_RDONLY: 0,
        O_WRONLY: 1,
        O_RDWR: 2,
        O_CREAT: 512,
        O_EXCL: 2048,
        O_TRUNC: 1024,
        O_APPEND: 8,
        O_NONBLOCK: 4,
      }
    : {
        O_RDONLY: 0,
        O_WRONLY: 1,
        O_RDWR: 2,
        O_CREAT: 64,
        O_EXCL: 128,
        O_TRUNC: 512,
        O_APPEND: 1024,
        O_NONBLOCK: 2048,
      };

// The modes of `stat`, and the modes of `access`, which are the same on both.
const modes = {
  S_IFMT: 61440,
  S_IFREG: 32768,
  S_IFDIR: 16384,
  S_IFLNK: 40960,
  F_OK: 0,
  R_OK: 4,
  W_OK: 2,
  X_OK: 1,
};

// What `fs.copyFile` takes, and what only it takes.
const copies = { COPYFILE_EXCL: 1 };

const constants = Object.freeze({
  ...os.constants,
  ...flags,
  ...modes,
  ...copies,
  ...raw.constants,
});

const {
  COPYFILE_EXCL,
  F_OK,
  O_APPEND,
  O_CREAT,
  O_EXCL,
  O_NONBLOCK,
  O_RDONLY,
  O_RDWR,
  O_TRUNC,
  O_WRONLY,
  R_OK,
  S_IFDIR,
  S_IFLNK,
  S_IFMT,
  S_IFREG,
  W_OK,
  X_OK,
  errno,
  priority,
  signals,
} = constants;

export {
  COPYFILE_EXCL,
  F_OK,
  O_APPEND,
  O_CREAT,
  O_EXCL,
  O_NONBLOCK,
  O_RDONLY,
  O_RDWR,
  O_TRUNC,
  O_WRONLY,
  R_OK,
  S_IFDIR,
  S_IFLNK,
  S_IFMT,
  S_IFREG,
  W_OK,
  X_OK,
  errno,
  priority,
  signals,
};

export default constants;
