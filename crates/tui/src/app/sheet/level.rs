//! The level sheet.
//!
//! The rows are built once, when the sheet opens: `off` when the model can stop
//! thinking, then the levels of its plan lowest first, then the words the plan
//! lists that reagent does not know, as written. The pick answers the word the
//! `set` request carries.

use std::sync::Arc;

use reagent_ai::levels::{Level, Reasoning};

use super::{Kind, Mark, Row, Sheet};

/// The word of the title row.
const TITLE: &str = "Level";

/// The hint at the right edge of the title row.
const HINT: &str = "";

/// The keys of the footer row, until a keys file moves one.
const FOOTER: &str =
    "\u{2191}\u{2193} choose \u{b7} enter applies from the next turn \u{b7} esc back to your draft";

/// The word that turns thinking off.
const OFF: &str = "off";

/// The text at the right edge of the row of the level in use.
const NOW: &str = "now";

/// The detail of the level the model starts at.
const DEFAULT: &str = "default";

/// The rows of a model whose plan says nothing.
const PLAIN: [Level; 3] = [Level::Low, Level::Medium, Level::High];

/// The level sheet for `plan`, the word `current` marked as the level in use.
///
/// The selection starts on the current row, and on the first one when no row
/// is the current level.
pub(crate) fn level_sheet(plan: Option<&Reasoning>, current: Option<&str>) -> Sheet {
    let mut rows = Vec::with_capacity(8);
    match plan.filter(|plan| !plan.is_empty()) {
        None => rows.extend(PLAIN.iter().map(|level| row(level.name(), false))),
        Some(plan) => {
            if plan.off == Some(true) && !plan.mandatory {
                rows.push(row(OFF, false));
            }
            rows.extend(
                plan.levels
                    .iter()
                    .map(|level| row(level.name(), plan.default == Some(level))),
            );
            rows.extend(plan.unknown.iter().map(|word| row(word, false)));
        }
    }
    // `none` and a zero budget turn thinking off too, and draw on the off row.
    let current = match current {
        Some(word) if reagent_ai::Reasoning::parse(word).is_off() => OFF,
        Some(word) => word,
        None => "",
    };
    for row in &mut rows {
        if row.name.eq_ignore_ascii_case(current) {
            row.mark = Mark::Current;
            row.right = Arc::from(NOW);
        }
    }
    let count = format!("{} levels", rows.len());
    let mut sheet = Sheet::new(TITLE, Arc::from(count), HINT, FOOTER, rows);
    if !current.is_empty() {
        sheet.select(current);
    }
    sheet
}

/// The word of the selected row, for the `set` request.
pub(crate) fn picked(sheet: &Sheet) -> Option<Arc<str>> {
    let (_, row) = sheet.selected()?;
    match row.kind {
        Kind::Item => Some(Arc::clone(&row.name)),
        _ => None,
    }
}

/// The row of one word, with `default` after the name; the opening mark is left
/// to the current row.
fn row(word: &str, default: bool) -> Row {
    Row {
        kind: Kind::Item,
        mark: Mark::None,
        name: Arc::from(word),
        detail: Arc::from(if default { DEFAULT } else { "" }),
        right: Arc::from(""),
        dim: false,
    }
}

#[cfg(test)]
mod tests {
    use reagent_ai::levels::{Level, Levels, Reasoning};

    use super::{FOOTER, HINT, TITLE, level_sheet, picked};
    use crate::app::sheet::{Mark, Sheet};

    /// A plan of `levels`, with the words of the source.
    fn plan(
        levels: &[Level],
        off: Option<bool>,
        mandatory: bool,
        default: Option<Level>,
        unknown: &[&str],
    ) -> Reasoning {
        let mut set = Levels::default();
        for level in levels {
            set.insert(*level);
        }
        Reasoning {
            levels: set,
            off,
            mandatory,
            default,
            unknown: unknown.iter().map(|word| (*word).to_string()).collect(),
            never: false,
        }
    }

    /// The item rows of `sheet`, as their name, mark, detail and right text.
    fn items(sheet: &Sheet) -> Vec<(&str, Mark, &str, &str)> {
        sheet
            .rows
            .iter()
            .filter(|row| row.kind == crate::app::sheet::Kind::Item)
            .map(|row| (&*row.name, row.mark, &*row.detail, &*row.right))
            .collect()
    }

