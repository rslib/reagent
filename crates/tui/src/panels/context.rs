//! The Context panel: the model, the context use of the last request, the cost
//! so far and the session.
//!
//! Every text of the panel is built when the data changes, so a steady draw
//! hands out the same `Arc<str>`s and allocates nothing. The token counts keep
//! small integer maths, and the cost is prices times the tokens spent so far,
//! so a price that arrives after a turn still prices it.

use std::sync::Arc;

use crate::slot::{Component, Panel, Priority, Role, Size, Text, Update};

use super::Right;

/// The fold order: the context use is the last panel to fold.
const PRIORITY: Priority = 9;

/// The narrowest sidebar this panel draws in.
const MIN_WIDTH: u16 = 20;

/// The tokens of a million, which prices are given for.
const MILLION: f64 = 1_000_000.0;

/// The key of the row that shows the context use.
const TOKENS: &str = "tokens";

/// The key of the row that shows the cost so far.
const COST: &str = "cost";

/// The key of the row that shows the session.
const SESSION: &str = "session";

/// What separates the level from the mode, on the line under the model.
const DOT: &str = " · ";

/// The columns of a short session id, as the mockup shows it.
const SHORT_ID: usize = 8;

/// What a value reads when the panel does not know it.
const NONE: &str = "-";

/// The bytes of reply text a token stands for, as the context use counts the
/// bytes that stream before the usage of their request arrives.
const BYTES_PER_TOKEN: u64 = 4;

/// The Context panel.
#[derive(Debug)]
pub struct ContextPanel {
    title: Text,
    /// The model, a strong line: `-` until a `Model` update.
    model: Text,
    /// The context window of the model, zero until a `Model` update.
    window: u64,
    /// The input tokens of the last request.
    input: u64,
    /// The tokens of the last request that its provider's cache read and wrote.
    cached: u64,
    /// The output tokens of the last request.
    output: u64,
    /// The bytes of reply text and thinking that came since the last usage.
    streamed: u64,
    /// The input tokens spent so far, so a late price still prices them.
    input_spent: u64,
    /// The output tokens spent so far.
    output_spent: u64,
    /// The price of a million input and of a million output tokens.
    prices: Option<(f64, f64)>,
    /// `50k / 200k`.
    tokens: Text,
    /// `$0.42`, or `-` without prices.
    cost: Text,
    /// `25%`, the share of the window the last request used, for the header
    /// row's small bar.
    share: Text,
    /// The share in percent, so a draw reads no text to find it.
    percent: u64,
    /// The context use the tokens row draws, in the value and the unit [`short`]
    /// gives it, so a streamed byte that does not move it builds no text.
    drawn: (u64, &'static str),
    /// `high · default mode`, empty until the app says what they are.
    settings: Text,
    /// The short id of the session, empty until the app names it.
    session: Text,
    /// The tokens row's value, pushed to the right edge of its row.
    right: Right,
    /// The cost row's value, pushed to the right edge of its row.
    right_cost: Right,
    /// The session id, padded to the right edge as the cost is.
    right_session: Right,
}

impl ContextPanel {
    /// An empty panel.
    pub fn new() -> ContextPanel {
        ContextPanel {
            title: Text::Static("Context"),
            model: Text::Static(NONE),
            window: 0,
            input: 0,
            cached: 0,
            output: 0,
            streamed: 0,
            input_spent: 0,
            output_spent: 0,
            prices: None,
            tokens: Text::Static(NONE),
            cost: Text::Static(NONE),
            share: Text::Static(NONE),
            percent: 0,
            drawn: (0, ""),
            settings: Text::Empty,
            session: Text::Empty,
            right: Right::default(),
            right_cost: Right::default(),
            right_session: Right::default(),
        }
    }

    /// The price of a million input and of a million output tokens, and say
    /// whether the panel draws something new.
    pub fn set_prices(&mut self, input_per_million: f64, output_per_million: f64) -> bool {
        let prices = (input_per_million, output_per_million);
        if self.prices == Some(prices) {
            return false;
        }
        self.prices = Some(prices);
        self.build_cost();
        true
    }

    /// Take the level and the permission mode the worker runs with, as the line
    /// under the model reads them, and say whether the panel draws something
    /// new. A level or a mode the app does not know is left out.
    pub fn set_settings(&mut self, level: Option<&str>, mode: Option<&str>) -> bool {
        let text = match (level, mode) {
            (Some(level), Some(mode)) => format!("{level}{DOT}{mode}"),
            (Some(one), None) | (None, Some(one)) => one.to_owned(),
            (None, None) => String::new(),
        };
        if self.settings.as_str() == text {
            return false;
        }
        self.settings = if text.is_empty() {
            Text::Empty
        } else {
            Text::Shared(Arc::from(text))
        };
        true
    }

