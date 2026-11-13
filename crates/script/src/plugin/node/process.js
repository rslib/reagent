// The `process` module of one plugin: LLRT's own object, with the calls that
// end or move this worker replaced by a throw and the environment behind the
// plugin's env permission.
//
// Every export is written out by name here: no lookup a plugin can patch decides
// what a name holds, and each refused call is a fixed function of its own.
import * as raw from "reagent:raw/process";
import { deny, env as envOf, envHas, envNames, index } from "reagent:raw/guard";

// The names the plugin's env permission decides: `env` and `envHas` answer for a
// name, `envNames` lists the ones a plugin may see, and every write throws.
const environment = new Proxy(
  {},
  {
    get(target, name) {
      if (typeof name !== "string") return undefined;
      return envOf(index, name);
    },
    has(target, name) {
      if (typeof name !== "string") return false;
      return envHas(index, name);
    },
    ownKeys() {
      return envNames(index);
    },
    getOwnPropertyDescriptor(target, name) {
      if (typeof name !== "string") return undefined;
      const value = envOf(index, name);
      if (value === undefined) return undefined;
      return { value, writable: true, enumerable: true, configurable: true };
    },
    set(target, name) {
      deny(index, "process.env");
    },
    deleteProperty(target, name) {
      deny(index, "process.env");
    },
  },
);

export const env = environment;
export const cwd = raw.cwd;
export const argv0 = raw.argv0;
export const id = raw.id;
export const argv = raw.argv;
export const platform = raw.platform;
export const arch = raw.arch;
export const hrtime = raw.hrtime;
export const release = raw.release;
export const version = raw.version;
export const versions = raw.versions;
export const exitCode = raw.exitCode;
export const nextTick = raw.nextTick;
export const getuid = raw.getuid;
export const getgid = raw.getgid;
export const geteuid = raw.geteuid;
export const getegid = raw.getegid;

// One fixed function per refused call: it holds its name and nothing a plugin
// writes can change what it does.
export const abort = function () {
  deny(index, "process.abort");
};
export const chdir = function () {
  deny(index, "process.chdir");
};
export const exit = function () {
  deny(index, "process.exit");
};
export const kill = function () {
  deny(index, "process.kill");
};
export const setegid = function () {
  deny(index, "process.setegid");
};
export const seteuid = function () {
  deny(index, "process.seteuid");
};
export const setgid = function () {
  deny(index, "process.setgid");
};
export const setuid = function () {
  deny(index, "process.setuid");
};

export default {
  env,
  cwd,
  argv0,
  id,
  argv,
  platform,
  arch,
  hrtime,
  release,
  version,
  versions,
  exitCode,
  nextTick,
  getuid,
  getgid,
  geteuid,
  getegid,
  abort,
  chdir,
  exit,
  kill,
  setegid,
  seteuid,
  setgid,
  setuid,
};