    /// The name of the selected row of `sheet`.
    fn selected(sheet: &Sheet) -> Option<&str> {
        sheet.selected().map(|(_, row)| &*row.name)
    }

    #[test]
    fn level_sheet_lists_off_the_levels_and_the_default_of_a_known_plan() {
        let plan = plan(
            &[Level::Low, Level::High],
            Some(true),
            false,
            Some(Level::High),
            &[],
        );
        let sheet = level_sheet(Some(&plan), Some("low"));
        assert_eq!(
            items(&sheet),
            [
                ("off", Mark::None, "", ""),
                ("low", Mark::Current, "", "now"),
                ("high", Mark::None, "default", ""),
            ]
        );
        assert_eq!(selected(&sheet), Some("low"));
        assert_eq!(&*sheet.count, "3 levels");
        assert_eq!(sheet.title, TITLE);
        assert_eq!(sheet.hint, HINT);
        assert_eq!(sheet.footer, FOOTER);
    }

    #[test]
    fn level_sheet_without_a_plan_lists_the_three_usual_levels() {
        for plan in [None, Some(plan(&[], None, false, None, &[]))] {
            let sheet = level_sheet(plan.as_ref(), None);
            assert_eq!(
                items(&sheet),
                [
                    ("low", Mark::None, "", ""),
                    ("medium", Mark::None, "", ""),
                    ("high", Mark::None, "", ""),
                ]
            );
            assert_eq!(selected(&sheet), Some("low"), "the first row");
            assert_eq!(&*sheet.count, "3 levels");
        }
        let named = level_sheet(None, Some("HIGH"));
        assert_eq!(selected(&named), Some("high"), "the current row");
        assert_eq!(items(&named)[2].1, Mark::Current);
        assert_eq!(items(&named)[2].3, "now");
    }

    #[test]
    fn level_sheet_hides_off_when_thinking_is_mandatory() {
        let mandatory = plan(&[Level::Low, Level::High], Some(true), true, None, &[]);
        let sheet = level_sheet(Some(&mandatory), None);
        assert_eq!(
            items(&sheet).iter().map(|row| row.0).collect::<Vec<_>>(),
            ["low", "high"]
        );
        let kept = plan(&[Level::Low], Some(false), false, None, &[]);
        assert_eq!(
            items(&level_sheet(Some(&kept), None))
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            ["low"],
            "thinking that cannot be turned off has no row"
        );
    }

    #[test]
    fn level_sheet_lists_the_unknown_words_after_the_levels() {
        let plan = plan(&[Level::Low], Some(true), false, None, &["ultra", "turbo"]);
        let sheet = level_sheet(Some(&plan), Some("turbo"));
        assert_eq!(
            items(&sheet).iter().map(|row| row.0).collect::<Vec<_>>(),
            ["off", "low", "ultra", "turbo"]
        );
        assert_eq!(selected(&sheet), Some("turbo"));
        assert_eq!(items(&sheet)[3].1, Mark::Current);
    }

    #[test]
    fn level_sheet_marks_the_off_row_for_any_off_word() {
        let plan = plan(&[Level::Low], Some(true), false, None, &[]);
        let sheet = level_sheet(Some(&plan), Some("none"));
        assert_eq!(selected(&sheet), Some("off"));
        assert_eq!(items(&sheet)[0].1, Mark::Current);
    }

    #[test]
    fn level_sheet_picked_answers_the_word_of_the_selected_row() {
        let plan = plan(&[Level::Low, Level::High], Some(true), false, None, &[]);
        let mut sheet = level_sheet(Some(&plan), Some("low"));
        assert_eq!(picked(&sheet).as_deref(), Some("low"));
        sheet.step(1);
        assert_eq!(picked(&sheet).as_deref(), Some("high"));
        sheet.step(-2);
        assert_eq!(picked(&sheet).as_deref(), Some("off"));
    }

    /// A file that moves nothing prints each word as the table has it.
    #[test]
    fn keys_in_force_level_foot_matches_the_table() {
        let (file, _) = crate::keyfile::KeyFile::parse("");
        let labels = file.labels();
        assert_eq!(labels.foot(crate::keyfile::Foot::Level), super::FOOTER);
    }
}