    /// Take the id of this session, which the panel shows short: the first
    /// [`SHORT_ID`] characters of it, in capitals as the mockup reads.
    pub fn set_session(&mut self, id: &str) -> bool {
        let short: String = id.chars().take(SHORT_ID).collect::<String>().to_uppercase();
        if self.session.as_str() == short {
            return false;
        }
        self.session = if short.is_empty() {
            Text::Empty
        } else {
            Text::Shared(Arc::from(short))
        };
        true
    }

    /// The share of the window the last request used, in percent, for the header
    /// row's bar.
    pub fn percent(&self) -> u64 {
        self.percent
    }

    /// The share as the header row reads it: `25%`.
    pub fn share(&self) -> &Text {
        &self.share
    }

    /// The cost as the header row reads it: `$0.42`.
    pub fn cost(&self) -> &Text {
        &self.cost
    }

    /// The model and its window, for the header row's mark when nothing else
    /// says what the run is.
    pub fn model_name(&self) -> &Text {
        &self.model
    }

    /// Take the model and its context window.
    fn model(&mut self, name: &str, window: u64) -> bool {
        if self.model.as_str() == name && self.window == window {
            return false;
        }
        self.model = Text::Shared(Arc::from(name));
        self.window = window;
        self.build_tokens();
        true
    }

    /// Take the token counts of the last request: the input, the cache tokens
    /// and the output are the context use, and the input and the output add to
    /// the cost. The bytes that streamed before the request are counted in it,
    /// so the count starts again at zero.
    fn usage(&mut self, input: u64, cached: u64, output: u64) -> bool {
        // The tokens of every request add to the cost, whatever they repeat.
        self.input_spent = self.input_spent.saturating_add(input);
        self.output_spent = self.output_spent.saturating_add(output);
        self.input = input;
        self.cached = cached;
        self.output = output;
        self.streamed = 0;
        let moved = self.refresh_tokens();
        let priced = self.prices.is_some() && (input > 0 || output > 0);
        if priced {
            self.build_cost();
        }
        moved || priced
    }

    /// Take the bytes of a reply that streamed since the last update, and say
    /// whether the panel draws something new.
    fn streamed(&mut self, bytes: usize) -> bool {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.streamed = self.streamed.saturating_add(bytes);
        self.refresh_tokens()
    }

    /// The context the last request and the bytes since it read: the input,
    /// the cache tokens and the output of the last usage, and a token for every
    /// [`BYTES_PER_TOKEN`] bytes that streamed after it.
    fn use_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cached)
            .saturating_add(self.output)
            .saturating_add(self.streamed / BYTES_PER_TOKEN)
    }

    /// Build the tokens row when the use it draws moved, and say whether it
    /// did: a streamed byte that rounds to the value already on screen builds
    /// no text.
    fn refresh_tokens(&mut self) -> bool {
        let use_tokens = self.use_tokens();
        let percent = percent(use_tokens, self.window);
        if rounded(use_tokens) == self.drawn && percent == self.percent {
            return false;
        }
        self.build_tokens();
        true
    }

    /// Build the tokens row: the context use against the window, and the share
    /// of the window it is, for the bar the header row shows.
    fn build_tokens(&mut self) {
        let use_tokens = self.use_tokens();
        self.drawn = rounded(use_tokens);
        self.percent = percent(use_tokens, self.window);
        self.share = if self.window == 0 && use_tokens == 0 {
            Text::Static(NONE)
        } else {
            Text::Shared(Arc::from(format!("{}%", self.percent)))
        };
        self.tokens = if self.window == 0 && use_tokens == 0 {
            Text::Static(NONE)
        } else if self.window == 0 {
            Text::Shared(Arc::from(short(use_tokens)))
        } else {
            Text::Shared(Arc::from(format!(
                "{} / {}",
                short(use_tokens),
                short(self.window)
            )))
        };
    }

    /// Build the cost row from the tokens spent and the prices, if there are
    /// prices yet.
    fn build_cost(&mut self) {
        self.cost = match self.prices {
            None => Text::Static(NONE),
            Some((input, output)) => {
                let spent = self.input_spent as f64 * input / MILLION
                    + self.output_spent as f64 * output / MILLION;
                Text::Shared(Arc::from(format!("${spent:.2}")))
            }
        };
    }
}

