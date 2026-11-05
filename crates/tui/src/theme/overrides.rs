//! The TOML theme file: a role's color, or `none` for the terminal's own.

use super::{Rgb, Role};

/// The roles a theme file sets. A role that is absent keeps the color its
/// mode and background give it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Overrides {
    colors: [Option<Option<Rgb>>; Role::ALL.len()],
}

impl Overrides {
    /// Read a theme file. The shape is one `[roles]` table of role names:
    ///
    /// ```toml
    /// [roles]
    /// accent = "#4fc1c9"   # the reagent hue
    /// warn = "none"        # the terminal's own color
    /// ```
    ///
    /// An unknown role, a bad color or a line that is neither is an error that
    /// names what it did not understand.
    pub fn parse(text: &str) -> Result<Overrides, Error> {
        let mut overrides = Overrides::default();
        let mut in_roles = false;
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                if name.trim() != "roles" || in_roles {
                    return Err(Error::Syntax {
                        line: number,
                        text: line.to_string(),
                    });
                }
                in_roles = true;
                continue;
            }
            let Some((name, value)) = line.split_once('=') else {
                return Err(Error::Syntax {
                    line: number,
                    text: line.to_string(),
                });
            };
            let name = name.trim();
            if !in_roles {
                return Err(Error::Syntax {
                    line: number,
                    text: line.to_string(),
                });
            }
            let role = Role::from_name(name).ok_or_else(|| Error::Role(name.to_string()))?;
            let value = unquote(value.trim());
            let color = if value == "none" {
                None
            } else {
                let rgb = Rgb::parse(value).ok_or_else(|| Error::Color {
                    role: name.to_string(),
                    value: value.to_string(),
                })?;
                Some(rgb)
            };
            overrides.colors[role.index()] = Some(color);
        }
        Ok(overrides)
    }

    /// What the file says for a role: `None` when it says nothing, `Some(None)`
    /// for `none`, and `Some(Some(color))` for a color.
    pub fn get(&self, role: Role) -> Option<Option<Rgb>> {
        self.colors[role.index()]
    }
}

/// Why a theme file was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// A role name no role has.
    Role(String),
    /// A role set to text that is neither a color nor `none`.
    Color {
        /// The role, as the file named it.
        role: String,
        /// The text it was set to.
        value: String,
    },
    /// A line that is not a table header or `name = "value"`.
    Syntax {
        /// The line number, from 1.
        line: usize,
        /// The line's text.
        text: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Role(name) => {
                write!(f, "theme: no role named \"{name}\"; the roles are ")?;
                for (index, role) in Role::ALL.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(role.name())?;
                }
                Ok(())
            }
            Error::Color { role, value } => write!(
                f,
                "theme: role \"{role}\" takes a color like \"#4fc1c9\" or \"none\", not \"{value}\""
            ),
            Error::Syntax { line, text } => {
                write!(
                    f,
                    "theme: line {line} is not a [roles] header or a role, not \"{text}\""
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// `line` without its comment. A `#` inside a quoted value starts no comment,
/// so the color `"#4fc1c9"` stays whole.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match character {
            '"' | '\'' => {
                if quote == Some(character) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(character);
                }
            }
            '#' if quote.is_none() => return &line[..index],
            _ => {}
        }
    }
    line
}

/// A value without its quotes; a value with no quotes is taken as it is, so a
/// missing pair still reaches the color check that names it.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_override_reads_a_color_and_none() {
        let overrides =
            Overrides::parse("[roles]\naccent = \"#4fc1c9\"\nwarn = \"none\"\n").expect("parses");
        assert_eq!(
            overrides.get(Role::Accent),
            Some(Some(Rgb::new(0x4f, 0xc1, 0xc9)))
        );
        assert_eq!(overrides.get(Role::Warn), Some(None));
        assert_eq!(overrides.get(Role::Text), None);
    }

    #[test]
    fn an_unknown_role_is_an_error_naming_it() {
        let error = Overrides::parse("[roles]\nblink = \"#ffffff\"\n").expect_err("refused");
        assert!(error.to_string().contains("blink"), "{error}");
    }

    #[test]
    fn a_bad_color_is_an_error_naming_it() {
        let error = Overrides::parse("[roles]\naccent = \"teal\"\n").expect_err("refused");
        let text = error.to_string();
        assert!(text.contains("accent"), "{text}");
        assert!(text.contains("teal"), "{text}");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let overrides = Overrides::parse(
            "# a theme\n\n[roles]\n\n# the text box\ninput-fill = \"#101010\" # the box\n",
        )
        .expect("parses");
        assert_eq!(
            overrides.get(Role::InputFill),
            Some(Some(Rgb::new(0x10, 0x10, 0x10)))
        );
    }
}
