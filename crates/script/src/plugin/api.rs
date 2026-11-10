//! The `reagent` object a plugin's default export receives, and the staging
//! list its registrations land in until the plugin's load returned.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rquickjs::{Array, Ctx, Exception, Function, Object, Persistent, Value};
use serde_json::{Map, Value as JsonValue};

use super::runtime;
use super::{Live, Notes};

/// The events a plugin may listen on.
const EVENTS: [&str; 5] = ["run_start", "turn_start", "turn_end", "tool_end", "run_end"];

/// The hooks a plugin may install.
const HOOKS: [&str; 3] = ["tool.call", "tool.result", "turn.end"];

/// The roles a panel line may carry; any other role is dropped.
const ROLES: [&str; 7] = ["text", "dim", "strong", "accent", "good", "warn", "bad"];

/// The most lines one panel keeps.
const LINES: usize = 50;

/// The longest text one panel line may carry, in bytes.
const LINE_BYTES: usize = 512;

/// The longest text `reagent.send` queues, in bytes.
const SENT_BYTES: usize = 32 * 1024;

/// The lines a panel holds before its first `set`.
const NO_LINES: &str = "[]";

/// The schema a tool gets when it registers without one.
const NO_PARAMETERS: &str = r#"{"type":"object","properties":{}}"#;

/// The bit of `"run_start"`, and so on; the low bit is the first event.
pub(crate) fn event_bit(name: &str) -> Option<u8> {
    EVENTS
        .iter()
        .position(|event| *event == name)
        .map(|at| 1 << at)
}

/// One tool a plugin registered while it loaded.
pub(crate) struct StagedTool {
    /// The name the model calls.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: JsonValue,
    /// The function that runs it.
    pub run: Persistent<Function<'static>>,
}

/// One command a plugin registered while it loaded.
pub(crate) struct StagedCommand {
    /// The name the user types after the slash.
    pub name: String,
    /// What the command does.
    pub description: String,
    /// The function that runs it.
    pub run: Persistent<Function<'static>>,
}

/// One panel's state, shared by its live JS object and the load that commits
/// it.
pub(crate) struct Slot {
    /// The id clients see: `<plugin>/<id>`.
    pub id: String,
    /// What the panel is called.
    pub title: String,
    /// The lines, as one JSON array text.
    pub lines: String,
    /// Where a live `set` publishes, once the plugin's load returned.
    pub live: Option<Arc<Live>>,
}

