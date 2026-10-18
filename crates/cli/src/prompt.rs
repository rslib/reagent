//! The system prompt template for a headless run.

use reagent_ai::Text;

/// The prompt template, baked in at build time from `prompts/system.md`.
const TEMPLATE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../prompts/system.md"
));

/// Builds the system prompt for a run rooted at `cwd`, filling the template.
pub fn system(cwd: &str) -> Text {
    Text::from(TEMPLATE.replace("{cwd}", cwd))
}

/// A rough token estimate: bytes divided by 4.
///
/// The budget test uses this so a change to the prompt or the tool specs
/// cannot silently grow the per-turn cost.
#[cfg(test)]
pub fn estimate_tokens(bytes: usize) -> usize {
    bytes / 4
}