impl Default for ContextPanel {
    fn default() -> Self {
        ContextPanel::new()
    }
}

impl Panel for ContextPanel {
    fn id(&self) -> &str {
        "context"
    }

    fn title(&self) -> &Text {
        &self.title
    }

    fn priority(&self) -> Priority {
        PRIORITY
    }

    fn min_width(&self) -> u16 {
        MIN_WIDTH
    }

    fn update(&mut self, update: &Update<'_>) -> bool {
        match *update {
            Update::Model { name, window } => self.model(name, window),
            Update::Usage {
                input,
                cached,
                output,
            } => self.usage(input, cached, output),
            Update::Streamed { bytes } => self.streamed(bytes),
            _ => false,
        }
    }

    fn draw(&self, size: Size, out: &mut Vec<Component>) {
        out.push(Component::strong(self.model.clone()));
        if !self.settings.as_str().is_empty() {
            out.push(Component::dim(self.settings.clone()));
        }
        out.push(Component::row(
            TOKENS,
            self.right.at(TOKENS, &self.tokens, size.width),
        ));
        out.push(Component::progress(
            Text::Empty,
            self.use_tokens(),
            self.window,
        ));
        out.push(Component::row(
            COST,
            self.right_cost.at(COST, &self.cost, size.width),
        ));
        if !self.session.as_str().is_empty() {
            out.push(Component::row_with(
                SESSION,
                self.right_session.at(SESSION, &self.session, size.width),
                Role::Dim,
            ));
        }
        // One blank row between the panels.
        out.push(Component::text(Text::Static("")));
    }
}

/// A token count as a short text: `999`, `1k`, `50k`, `2M`, rounded to the
/// nearest of the unit.
fn short(value: u64) -> String {
    let (number, unit) = rounded(value);
    format!("{number}{unit}")
}

/// The value [`short`] draws for a token count, rounded to its unit, and the
/// unit it draws it with: exact under a thousand, then thousands, then
/// millions.
fn rounded(value: u64) -> (u64, &'static str) {
    if value < 1_000 {
        return (value, "");
    }
    let thousands = (value + 500) / 1_000;
    if thousands < 1_000 {
        return (thousands, "k");
    }
    ((value + 500_000) / 1_000_000, "M")
}

