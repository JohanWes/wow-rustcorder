// SPDX-License-Identifier: GPL-3.0-or-later

//! Single-view viewpoint selection logic, GTK-free: which local POVs a
//! correlated activity offers, how they are labelled, and which one to open.
//! The player shows one viewpoint at a time.

use warcraft_rustcorder::domain::{LibraryEntry, RecordingId};

use super::filters::spec_name;
use super::timeline::plain_name;

/// One selectable local viewpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pov {
    pub id: RecordingId,
    /// "Player (Spec)" when known, else the entry title.
    pub label: String,
    /// The plain player name, the key `preferredViewpoint` remembers.
    pub player: Option<String>,
}

/// Distinct local POVs for a correlated activity: primary first, then the
/// correlated order, deduplicated by player label.
pub fn povs(entries: &[&LibraryEntry]) -> Vec<Pov> {
    let mut out: Vec<Pov> = Vec::new();
    for entry in entries {
        let player = entry
            .player
            .as_ref()
            .filter(|player| !player.name.is_empty())
            .map(|player| plain_name(&player.name).to_owned());
        let label = match (&player, entry.player.as_ref().and_then(|p| p.spec_id)) {
            (Some(name), Some(spec)) => match spec_name(spec) {
                Some(spec) => format!("{name} ({spec})"),
                None => name.clone(),
            },
            (Some(name), None) => name.clone(),
            _ => entry.title.clone(),
        };
        if out.iter().any(|existing| existing.label == label) {
            continue;
        }
        out.push(Pov {
            id: entry.id.clone(),
            label,
            player,
        });
    }
    out
}

/// The POV to open for a selection: the remembered preferred player when it
/// exists among these POVs, else the first (the primary/default).
pub fn choose<'a>(povs: &'a [Pov], preferred_player: Option<&str>) -> Option<&'a Pov> {
    preferred_player
        .and_then(|name| povs.iter().find(|pov| pov.player.as_deref() == Some(name)))
        .or_else(|| povs.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use warcraft_rustcorder::domain::{Category, PlayerSummary};

    fn entry(player: Option<(&str, u16)>, title: &str) -> LibraryEntry {
        let mut entry = crate::ui::window::tests::entry(Category::Raids, title, 0);
        entry.player = player.map(|(name, spec)| PlayerSummary {
            name: name.to_owned(),
            realm: None,
            guid: None,
            spec_id: Some(spec),
        });
        entry
    }

    #[test]
    fn povs_are_labelled_deduplicated_and_preferred_selection_wins() {
        let alice = entry(Some(("Alice-Realm", 64)), "a");
        let bob = entry(Some(("Bob-Realm", 577)), "b");
        let alice_again = entry(Some(("Alice-Other", 64)), "c");
        let unnamed = entry(None, "Manual title");
        let list = povs(&[&alice, &bob, &alice_again, &unnamed]);
        let labels: Vec<&str> = list.iter().map(|pov| pov.label.as_str()).collect();
        assert_eq!(labels, vec!["Alice (Frost)", "Bob (Havoc)", "Manual title"]);

        // Preferred player is remembered by plain name; unknown falls back to
        // the first POV.
        assert_eq!(choose(&list, Some("Bob")).unwrap().label, "Bob (Havoc)");
        assert_eq!(choose(&list, Some("Carol")).unwrap().label, "Alice (Frost)");
        assert_eq!(choose(&list, None).unwrap().label, "Alice (Frost)");
    }
}
