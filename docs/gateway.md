# The gateway

`reagent gateway` is one long-lived process per machine. It hands the questions
a run parked to the gateway plugins, and takes their answers. Only its owner is
asked: nothing is forwarded and no approval is taken.

## Start it

```bash
reagent gateway
```

It runs in the foreground until `SIGINT` or `SIGTERM`, and then exits 0. Keep it
running with your service manager, for example:

```
# launchd: ~/Library/LaunchAgents/dev.reagent.gateway.plist
ProgramArguments = ["/path/to/reagent", "gateway"]
KeepAlive = true
```

```
# systemd: ~/.config/systemd/user/reagent-gateway.service
ExecStart=/path/to/reagent gateway
Restart=always
```

It says `reagent: a gateway already runs` and exits 1 when another gateway of the
same state folder holds the lock. It says `reagent: no gateway plugins in <dir>`
and exits 1 when no plugin has the gateway role.

## A gateway plugin

`<config>/plugins/<folder>/package.json` names the role:

```json
{
  "name": "telegram",
  "reagent": {
    "entry": "index.js",
    "role": "gateway",
    "permissions": { "net": ["api.telegram.org"] }
  }
}
```

The only role is `"gateway"`; any other value is refused, and the message names
it. A normal run (`-p`, the worker, the TUI) skips gateway plugins without a
word, and `reagent gateway` loads only them. Permissions work as for any plugin:
a plugin that fetches needs `net`.

## Events and the answer

`question_parked` carries `{ qid, session, questions, at, deadline, token }`.
`question_closed` carries `{ qid, how, by }`. Both reach gateway plugins only,
and `docs/plugin-events.md` has each key and type.

`reagent.gateway.answer(qid, token, answers, by)` answers a Promise of one
string. `answers` is the JSON list the model asked for, one list per question,
or `null` for none. `by` is a name you choose, such as a chat user name, cut to
64 bytes.

| result | meaning |
|---|---|
| `"ok"` | the answer is in the journal; the gateway then starts the session again |
| `"denied"` | the token is wrong, or the answers have the wrong shape or count |
| `"stale"` | the question is no longer open, or the gateway never announced it |
| `"busy"` | the answer was not written: the session is live (answer it in its client) or its journal could not be written; the question stays open, so you may try again |

The token is what lets an answer in. It is random, it lives only in the memory
of the gateway, and every gateway start hands out new ones, so announce your
questions again and drop a repeat by `qid`. Every gateway plugin hears every
question with the same token, so install only gateway plugins you trust.

Answer from a promise callback or a timer, not by awaiting inside a
`question_parked` listener: a listener has a few seconds.

```js
import { readFileSync, writeFileSync } from "node:fs";

export default function (reagent) {
  reagent.on("question_parked", (question) => {
    writeFileSync("/tmp/questions.json", JSON.stringify(question));
  });
  reagent.on("question_closed", (closed) => {
    writeFileSync("/tmp/closed.json", JSON.stringify(closed));
  });
  setInterval(() => {
    let ask;
    try {
      ask = JSON.parse(readFileSync("/tmp/answer.json", "utf8"));
    } catch {
      return;
    }
    reagent.gateway
      .answer(ask.qid, ask.token, ask.answers, ask.by)
      .then((result) => writeFileSync("/tmp/result.txt", result));
  }, 1000);
}
```

## The deadline

A question waits `[gateway] deadline` from the moment it was parked:

```toml
[gateway]
deadline = "72h"
```

The units are `s`, `m`, `h` and `d`, and `"off"` means never. The default is
`72h`, and a wrong value is a config error that names it. Only the global config
file and a profile may hold the table.

At the deadline the gateway writes an answer with no lists, so the run goes on
with `No answer; decide yourself and state the choice.` It then restarts the
session and fires `question_closed` with `how: "expired"`.

Nothing expires while no gateway runs: a parked question waits until a gateway
starts, and one already past its deadline expires at once.

## What it does with an answer

The gateway writes the answer into the journal of the session with `by` set to
`gateway:<plugin name>:<by>`, removes the parked file, starts the session again
with no client, and then fires `question_closed`. When the restart fails, the
answer is still kept and the reason goes to the gateway's stderr as one line.
When the session is live, nothing is written and the result is `"busy"`.