/// What one plugin registered, kept aside until its load is known to have
/// returned without a throw.
#[derive(Default)]
pub(crate) struct Staging {
    /// The tools it registered.
    pub tools: Vec<StagedTool>,
    /// The commands it registered.
    pub commands: Vec<StagedCommand>,
    /// The panels it made, in creation order; the same list a second `panel`
    /// call finds an existing slot in.
    pub panels: Rc<RefCell<Vec<Rc<RefCell<Slot>>>>>,
    /// Its `tool.call` layers, in registration order.
    pub call: Vec<Persistent<Function<'static>>>,
    /// Its `tool.result` layers, in registration order.
    pub result: Vec<Persistent<Function<'static>>>,
    /// Its `turn.end` layers, in registration order.
    pub turn: Vec<Persistent<Function<'static>>>,
    /// Its event listeners with the event each one waits on.
    pub listeners: Vec<(String, Persistent<Function<'static>>)>,
}

/// The host side a plugin's `reagent` object writes to besides its staging
/// list: the notes, the panels and sent texts, and the command names taken.
pub(crate) struct Host {
    /// The notes a refusal or a skipped line raises.
    pub notes: Arc<Mutex<Notes>>,
    /// The panels and the texts the client sees.
    pub live: Arc<Live>,
    /// Every command name another plugin took.
    pub taken: Rc<RefCell<HashSet<String>>>,
    /// Whether the plugin's load committed; a panel it makes later goes to
    /// `live` itself, because no load is left to commit it.
    pub committed: Rc<Cell<bool>>,
}

/// Builds the `reagent` object for the plugin `name` at `version`, whose
/// registrations land in `staging`.
///
/// `names` holds every tool name already taken, so a duplicate throws at
/// register time rather than when the plugin takes it.
pub(crate) fn reagent<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    version: &str,
    staging: &Rc<RefCell<Staging>>,
    names: &Rc<RefCell<HashSet<String>>>,
    host: &Host,
) -> rquickjs::Result<Value<'js>> {
    let reagent = Object::new(ctx.clone())?;
    let register = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        let names = Rc::clone(names);
        move |ctx: Ctx<'js>, spec: Object<'js>| -> rquickjs::Result<()> {
            let name: String = spec
                .get("name")
                .map_err(|_| fail(&ctx, "registerTool needs a string name"))?;
            if !named(&name) {
                return Err(fail(
                    &ctx,
                    format!("bad tool name '{name}': use 1 to 64 letters, digits, '_' or '-'"),
                ));
            }
            if names.borrow().contains(&name)
                || staging.borrow().tools.iter().any(|t| t.name == name)
            {
                return Err(fail(&ctx, format!("duplicate tool name '{name}'")));
            }
            let run: Function<'js> = spec
                .get("run")
                .map_err(|_| fail(&ctx, format!("tool '{name}' needs a run function")))?;
            let description = spec
                .get::<_, Option<String>>("description")
                .ok()
                .flatten()
                .unwrap_or_default();
            let parameters = match spec.get::<_, Option<Value>>("parameters") {
                Ok(Some(value)) if value.is_object() => json_of(&ctx, value)
                    .unwrap_or_else(|| serde_json::from_str(NO_PARAMETERS).unwrap_or_default()),
                Ok(Some(_)) => {
                    return Err(fail(
                        &ctx,
                        format!("tool '{name}' needs a JSON Schema object as its parameters"),
                    ));
                }
                _ => serde_json::from_str(NO_PARAMETERS).unwrap_or_default(),
            };
            staging.borrow_mut().tools.push(StagedTool {
                name,
                description,
                parameters,
                run: Persistent::save(&ctx, run),
            });
            Ok(())
        }
    })?;
    reagent.set("registerTool", register)?;
    let register_command = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        let taken = Rc::clone(&host.taken);
        move |ctx: Ctx<'js>, name: String, spec: Object<'js>| -> rquickjs::Result<()> {
            if !named(&name) {
                return Err(fail(
                    &ctx,
                    format!("bad command name '{name}': use 1 to 64 letters, digits, '_' or '-'"),
                ));
            }
            if taken.borrow().contains(&name)
                || staging.borrow().commands.iter().any(|c| c.name == name)
            {
                return Err(fail(&ctx, format!("duplicate command name '{name}'")));
            }
            let run: Function<'js> = spec
                .get("run")
                .map_err(|_| fail(&ctx, format!("command '{name}' needs a run function")))?;
            let description = spec
                .get::<_, Option<String>>("description")
                .ok()
                .flatten()
                .unwrap_or_default();
            staging.borrow_mut().commands.push(StagedCommand {
                name,
                description,
                run: Persistent::save(&ctx, run),
            });
            Ok(())
        }
    })?;
    reagent.set("registerCommand", register_command)?;
    let on = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        move |ctx: Ctx<'js>, event: String, listener: Function<'js>| -> rquickjs::Result<()> {
            if !EVENTS.contains(&event.as_str()) {
                return Err(fail(
                    &ctx,
                    format!("unknown event '{event}': use one of {}", EVENTS.join(", ")),
                ));
            }
            staging
                .borrow_mut()
                .listeners
                .push((event, Persistent::save(&ctx, listener)));
            Ok(())
        }
    })?;
    reagent.set("on", on)?;
    let hook = Function::new(ctx.clone(), {
        let staging = Rc::clone(staging);
        move |ctx: Ctx<'js>, hook: String, layer: Function<'js>| -> rquickjs::Result<()> {
            let mut staged = staging.borrow_mut();
            let saved = Persistent::save(&ctx, layer);
            match hook.as_str() {
                "tool.call" => staged.call.push(saved),
                "tool.result" => staged.result.push(saved),
                "turn.end" => staged.turn.push(saved),
                _ => {
                    return Err(fail(
                        &ctx,
                        format!("unknown hook '{hook}': use one of {}", HOOKS.join(", ")),
                    ));
                }
            }
            Ok(())
        }
    })?;
    reagent.set("hook", hook)?;
    let slots = Rc::clone(&staging.borrow().panels);
    let panel = Function::new(ctx.clone(), {
        let slots = Rc::clone(&slots);
        let notes = Arc::clone(&host.notes);
        let live = Arc::clone(&host.live);
        let committed = Rc::clone(&host.committed);
        let plugin = name.to_string();
        move |ctx: Ctx<'js>, spec: Object<'js>| -> rquickjs::Result<Value<'js>> {
            let id: String = spec
                .get("id")
                .map_err(|_| fail(&ctx, "ui.panel needs a string id"))?;
            if !named(&id) {
                return Err(fail(
                    &ctx,
                    format!("bad panel id '{id}': use 1 to 64 letters, digits, '_' or '-'"),
                ));
            }
            let title = spec
                .get::<_, Option<String>>("title")
                .ok()
                .flatten()
                .unwrap_or_default();
            let full = format!("{plugin}/{id}");
            let found = slots
                .borrow()
                .iter()
                .find(|slot| slot.borrow().id == full)
                .map(Rc::clone);
            let slot = match found {
                Some(found) => {
                    let mut slot = found.borrow_mut();
                    slot.title = title;
                    if let Some(live) = &slot.live {
                        live.retitle(&slot.id, &slot.title);
                    }
                    drop(slot);
                    found
                }
                None => {
                    let slot = Rc::new(RefCell::new(Slot {
                        id: full,
                        title,
                        lines: NO_LINES.into(),
                        live: None,
                    }));
                    // A load that already returned has nothing left to commit:
                    // the host sees the panel now, and its `set` from now. A
                    // load that throws never gets here, and publishes nothing.
                    if committed.get() {
                        {
                            let fresh = slot.borrow();
                            live.add_panel(
                                fresh.id.clone(),
                                fresh.title.clone(),
                                fresh.lines.clone(),
                            );
                        }
                        slot.borrow_mut().live = Some(Arc::clone(&live));
                    }
                    slots.borrow_mut().push(Rc::clone(&slot));
                    slot
                }
            };
            let object = Object::new(ctx.clone())?;
            object.set("id", slot.borrow().id.as_str())?;
            let set = Function::new(ctx.clone(), {
                let slot = Rc::clone(&slot);
                let notes = Arc::clone(&notes);
                move |ctx: Ctx<'js>, lines: Value<'js>| -> rquickjs::Result<()> {
                    let Some(array) = lines.as_array() else {
                        return Err(fail(&ctx, "panel.set needs an array of lines"));
                    };
                    // Read every item first: a getter may call `set` again.
                    let (text, skipped) = render(&ctx, array);
                    let id = {
                        let mut slot = slot.borrow_mut();
                        slot.lines = text;
                        if let Some(live) = &slot.live {
                            live.publish(&slot.id, &slot.lines);
                        }
                        slot.id.clone()
                    };
                    if skipped > 0 {
                        runtime::note(
                            &notes,
                            format!("plugins: panel {id} skipped {skipped} bad lines"),
                        );
                    }
                    Ok(())
                }
            })?;
            object.set("set", set)?;
            Ok(object.into_value())
        }
    })?;
    let ui = Object::new(ctx.clone())?;
    ui.set("panel", panel)?;
    reagent.set("ui", ui)?;
    let send = Function::new(ctx.clone(), {
        let live = Arc::clone(&host.live);
        move |text: String| {
            // An empty text is nothing to say.
            if !text.is_empty() {
                live.send(cut(&text, SENT_BYTES).to_owned());
            }
        }
    })?;
    reagent.set("send", send)?;
    let plugin = Object::new(ctx.clone())?;
    plugin.set("name", name)?;
    plugin.set("version", version)?;
    reagent.set("plugin", plugin)?;
    Ok(reagent.into_value())
}

