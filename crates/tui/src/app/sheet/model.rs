//! The model sheet.
//!
//! The rows are built once, when the sheet opens: the recent models first, then
//! a group per provider with each model's window and prices. The pick answers
//! the model id the `set` request carries.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::host::{Model, Models, Provider};

use super::{Kind, Mark, Row, Sheet};

/// The word of the title row.
const TITLE: &str = "Model";

/// The hint at the right edge of the title row.
const HINT: &str = "context \u{b7} $ in/out per M";

/// The keys of the footer row, until a keys file moves one.
const FOOTER: &str =
    "\u{2191}\u{2193} choose \u{b7} enter switches the next turn \u{b7} esc back to your draft";

/// The name of the group of the recent models.
const RECENT: &str = "recent";

/// The columns a model's window takes, right-aligned, before its prices.
const WINDOW: usize = 6;

/// The text at the right edge of the row of the model the session runs.
const NOW: &str = "now";

/// The model sheet for `models`, the model the session runs marked as current.
pub(crate) fn model_sheet(models: &Models, current: Option<&str>) -> Sheet {
    let listed = models
        .providers
        .iter()
        .map(|provider| provider.models.len())
        .sum::<usize>();
    let mut rows =
        Vec::with_capacity(listed + 2 * models.providers.len() + models.recent.len() + 2);
    if !models.recent.is_empty() {
        rows.push(group(RECENT, ""));
        rows.extend(models.recent.iter().map(|id| {
            let found = models
                .providers
                .iter()
                .flat_map(|provider| &provider.models)
                .find(|model| model.id == *id);
            model_row(id, found, current == Some(&**id))
        }));
        rows.push(blank());
    }
    for (at, provider) in models.providers.iter().enumerate() {
        if at > 0 {
            rows.push(blank());
        }
        rows.push(provider_row(provider));
        rows.extend(
            provider
                .models
                .iter()
                .map(|model| model_row(&model.id, Some(model), current == Some(&*model.id))),
        );
    }
    let count = format!(
        "{listed} models \u{b7} {} providers",
        models.providers.len()
    );
    Sheet::new(TITLE, Arc::from(count), HINT, FOOTER, rows)
}

/// The model id of the selected row, for the `set` request.
pub(crate) fn picked(sheet: &Sheet) -> Option<Arc<str>> {
    let (_, row) = sheet.selected()?;
    match row.kind {
        Kind::Item => Some(Arc::clone(&row.name)),
        _ => None,
    }
}

/// The row of one model, with `now` and the current mark when it is the model
/// the session runs.
fn model_row(id: &Arc<str>, model: Option<&Model>, current: bool) -> Row {
    Row {
        kind: Kind::Item,
        mark: if current { Mark::Current } else { Mark::None },
        name: Arc::clone(id),
        detail: detail(model),
        right: Arc::from(if current { NOW } else { "" }),
        dim: false,
    }
}

/// The dim group row of a provider: its name and how the run is authenticated.
fn provider_row(provider: &Provider) -> Row {
    group(&provider.name, &format!("\u{b7} {}", provider.note))
}

/// The dim header row of `name` with `detail` after it.
fn group(name: &str, detail: &str) -> Row {
    Row {
        kind: Kind::Group,
        mark: Mark::None,
        name: Arc::from(name),
        detail: Arc::from(detail),
        right: Arc::from(""),
        dim: false,
    }
}

/// An empty row between the groups.
fn blank() -> Row {
    Row {
        kind: Kind::Blank,
        mark: Mark::None,
        name: Arc::from(""),
        detail: Arc::from(""),
        right: Arc::from(""),
        dim: false,
    }
}

