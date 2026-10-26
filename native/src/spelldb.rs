// SPDX-License-Identifier: GPL-3.0-or-later

//! The bundled spell database powering damage-meter icons and tooltips.
//!
//! `data/spells/spells.json` is a name-keyed map produced by
//! `scripts/fetch-spell-data.py`:
//!
//! ```json
//! { "Fireball": ["Throws a fiery ball...", "spell_fire_flamebolt"], ... }
//! ```
//!
//! Each value is `[description, icon_basename]`. Rank variants fold onto
//! their base spell's entry, so a lookup by the name a combat log reports
//! resolves to the right icon and tooltip. Spells without a bundled entry
//! simply render without an icon or tooltip. No file I/O here: the caller
//! looks up the resource and hands its JSON text to [`SpellDb::parse`].

use std::borrow::{Borrow, Cow};
use std::collections::HashMap;

use serde::Deserialize;

/// One spell's tooltip facts, borrowed from the bundled JSON. Strings with
/// JSON escapes cannot be borrowed and are owned instead, hence `Cow`.
#[derive(Clone, Debug, Deserialize)]
pub struct SpellInfo {
    #[serde(borrow)]
    pub description: Cow<'static, str>,
    /// Icon basename, e.g. `spell_fire_flamebolt`; the PNG lives at
    /// `/io/github/JohanWes/WarcraftRustcorder/spells/{icon}.png`.
    pub icon: &'static str,
}

/// A spell-name map key; a newtype only so serde borrows it.
#[derive(Debug, PartialEq, Eq, Hash, Deserialize)]
struct Name(#[serde(borrow)] Cow<'static, str>);

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// An immutable spell-name index built once from the bundled JSON.
#[derive(Debug)]
pub struct SpellDb {
    by_name: HashMap<Name, SpellInfo>,
}

impl SpellDb {
    /// Index the bundled spell JSON (`{name: [description, icon]}`) without
    /// copying its strings, so the text must live as long as the process,
    /// as the registered resource bundle does.
    pub fn parse(json: &'static str) -> Result<Self, serde_json::Error> {
        Ok(Self {
            by_name: serde_json::from_str(json)?,
        })
    }

    /// The entry for `name`, if the database knows it.
    pub fn lookup(&self, name: &str) -> Option<&SpellInfo> {
        self.by_name.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real bundled database must parse and contain the well-known spells.
    #[test]
    fn bundled_database_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("native/ has a parent")
            .join("data/spells/spells.json");
        let json = std::fs::read_to_string(path).expect("read bundled spells.json");
        let db =
            SpellDb::parse(Box::leak(json.into_boxed_str())).expect("bundled spells.json parses");
        assert!(db.lookup("Fireball").is_some());
        assert!(db.lookup("Flash Heal").is_some());
        // These current player abilities use inventory-prefixed icon files;
        // keep them as regressions against filtering by icon basename.
        assert!(db.lookup("Voltaic Blaze").is_some());
        assert!(db.lookup("Deathstalker's Mark").is_some());
        assert!(db.lookup("Goremaw's Bite").is_some());
        for encounter_spell in [
            "Fel Steps",
            "Ferocious Leap",
            "Lightbloom Lashing",
            "Sappy Demise",
            "Savage Smash",
            "Umbral Rupture",
        ] {
            assert!(
                !db.lookup(encounter_spell)
                    .expect("current encounter spell is indexed")
                    .description
                    .is_empty(),
                "{encounter_spell} has tooltip text"
            );
        }
    }
}