/// Throws `message` and returns the error that carries it.
fn fail(ctx: &Ctx<'_>, message: impl AsRef<str>) -> rquickjs::Error {
    Exception::throw_message(ctx, message.as_ref())
}

/// Whether `name` matches `^[a-zA-Z0-9_-]{1,64}$`.
fn named(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a char
/// boundary.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    &text[..text.floor_char_boundary(max)]
}

/// One panel's items as the JSON array text, and how many were skipped.
fn render<'js>(ctx: &Ctx<'js>, array: &Array<'js>) -> (String, usize) {
    let mut lines: Vec<JsonValue> = Vec::new();
    let mut skipped = 0;
    for at in 0..array.len().min(LINES) {
        let item = match array.get::<Value<'js>>(at) {
            Ok(item) => item,
            Err(_) => {
                // A throwing getter is one bad line, not a failed `set`.
                ctx.catch();
                skipped += 1;
                continue;
            }
        };
        match line(ctx, &item) {
            Some(line) => lines.push(line),
            None => skipped += 1,
        }
    }
    (JsonValue::Array(lines).to_string(), skipped)
}

/// One item of a `set` array as a line, or `None` when it is not one.
fn line<'js>(ctx: &Ctx<'js>, item: &Value<'js>) -> Option<JsonValue> {
    if let Some(text) = item.as_string().and_then(|text| text.to_string().ok()) {
        return Some(JsonValue::String(cut(&text, LINE_BYTES).to_owned()));
    }
    let object = item.as_object()?;
    if let Some(text) = field(ctx, object, "text") {
        let mut line = Map::new();
        line.insert(
            "text".into(),
            JsonValue::String(cut(&text, LINE_BYTES).to_owned()),
        );
        if let Some(role) = field(ctx, object, "role")
            && ROLES.contains(&role.as_str())
        {
            line.insert("role".into(), JsonValue::String(role));
        }
        return Some(JsonValue::Object(line));
    }
    if let Some(key) = field(ctx, object, "key") {
        let value = shown_value(ctx, object, "value")?;
        let mut line = Map::new();
        line.insert(
            "key".into(),
            JsonValue::String(cut(&key, LINE_BYTES).to_owned()),
        );
        line.insert("value".into(), JsonValue::String(value));
        return Some(JsonValue::Object(line));
    }
    if let Some(label) = field(ctx, object, "label") {
        let value = whole(ctx, object, "value")?;
        let max = whole(ctx, object, "max")?;
        let mut line = Map::new();
        line.insert(
            "label".into(),
            JsonValue::String(cut(&label, LINE_BYTES).to_owned()),
        );
        line.insert("value".into(), JsonValue::from(value));
        line.insert("max".into(), JsonValue::from(max));
        return Some(JsonValue::Object(line));
    }
    None
}

