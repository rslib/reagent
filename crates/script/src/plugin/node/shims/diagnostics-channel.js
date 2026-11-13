// The `diagnostics_channel` module of one plugin: channels of its own.
//
// A plugin has its own module instance, so the map below is its alone: a publish
// in one plugin never reaches a subscriber of another.
const channels = new Map();

// What a subscriber reports with: the function itself, or its `onMessage`.
function messageOf(subscriber) {
  if (typeof subscriber === "function") return subscriber;
  if (subscriber !== null && typeof subscriber === "object") {
    if (typeof subscriber.onMessage === "function") return subscriber.onMessage;
  }
  return undefined;
}

function makeChannel(name) {
  const subscribers = [];
  return {
    name,
    get hasSubscribers() {
      return subscribers.length > 0;
    },
    get subscribers() {
      return subscribers;
    },
    subscribe(subscriber) {
      const onMessage = messageOf(subscriber);
      if (onMessage === undefined) {
        throw new TypeError(
          'The "subscriber" argument must be a function or an object with an onMessage function'
        );
      }
      subscribers.push({ subscriber, onMessage });
    },
    unsubscribe(subscriber) {
      for (let index = 0; index < subscribers.length; index += 1) {
        const entry = subscribers[index];
        if (entry.subscriber === subscriber || entry.onMessage === subscriber) {
          subscribers.splice(index, 1);
          return true;
        }
      }
      return false;
    },
    // Every subscriber hears the message before the next one runs, in order.
    publish(message) {
      for (const entry of subscribers.slice()) {
        entry.onMessage.call(entry.subscriber, message, name);
      }
      return subscribers.length > 0;
    },
  };
}

function channel(name) {
  let found = channels.get(name);
  if (found === undefined) {
    found = makeChannel(name);
    channels.set(name, found);
  }
  return found;
}

function hasSubscribers(name) {
  return channel(name).hasSubscribers;
}

function subscribe(name, subscriber) {
  channel(name).subscribe(subscriber);
}

function unsubscribe(name, subscriber) {
  const found = channels.get(name);
  return found === undefined ? false : found.unsubscribe(subscriber);
}

// The events a tracing channel publishes around a call.
const EVENTS = ["start", "end", "asyncStart", "asyncEnd", "error"];

const START = 0;
const END = 1;
const ERROR = 4;

// A tracing channel publishes to its own channels and calls the function it is
// given with the arguments of the call.
function tracingChannel(name) {
  const events = EVENTS.map((event) => channel(`${name}:${event}`));
  const call = (context, body, settle) => {
    events[START].publish(context);
    try {
      const value = body();
      events[END].publish({ ...context, result: value });
      return settle === undefined ? value : settle(value);
    } catch (error) {
      events[ERROR].publish({ ...context, error });
      throw error;
    }
  };
  return {
    name,
    get hasSubscribers() {
      return events.some((event) => event.hasSubscribers);
    },
    subscribe(subscriber) {
      for (const event of events) event.subscribe(subscriber);
    },
    unsubscribe(subscriber) {
      for (const event of events) event.unsubscribe(subscriber);
    },
    traceSync(fn, context, thisArg, ...args) {
      return call(context, () => fn.apply(thisArg, args));
    },
    tracePromise(fn, context, thisArg, ...args) {
      return Promise.resolve(call(context, () => fn.apply(thisArg, args)));
    },
    traceCallback(fn, _position, context, thisArg, ...args) {
      return call(context, () => fn.apply(thisArg, args));
    },
  };
}

export { channel, hasSubscribers, subscribe, tracingChannel, unsubscribe };

export default {
  channel,
  hasSubscribers,
  subscribe,
  tracingChannel,
  unsubscribe,
};
