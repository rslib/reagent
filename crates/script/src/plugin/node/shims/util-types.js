// The `types` module of one plugin: the `types` LLRT's `util` has when it has
// one, and reagent's own checks otherwise.
//
// Every check reads the tag `Object.prototype.toString` gives a value, which the
// value itself does not decide. A proxy answers the tag of what it wraps and
// QuickJS tells no host that a value is one, so `isProxy` answers `false`.
import * as raw from "reagent:raw/util";

function tagOf(value) {
  return Object.prototype.toString.call(value);
}

// The tags of one group of names, so a check is one lookup.
function tags(...names) {
  const out = Object.create(null);
  for (const name of names) out[`[object ${name}]`] = true;
  return out;
}

const arrayBuffers = tags("ArrayBuffer", "SharedArrayBuffer");
const boxed = tags("Number", "String", "Boolean", "Symbol", "BigInt");
const errors = tags(
  "Error",
  "EvalError",
  "RangeError",
  "ReferenceError",
  "SyntaxError",
  "TypeError",
  "URIError",
  "AggregateError"
);
const typed = tags(
  "Int8Array",
  "Uint8Array",
  "Uint8ClampedArray",
  "Int16Array",
  "Uint16Array",
  "Int32Array",
  "Uint32Array",
  "Float32Array",
  "Float64Array",
  "BigInt64Array",
  "BigUint64Array"
);

// The source of a function is what tells a generator and an async function from
// a plain one, whatever tag the runtime gives it.
function generator(value) {
  if (typeof value !== "function") return false;
  const source = Function.prototype.toString.call(value).trimStart();
  const rest = source.startsWith("async") ? source.slice(5).trimStart() : source;
  return rest.startsWith("function*") || rest.startsWith("function *");
}

const own = {
  isAnyArrayBuffer(value) {
    return arrayBuffers[tagOf(value)] === true;
  },
  isArrayBuffer(value) {
    return tagOf(value) === "[object ArrayBuffer]";
  },
  isAsyncFunction(value) {
    if (typeof value !== "function" || generator(value)) return false;
    return Function.prototype.toString.call(value).trimStart().startsWith("async");
  },
  isBoxedPrimitive(value) {
    return typeof value === "object" && value !== null && boxed[tagOf(value)] === true;
  },
  isDate(value) {
    return tagOf(value) === "[object Date]";
  },
  isGeneratorFunction: generator,
  isMap(value) {
    return tagOf(value) === "[object Map]";
  },
  isNativeError(value) {
    return errors[tagOf(value)] === true;
  },
  isPromise(value) {
    return tagOf(value) === "[object Promise]";
  },
  isProxy() {
    return false;
  },
  isRegExp(value) {
    return tagOf(value) === "[object RegExp]";
  },
  isSet(value) {
    return tagOf(value) === "[object Set]";
  },
  isTypedArray(value) {
    return typed[tagOf(value)] === true;
  },
  isUint8Array(value) {
    return tagOf(value) === "[object Uint8Array]";
  },
  isWeakMap(value) {
    return tagOf(value) === "[object WeakMap]";
  },
  isWeakSet(value) {
    return tagOf(value) === "[object WeakSet]";
  },
};

const types = raw.types === undefined ? Object.freeze(own) : raw.types;

const {
  isAnyArrayBuffer,
  isArrayBuffer,
  isAsyncFunction,
  isBoxedPrimitive,
  isDate,
  isGeneratorFunction,
  isMap,
  isNativeError,
  isPromise,
  isProxy,
  isRegExp,
  isSet,
  isTypedArray,
  isUint8Array,
  isWeakMap,
  isWeakSet,
} = types;

export {
  isAnyArrayBuffer,
  isArrayBuffer,
  isAsyncFunction,
  isBoxedPrimitive,
  isDate,
  isGeneratorFunction,
  isMap,
  isNativeError,
  isPromise,
  isProxy,
  isRegExp,
  isSet,
  isTypedArray,
  isUint8Array,
  isWeakMap,
  isWeakSet,
  types,
};

export default types;
