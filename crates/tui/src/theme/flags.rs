//! `NO_COLOR` and `NO_MOTION`, read once from the environment.

/// What the environment asks the UI to leave out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Flags {
    /// Draw color; false under `NO_COLOR`.
    pub color: bool,
    /// Draw the moving bar and the animations; false under `NO_MOTION`.
    pub motion: bool,
}

impl Flags {
    /// The flags a pair of environment values asks for. The convention of
    /// <https://no-color.org> is that a variable set to an empty string is
    /// unset, so `NO_COLOR=` still draws color.
    pub const fn read(no_color: Option<&str>, no_motion: Option<&str>) -> Flags {
        Flags {
            color: !set(no_color),
            motion: !set(no_motion),
        }
    }

    /// The flags the process environment asks for.
    pub fn from_env() -> Flags {
        let read = |name| std::env::var(name).ok();
        Flags::read(read("NO_COLOR").as_deref(), read("NO_MOTION").as_deref())
    }
}

/// True for a variable that is set to something, which is the convention of
/// <https://no-color.org>: `NO_COLOR=` asks for color.
const fn set(value: Option<&str>) -> bool {
    match value {
        Some(text) => !text.is_empty(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_and_no_motion_read_their_values() {
        assert_eq!(
            Flags::read(None, None),
            Flags {
                color: true,
                motion: true
            }
        );
        assert_eq!(
            Flags::read(Some("1"), Some("1")),
            Flags {
                color: false,
                motion: false
            }
        );
        assert_eq!(
            Flags::read(Some(""), Some("")),
            Flags {
                color: true,
                motion: true
            }
        );
        assert_eq!(
            Flags::read(Some("1"), None),
            Flags {
                color: false,
                motion: true
            }
        );
    }
}
