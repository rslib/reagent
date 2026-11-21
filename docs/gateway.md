# The gateway

`reagent gateway` is one long-lived process per machine. It hands the questions
and the approvals a run parked to the gateway plugins, and takes their answers.
The owner is asked first, and may forward a question to a person of the
contacts file.

A run parks when the last client of its session leaves while a call waits for a
question or an approval. `reagent answer` lists what is parked and answers it
from the command line; the gateway does the same for its plugins.

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

## Approvals

`approval_parked` carries `{ qid, session, tool, call, rule, save, at, deadline,
token }`: the call the run asks about, the rule that asked, and the rule a
`save` writes (`null` when the call has none). `approval_closed` carries
`{ qid, how, by }`.

`reagent.gateway.approve(qid, token, answer, by, reason)` answers a Promise of
the same four results. `answer` is one of:

| answer | meaning |
|---|---|
| `"once"` | run this call |
| `"session"` | run it, and every later call its rules match in this session |
| `"save"` | as `"session"`, and save the rules for the project, as the TUI `a` key does; only when `save` is not `null` |
| `"refuse"` | do not run it; `reason`, when given, is what the model reads |

Any other word, a `reason` with a yes, `save` for a call with no rule to save,
or an answer of the other kind (`answer` for an approval, `approve` for a
question) is `"denied"`, and the approval stays open.

The session checks the call again when it starts, so the rules a `session` or
`save` answer adds are the ones that check shows, never text from the plugin.

From the command line, the answer is the word, or the JSON object with a reason:

```bash
echo once | reagent answer <qid>
echo '{"answer":"refuse","reason":"not on main"}' | reagent answer <qid>
```

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

## Forwarding

A gateway plugin may hand a parked question to one more person: the owner says
"ask Bob", and Bob answers. Only the people of `contacts.toml` in the global
config folder (`~/.config/reagent/contacts.toml`) may get one. A project folder
never holds this file.

```toml
[bob]
name = "Bob"          # what the model reads; the key when not set
channel = "telegram"  # the gateway plugin that reaches Bob, as its package.json names it
user = "123456789"    # that plugin's own id of Bob, such as a chat id
```

`reagent.gateway.forward(qid, token, to, note)` answers a Promise of the same
four results as `answer`. `to` is the key of the contact, and `note`, when
given, is text the model reads, cut to 1024 bytes. It is `"denied"` when the
token is wrong, when `to` is not in the file, when no gateway plugin has the
name of its `channel`, for an approval, and after 16 forwards of one question.

```js
reagent.gateway.forward(question.qid, question.token, "bob", "Bob owns the budget");
```

After `"ok"`, the plugin of the contact's `channel`, and no other plugin, gets
`question_forwarded` with `user` and a token for that person alone. That
plugin sends the question to `user`, and answers with
`reagent.gateway.answer` and the new token. The token works only from that
plugin. The person may also forward it with that token.

The owner's token stays good. The first answer closes the question, and every
later answer is `"stale"`. A forward does not move the deadline. Every forward
is in the journal, so a gateway that starts again tells the contact again,
with a new token.

The model reads who answered and who forwarded it, by the names of the
contacts file:

```text
Q: Which database?
A: postgres
Answered by Bob, forwarded by the owner with the note "Bob owns the budget".
```

An approval is never forwarded: only the owner gives one.

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

An approval nobody gives is refused at the deadline: the call does not run, and
the model reads `nobody answered the approval request before its deadline`. The
gateway fires `approval_closed` with `how: "expired"`.

Nothing expires while no gateway runs: a parked question waits until a gateway
starts, and one already past its deadline expires at once.

## What it does with an answer

The gateway writes the answer into the journal of the session with `by` set to
`gateway:<plugin name>:<by>`, removes the parked file, starts the session again
with no client, and then fires `question_closed` or `approval_closed`. When the restart fails, the
answer is still kept and the reason goes to the gateway's stderr as one line.
When the session is live, nothing is written and the result is `"busy"`.
