//! The people a gateway may forward a parked question to, from
//! `contacts.toml` in the global config folder.
//!
//! Only the owner edits that file: a project folder never holds one, so a
//! repository cannot add a person who may answer.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// One person a parked question may be forwarded to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    /// The name the model reads; the key of the table when not set.
    #[serde(default)]
    pub name: Option<String>,
    /// The gateway plugin that reaches this person, as its manifest names it.
    pub channel: String,
    /// The plugin's own id of this person, such as a chat id.
    pub user: String,
}

/// The file name of the contacts, in the global config folder.
pub const FILE: &str = "contacts.toml";

/// The contacts of this machine by key, or none when the file is absent.
///
/// Reading blocks, so a caller on a runtime runs this in `spawn_blocking`.
pub fn load() -> Result<BTreeMap<String, Contact>, String> {
    let Some(dir) = crate::global_dir() else {
        return Ok(BTreeMap::new());
    };
    load_from(&dir.join(FILE))
}

/// The contacts of the file at `path`, or none when it is absent.
pub fn load_from(path: &Path) -> Result<BTreeMap<String, Contact>, String> {
    let Some(text) = crate::read_text(path)? else {
        return Ok(BTreeMap::new());
    };
    let contacts: BTreeMap<String, Contact> =
        toml::from_str(&text.text).map_err(|error| crate::parse_error(path, &text.text, &error))?;
    for (key, contact) in &contacts {
        if contact.channel.is_empty() || contact.user.is_empty() {
            return Err(format!(
                "{}: contact {key} needs a channel and a user",
                path.display()
            ));
        }
    }
    Ok(contacts)
}

impl Contact {
    /// The name the model reads for the contact of key `key`.
    pub fn shown<'a>(&'a self, key: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A contacts file reads by key, an absent one is empty, and an unknown
    /// field or an empty channel is an error that names the file.
    #[test]
    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn contacts_read_by_key_and_refuse_a_wrong_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(FILE);
        assert_eq!(load_from(&path), Ok(BTreeMap::new()));

        std::fs::write(
            &path,
            "[bob]\nname = \"Bob\"\nchannel = \"telegram\"\nuser = \"42\"\n\n[carol]\nchannel = \"mail\"\nuser = \"carol@example.org\"\n",
        )
        .expect("write");
        let contacts = load_from(&path).expect("read");
        assert_eq!(contacts["bob"].shown("bob"), "Bob");
        assert_eq!(contacts["carol"].shown("carol"), "carol");
        assert_eq!(contacts["bob"].channel, "telegram");

        std::fs::write(
            &path,
            "[bob]\nchannel = \"telegram\"\nuser = \"42\"\nrole = \"x\"\n",
        )
        .expect("write");
        let error = load_from(&path).expect_err("an unknown field");
        assert!(error.contains(FILE), "{error}");

        std::fs::write(&path, "[bob]\nchannel = \"\"\nuser = \"42\"\n").expect("write");
        let error = load_from(&path).expect_err("an empty channel");
        assert!(error.contains("contact bob"), "{error}");
    }
}
