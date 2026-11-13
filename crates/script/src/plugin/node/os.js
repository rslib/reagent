// The `os` module of one plugin: LLRT's own, with the `constants` Node has.
//
// The numbers differ between the platforms reagent runs on, so they are picked
// by `os.platform()` once, when this module loads.
import * as raw from "reagent:raw/os";

export * from "reagent:raw/os";

const darwin = raw.default.platform() === "darwin";

// The signals a plugin may name, with the number of this platform.
const signals = Object.freeze(
  darwin
    ? {
        SIGHUP: 1,
        SIGINT: 2,
        SIGQUIT: 3,
        SIGILL: 4,
        SIGTRAP: 5,
        SIGABRT: 6,
        SIGFPE: 8,
        SIGKILL: 9,
        SIGBUS: 10,
        SIGSEGV: 11,
        SIGPIPE: 13,
        SIGALRM: 14,
        SIGTERM: 15,
        SIGSTOP: 17,
        SIGTSTP: 18,
        SIGCONT: 19,
        SIGCHLD: 20,
        SIGUSR1: 30,
        SIGUSR2: 31,
      }
    : {
        SIGHUP: 1,
        SIGINT: 2,
        SIGQUIT: 3,
        SIGILL: 4,
        SIGTRAP: 5,
        SIGABRT: 6,
        SIGBUS: 7,
        SIGFPE: 8,
        SIGKILL: 9,
        SIGUSR1: 10,
        SIGSEGV: 11,
        SIGUSR2: 12,
        SIGPIPE: 13,
        SIGALRM: 14,
        SIGTERM: 15,
        SIGCHLD: 17,
        SIGCONT: 18,
        SIGSTOP: 19,
        SIGTSTP: 20,
      }
);

// The errors a plugin may name, with the number `errno` of this platform.
const errno = Object.freeze(
  darwin
    ? {
        EPERM: 1,
        ENOENT: 2,
        EBADF: 9,
        EACCES: 13,
        EEXIST: 17,
        EXDEV: 18,
        ENOTDIR: 20,
        EISDIR: 21,
        ENFILE: 23,
        EMFILE: 24,
        EINVAL: 22,
        ENOSPC: 28,
        EPIPE: 32,
        ELOOP: 62,
        ENAMETOOLONG: 63,
        ENOTEMPTY: 66,
        EAGAIN: 35,
      }
    : {
        EPERM: 1,
        ENOENT: 2,
        EBADF: 9,
        EAGAIN: 11,
        EACCES: 13,
        EEXIST: 17,
        EXDEV: 18,
        ENOTDIR: 20,
        EISDIR: 21,
        EINVAL: 22,
        ENFILE: 23,
        EMFILE: 24,
        ENOSPC: 28,
        EPIPE: 32,
        ENAMETOOLONG: 36,
        ENOTEMPTY: 39,
        ELOOP: 40,
      }
);

const priority = Object.freeze({
  PRIORITY_LOW: 19,
  PRIORITY_BELOW_NORMAL: 10,
  PRIORITY_NORMAL: 0,
  PRIORITY_ABOVE_NORMAL: -7,
  PRIORITY_HIGH: -14,
  PRIORITY_HIGHEST: -20,
});

export const constants = Object.freeze({ errno, priority, signals });

const os = raw.default;
os.constants = constants;

export default os;