/// One string field of a line object; a read that throws is no field.
fn field<'js>(ctx: &Ctx<'js>, object: &Object<'js>, name: &str) -> Option<String> {
    match object.get::<_, Option<String>>(name) {
        Ok(value) => value,
        Err(_) => {
            ctx.catch();
            None
        }
    }
}

/// A `{ key, value }` value as the text to show: a number becomes its own.
fn shown_value<'js>(ctx: &Ctx<'js>, object: &Object<'js>, name: &str) -> Option<String> {
    let value = match object.get::<_, Option<Value<'js>>>(name) {
        Ok(value) => value?,
        Err(_) => {
            ctx.catch();
            return None;
        }
    };
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return Some(cut(&text, LINE_BYTES).to_owned());
    }
    if value.is_number() {
        return number_value(ctx, &value);
    }
    None
}

/// A `{ label, value, max }` number, clamped to a non-negative integer.
fn whole<'js>(ctx: &Ctx<'js>, object: &Object<'js>, name: &str) -> Option<u64> {
    let value = match object.get::<_, Option<Value<'js>>>(name) {
        Ok(value) => value?,
        Err(_) => {
            ctx.catch();
            return None;
        }
    };
    if !value.is_number() {
        return None;
    }
    let number: f64 = value.get().ok()?;
    // A fraction rounds down, `NaN` and everything below zero is 0, and
    // `Infinity` is the largest whole.
    Some(if number.is_nan() || number <= 0.0 {
        0
    } else if number >= u64::MAX as f64 {
        u64::MAX
    } else {
        number as u64
    })
}