/// A model's window in its columns and its prices after two columns, both left
/// out when unknown.
fn detail(model: Option<&Model>) -> Arc<str> {
    let mut text = String::new();
    if let Some(window) = model.and_then(|model| model.window) {
        let value = window_text(window);
        text.extend(std::iter::repeat_n(' ', WINDOW.saturating_sub(value.len())));
        text.push_str(&value);
    }
    if let Some((input, output)) = model.and_then(|model| model.prices) {
        if !text.is_empty() {
            text.push_str("  ");
        }
        let _ = write!(text, "${} / {}", price(input), price(output));
    }
    Arc::from(text)
}

/// A window in tokens as `200k` or `1M`, with one decimal when it is not a
/// whole unit.
fn window_text(window: u64) -> String {
    let (unit, per) = if window >= 1_000_000 {
        ("M", 1_000_000)
    } else {
        ("k", 1_000)
    };
    let whole = window / per;
    let tenth = window % per / (per / 10);
    if tenth == 0 {
        format!("{whole}{unit}")
    } else {
        format!("{whole}.{tenth}{unit}")
    }
}

/// One price in USD per million tokens: two decimals unless it is a whole
/// dollar amount.
fn price(price: f64) -> String {
    if price.fract() == 0.0 {
        format!("{}", price as u64)
    } else {
        format!("{price:.2}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::host::{Model, Models, Provider};

    use super::super::{Kind, Mark, Sheet};
    use super::{model_sheet, picked};

    /// A model with a window and prices, either of which may be unknown.
    fn model(id: &str, window: Option<u64>, prices: Option<(f64, f64)>) -> Model {
        Model {
            id: Arc::from(id),
            window,
            prices,
            reasoning: None,
        }
    }

    /// The models of the tests: one recent id, then two providers.
    fn models() -> Models {
        Models {
            recent: vec![Arc::from("deepseek/flash")],
            providers: vec![
                Provider {
                    name: Arc::from("anthropic"),
                    note: Arc::from("logged in"),
                    models: vec![
                        model("anthropic/opus", Some(1_000_000), Some((15.0, 75.0))),
                        model("anthropic/haiku", Some(200_000), Some((1.0, 5.0))),
                    ],
                },
                Provider {
                    name: Arc::from("openrouter"),
                    note: Arc::from("key from config"),
                    models: vec![model(
                        "openrouter/deepseek",
                        Some(256_000),
                        Some((0.3, 1.1)),
                    )],
                },
            ],
        }
    }

    /// The names of the rows, top down.
    fn names(sheet: &Sheet) -> Vec<&str> {
        sheet.rows.iter().map(|row| &*row.name).collect()
    }

    /// The kinds of the rows, top down.
    fn kinds(sheet: &Sheet) -> Vec<Kind> {
        sheet.rows.iter().map(|row| row.kind).collect()
    }

    /// The recent group comes first, then a blank and one group per provider
    /// with its models under it.
    #[test]
    fn the_recent_group_then_a_group_per_provider_hold_the_items_in_order() {
        let sheet = model_sheet(&models(), Some("anthropic/opus"));
        assert_eq!(
            names(&sheet),
            [
                "recent",
                "deepseek/flash",
                "",
                "anthropic",
                "anthropic/opus",
                "anthropic/haiku",
                "",
                "openrouter",
                "openrouter/deepseek",
            ]
        );
        assert_eq!(
            kinds(&sheet),
            [
                Kind::Group,
                Kind::Item,
                Kind::Blank,
                Kind::Group,
                Kind::Item,
                Kind::Item,
                Kind::Blank,
                Kind::Group,
                Kind::Item,
            ]
        );
        assert_eq!(&*sheet.rows[3].detail, "\u{b7} logged in");
        assert_eq!(&*sheet.rows[7].detail, "\u{b7} key from config");
        assert_eq!(sheet.title, "Model");
        assert_eq!(&*sheet.count, "3 models \u{b7} 2 providers");
        assert_eq!(sheet.hint, "context \u{b7} $ in/out per M");
        assert_eq!(
            sheet.footer,
            "\u{2191}\u{2193} choose \u{b7} enter switches the next turn \u{b7} esc back to your draft"
        );
    }

    /// The model the session runs carries the current mark and `now` on its
    /// right, and no other row does.
    #[test]
    fn the_current_model_is_marked_and_says_now() {
        let sheet = model_sheet(&models(), Some("anthropic/opus"));
        assert_eq!(sheet.rows[4].mark, Mark::Current);
        assert_eq!(&*sheet.rows[4].right, "now");
        assert_eq!(sheet.rows[1].mark, Mark::None);
        assert_eq!(&*sheet.rows[1].right, "");

        // No current model: no row is marked.
        let none = model_sheet(&models(), None);
        assert!(none.rows.iter().all(|row| row.mark == Mark::None));
        assert!(none.rows.iter().all(|row| row.right.is_empty()));
    }

    /// A model's window is six columns wide and its prices follow two columns
    /// later, with no trailing zeros on a whole price.
    #[test]
    fn the_window_is_six_columns_and_the_prices_follow() {
        let sheet = model_sheet(&models(), None);
        assert_eq!(&*sheet.rows[4].detail, "    1M  $15 / 75");
        assert_eq!(&*sheet.rows[5].detail, "  200k  $1 / 5");
        assert_eq!(&*sheet.rows[8].detail, "  256k  $0.30 / 1.10");

        // A recent id the provider lists hold takes their window and prices,
        // and one they do not hold has neither.
        let mut listed = models();
        listed.recent = vec![Arc::from("anthropic/haiku")];
        let sheet = model_sheet(&listed, None);
        assert_eq!(&*sheet.rows[1].detail, "  200k  $1 / 5");

        let sheet = model_sheet(&models(), None);
        assert_eq!(&*sheet.rows[1].detail, "");
    }

    /// A model with no window and no price leaves the detail empty.
    #[test]
    fn an_unknown_window_and_price_leave_the_detail_empty() {
        let models = Models {
            recent: Vec::new(),
            providers: vec![Provider {
                name: Arc::from("local"),
                note: Arc::from("no login"),
                models: vec![model("local/tiny", None, None)],
            }],
        };
        let sheet = model_sheet(&models, None);
        assert_eq!(&*sheet.rows[1].detail, "");
    }

    /// A step moves the pick down the items and `picked` answers the model of
    /// the row it lands on, never a group, a blank or a note.
    #[test]
    fn picked_answers_the_model_after_a_step() {
        let mut sheet = model_sheet(&models(), Some("anthropic/opus"));
        assert_eq!(picked(&sheet).as_deref(), Some("deepseek/flash"));
        sheet.step(1);
        assert_eq!(picked(&sheet).as_deref(), Some("anthropic/opus"));
        sheet.step(1);
        assert_eq!(picked(&sheet).as_deref(), Some("anthropic/haiku"));
        sheet.step(1);
        assert_eq!(picked(&sheet).as_deref(), Some("openrouter/deepseek"));
        // The last item is the end of the list.
        sheet.step(1);
        assert_eq!(picked(&sheet).as_deref(), Some("openrouter/deepseek"));
    }

    /// With no recent model the sheet opens on the first provider.
    #[test]
    fn no_recent_model_means_no_recent_group() {
        let mut models = models();
        models.recent.clear();
        let sheet = model_sheet(&models, None);
        assert_eq!(names(&sheet)[0], "anthropic");
        assert_eq!(kinds(&sheet)[0], Kind::Group);
        assert_eq!(&*sheet.count, "3 models \u{b7} 2 providers");
        assert_eq!(picked(&sheet).as_deref(), Some("anthropic/opus"));
    }

    /// A file that moves nothing prints each word as the table has it.
    #[test]
    fn keys_in_force_model_foot_matches_the_table() {
        let (file, _) = crate::keyfile::KeyFile::parse("");
        let labels = file.labels();
        assert_eq!(labels.foot(crate::keyfile::Foot::Model), super::FOOTER);
    }
}