/// The share of a `window` that `value` tokens are, in percent.
fn percent(value: u64, window: u64) -> u64 {
    match window {
        0 => 0,
        window => (value.saturating_mul(100) / window).min(100),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(panel: &ContextPanel) -> Vec<Component> {
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        out
    }

    /// Every component as text, for comparing two draws.
    fn rendered(components: &[Component]) -> Vec<String> {
        components
            .iter()
            .map(|component| format!("{component:?}"))
            .collect()
    }

    /// The value of a `Row` component.
    fn value_of(components: &[Component], key: &str) -> String {
        for component in components {
            if let Component::Row { key: k, value } = component
                && k.as_str() == key
            {
                return value.as_str().to_owned();
            }
        }
        panic!("no {key} row in {components:?}");
    }

    /// The text of the strong line, which is the model.
    fn model_of(components: &[Component]) -> String {
        for component in components {
            if let Component::Text { text, role } = component
                && *role == Role::Strong
            {
                return text.as_str().to_owned();
            }
        }
        panic!("no model line in {components:?}");
    }

    /// The context window the streaming tests read the row against.
    const WINDOW: u64 = 200_000;

    /// A panel on a model whose window is [`WINDOW`].
    fn window_panel() -> ContextPanel {
        let mut panel = ContextPanel::new();
        panel.update(&Update::Model {
            name: "a-model",
            window: WINDOW,
        });
        panel
    }

    /// The tokens row of a panel, as it stands.
    fn tokens_of(panel: &ContextPanel) -> String {
        value_of(&drawn(panel), TOKENS)
    }

    #[test]
    fn a_turn_shows_the_tokens_against_the_window_and_the_cost() {
        let mut panel = ContextPanel::new();
        assert!(panel.set_prices(3.0, 15.0));
        assert!(panel.update(&Update::Model {
            name: "a-model",
            window: 200_000,
        }));
        assert!(panel.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 0,
        }));
        let out = drawn(&panel);
        assert_eq!(out.len(), 5, "four rows and the blank one: {out:?}");
        assert_eq!(model_of(&out), "a-model");
        let tokens = value_of(&out, TOKENS);
        assert_eq!(
            tokens.chars().count(),
            18,
            "the row's whole room: {tokens:?}"
        );
        assert!(tokens.ends_with("50k / 200k"), "{tokens:?}");
        let cost = value_of(&out, COST);
        assert!(cost.ends_with("$0.15"), "{cost:?}");
        assert!(
            matches!(
                out[2],
                Component::Progress {
                    value: 50_000,
                    max: 200_000,
                    ..
                }
            ),
            "{:?}",
            out[2]
        );
        assert_eq!(panel.percent(), 25);
        assert_eq!(panel.share().as_str(), "25%");
    }

    #[test]
    fn the_cost_adds_each_usage() {
        let mut panel = ContextPanel::new();
        panel.set_prices(1.0, 2.0);
        panel.update(&Update::Model {
            name: "m",
            window: 1_000_000,
        });
        assert!(panel.update(&Update::Usage {
            input: 1_000_000,
            cached: 0,
            output: 1_000_000,
        }));
        assert!(matches!(
            value_of(&drawn(&panel), COST).as_str(),
            cost if cost.ends_with("$3.00")
        ));
        assert!(panel.update(&Update::Usage {
            input: 500_000,
            cached: 0,
            output: 0,
        }));
        let out = drawn(&panel);
        assert!(value_of(&out, COST).ends_with("$3.50"));
        assert!(value_of(&out, TOKENS).ends_with("500k / 1M"));
    }

    #[test]
    fn without_prices_the_cost_is_a_dash() {
        let mut panel = ContextPanel::new();
        assert!(value_of(&drawn(&panel), COST).ends_with(NONE));
        assert!(panel.update(&Update::Usage {
            input: 10,
            cached: 0,
            output: 20,
        }));
        assert!(value_of(&drawn(&panel), COST).ends_with(NONE));
        // A price that arrives after the turn still prices it.
        assert!(panel.set_prices(1_000_000.0, 0.0));
        assert!(value_of(&drawn(&panel), COST).ends_with("$10.00"));
    }

    #[test]
    fn the_level_and_the_mode_read_under_the_model() {
        let mut panel = ContextPanel::new();
        assert!(!panel.set_settings(None, None), "nothing to say");
        assert!(panel.set_settings(Some("high"), Some("default mode")));
        let out = drawn(&panel);
        assert!(
            matches!(
                &out[1],
                Component::Text { text, role } if text.as_str() == "high · default mode"
                    && *role == Role::Dim
            ),
            "{:?}",
            out[1]
        );
        assert!(!panel.set_settings(Some("high"), Some("default mode")));
        assert!(panel.set_settings(Some("high"), None));
        assert_eq!(panel.settings.as_str(), "high");
    }

    #[test]
    fn the_session_reads_short_at_the_right() {
        let mut panel = ContextPanel::new();
        assert!(!panel.set_session(""), "no id, no row");
        assert!(panel.set_session("01m68hgz7c9d"));
        let out = drawn(&panel);
        let row = out.len() - 2;
        assert!(
            matches!(
                &out[row],
                Component::Row { key, value }
                    if key.as_str() == SESSION
                        && value.as_str().ends_with(" 01M68HGZ")
                        && value.as_str().trim_start() == "01M68HGZ"
                        && value.role() == Role::Dim
            ),
            "{out:?}"
        );
        assert_eq!(
            SESSION.len() + value_of(&out, SESSION).len(),
            COST.len() + value_of(&out, COST).len(),
            "the id ends at the right edge, where the cost ends"
        );
        assert!(!panel.set_session("01m68hgz7c9d"), "the same id");
    }

    #[test]
    fn numbers_use_k_and_m_suffixes() {
        assert_eq!(short(0), "0");
        assert_eq!(short(999), "999");
        assert_eq!(short(1_000), "1k");
        assert_eq!(short(50_000), "50k");
        assert_eq!(short(200_000), "200k");
        assert_eq!(short(2_000_000), "2M");
    }

    /// The tokens row reads the cache of the last request with its input, and
    /// the cost still prices the input alone: 1,000 in and 49,000 cached read
    /// as 50k, and cost a dollar at a thousand dollars a million.
    #[test]
    fn stream_tokens_count_the_cache_of_the_last_request() {
        let mut panel = window_panel();
        panel.set_prices(1_000.0, 2_000.0);
        assert!(panel.update(&Update::Usage {
            input: 1_000,
            cached: 49_000,
            output: 0,
        }));
        let out = drawn(&panel);
        let tokens = value_of(&out, TOKENS);
        assert!(tokens.ends_with("50k / 200k"), "{tokens:?}");
        let cost = value_of(&out, COST);
        assert!(
            cost.ends_with("$1.00"),
            "the cost prices the input and the output alone: {cost:?}"
        );
    }

    /// The row reads the output of the last request too.
    #[test]
    fn stream_tokens_count_the_output_of_the_last_request() {
        let mut panel = window_panel();
        assert!(panel.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 1_000,
        }));
        assert!(tokens_of(&panel).ends_with("51k / 200k"));
    }

    /// A streamed reply moves the row as its bytes arrive, four bytes to a
    /// token, and a stream that does not move the drawn value draws nothing.
    #[test]
    fn stream_tokens_move_the_row_before_the_usage_arrives() {
        let mut panel = window_panel();
        panel.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 0,
        });
        assert!(tokens_of(&panel).ends_with("50k / 200k"));
        assert!(
            panel.update(&Update::Streamed { bytes: 8_000 }),
            "8,000 bytes are 2k tokens over the usage"
        );
        assert!(tokens_of(&panel).ends_with("52k / 200k"));
        assert!(
            !panel.update(&Update::Streamed { bytes: 1 }),
            "a byte that rounds to the same row draws nothing"
        );
        assert!(tokens_of(&panel).ends_with("52k / 200k"));
    }

    /// Many small streams add up to the row one big one draws.
    #[test]
    fn stream_tokens_add_up_the_same_in_many_updates_or_one() {
        let mut many = window_panel();
        many.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 0,
        });
        for _ in 0..80 {
            many.update(&Update::Streamed { bytes: 100 });
        }
        let mut one = window_panel();
        one.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 0,
        });
        one.update(&Update::Streamed { bytes: 8_000 });
        assert_eq!(tokens_of(&many), tokens_of(&one), "the same 8,000 bytes");
        assert!(tokens_of(&many).ends_with("52k / 200k"));
    }

    /// The usage that follows a stream starts the count again, so the estimate
    /// of the stream goes and the row reads the request alone.
    #[test]
    fn stream_tokens_go_when_the_usage_behind_them_arrives() {
        let mut panel = window_panel();
        assert!(panel.update(&Update::Streamed { bytes: 20_000 }));
        assert!(tokens_of(&panel).ends_with("5k / 200k"), "an estimate");
        assert!(panel.update(&Update::Usage {
            input: 53_000,
            cached: 0,
            output: 0,
        }));
        assert!(
            tokens_of(&panel).ends_with("53k / 200k"),
            "the estimate is gone"
        );
        assert!(panel.update(&Update::Streamed { bytes: 4_000 }));
        assert!(tokens_of(&panel).ends_with("54k / 200k"));
    }

    /// The share of the window the header row draws moves with the stream.
    #[test]
    fn stream_tokens_move_the_share_with_them() {
        let mut panel = window_panel();
        panel.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 0,
        });
        assert_eq!(panel.percent(), 25);
        assert!(panel.update(&Update::Streamed { bytes: 200_000 }));
        assert_eq!(panel.percent(), 50, "50k tokens over the 50k of the usage");
        assert_eq!(panel.share().as_str(), "50%");
    }

    #[test]
    fn an_ignored_update_changes_nothing() {
        let mut panel = ContextPanel::new();
        panel.update(&Update::Model {
            name: "m",
            window: 1_000,
        });
        let before = rendered(&drawn(&panel));
        for update in [
            Update::Tick,
            Update::Busy(false),
            Update::ToolCall {
                name: "write",
                arguments: r#"{"path":"a.rs","content":"one"}"#,
            },
        ] {
            assert!(!panel.update(&update), "{update:?}");
        }
        assert_eq!(rendered(&drawn(&panel)), before);
        assert!(!panel.wants_tick());
    }

    #[test]
    fn a_second_draw_keeps_its_buffer_and_returns_equal_components() {
        let mut panel = ContextPanel::new();
        panel.set_prices(3.0, 15.0);
        panel.update(&Update::Model {
            name: "a-model",
            window: 200_000,
        });
        panel.update(&Update::Usage {
            input: 50_000,
            cached: 0,
            output: 1_000,
        });
        let mut out = Vec::new();
        panel.draw(Size::new(28, 12), &mut out);
        let capacity = out.capacity();
        let first = rendered(&out);
        for width in [28, 40, 28] {
            out.clear();
            panel.draw(Size::new(width, 12), &mut out);
            assert_eq!(out.capacity(), capacity, "the buffer grew");
            if width == 28 {
                assert_eq!(rendered(&out), first, "the same components drew");
            }
        }
    }
}