/// A JSON number as the text of a `{ key, value }` line, spelled exactly as
/// the script's own `String(value)` spells it: `-0` is "0", `1e-7` stays
/// "1e-7" and `12` is "12".
fn number_value<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Option<String> {
    let to_text: Function = ctx.globals().get("String").ok()?;
    to_text.call::<_, String>((value.clone(),)).ok()
}

/// The JSON value of a JS value, as `JSON.stringify` writes it.
pub(crate) fn json_of<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Option<JsonValue> {
    let text = ctx.json_stringify(value).ok().flatten()?;
    serde_json::from_str(&text.to_string().ok()?).ok()
}

/// A JSON value as the JS value of the same shape.
pub(crate) fn from_json<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> rquickjs::Result<Value<'js>> {
    use rquickjs::IntoJs;
    Ok(match value {
        JsonValue::Null => Value::new_null(ctx.clone()),
        JsonValue::Bool(flag) => Value::new_bool(ctx.clone(), *flag),
        JsonValue::Number(number) => match number.as_i64() {
            Some(whole) => whole.into_js(ctx)?,
            None => number.as_f64().unwrap_or_default().into_js(ctx)?,
        },
        JsonValue::String(text) => text.as_str().into_js(ctx)?,
        JsonValue::Array(items) => {
            let array = Array::new(ctx.clone())?;
            for (at, item) in items.iter().enumerate() {
                array.set(at, from_json(ctx, item)?)?;
            }
            array.into_value()
        }
        JsonValue::Object(fields) => {
            let object = Object::new(ctx.clone())?;
            for (name, field) in fields {
                object.set(name.as_str(), from_json(ctx, field)?)?;
            }
            object.into_value()
        }
    })
}

/// The message of the value a plugin threw.
pub(crate) fn thrown<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> String {
    if let Some(exception) = value.as_exception()
        && let Some(message) = exception.message()
    {
        return message;
    }
    if let Some(text) = value.as_string().and_then(|text| text.to_string().ok()) {
        return text;
    }
    json_of(ctx, value).map_or_else(
        || "the plugin threw a value".into(),
        |json| json.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_letters_digits_underscore_and_dash() {
        assert!(named("shout"));
        assert!(named("read_file-2"));
        assert!(!named(""));
        assert!(!named("has space"));
        assert!(!named(&"a".repeat(65)));
    }

    #[test]
    fn events_have_their_own_bit() {
        assert_eq!(event_bit("run_start"), Some(1));
        assert_eq!(event_bit("run_end"), Some(16));
        assert_eq!(event_bit("nothing"), None);
    }

    #[test]
    fn a_cut_ends_on_a_char_boundary() {
        assert_eq!(cut("hello", 5), "hello");
        assert_eq!(cut("hello", 2), "he");
        assert_eq!(cut("a\u{e9}b", 2), "a");
        assert_eq!(cut("", 0), "");
    }
}
