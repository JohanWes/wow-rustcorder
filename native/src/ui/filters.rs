// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure suggestion-chip and date-range filtering. No GTK here: this module is
//! table-driven and unit tested on its own.
//!
//! A chip is a numeric grouping plus its label; icon and colour are a
//! deterministic function of the grouping. A correlated row passes only when
//! every selected chip occurs in the row's primary entry or one of its
//! correlated POVs (AND), and, when both endpoints exist, its start falls
//! inside the inclusive date range.

use std::collections::BTreeSet;

use warcraft_rustcorder::domain::{ActivityDetails, GameFlavor, LibraryEntry, Outcome};

// Chip groupings. Distinct groupings keep otherwise-equal labels
// (e.g. a "Frost" spec vs a hypothetical zone) from colliding in matching.
const GROUP_PROTECTION: u16 = 101;
const GROUP_TAGGED: u16 = 102;
const GROUP_FLAVOUR: u16 = 103;
const GROUP_NAME: u16 = 200;
const GROUP_SPEC: u16 = 201;
const GROUP_ZONE: u16 = 202;
const GROUP_DUNGEON: u16 = 203;
const GROUP_AFFIX: u16 = 204;
const GROUP_ENCOUNTER: u16 = 205;
const GROUP_RESULT: u16 = 50;
const GROUP_DIFFICULTY_LFR: u16 = 51;
const GROUP_DIFFICULTY_NORMAL: u16 = 52;
const GROUP_DIFFICULTY_HEROIC: u16 = 53;
const GROUP_DIFFICULTY_MYTHIC: u16 = 54;

/// One selectable/encoded search suggestion. `Ord` gives the suggestion list a
/// stable display order (grouping, then label).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chip {
    pub group: u16,
    pub label: String,
}

impl Chip {
    fn new(group: u16, label: impl Into<String>) -> Self {
        Self {
            group,
            label: label.into(),
        }
    }

    /// A stock symbolic icon for the chip's grouping. Game-specific art is not
    /// redistributable, so chips are icon+text.
    pub fn icon_name(&self) -> &'static str {
        match self.group {
            GROUP_PROTECTION => "starred-symbolic",
            GROUP_TAGGED => "tag-symbolic",
            GROUP_FLAVOUR => "applications-games-symbolic",
            GROUP_NAME => "avatar-default-symbolic",
            GROUP_SPEC => "user-info-symbolic",
            GROUP_ZONE | GROUP_DUNGEON => "mark-location-symbolic",
            GROUP_AFFIX => "dialog-warning-symbolic",
            GROUP_ENCOUNTER => "view-list-symbolic",
            GROUP_RESULT => "emblem-ok-symbolic",
            GROUP_DIFFICULTY_LFR
            | GROUP_DIFFICULTY_NORMAL
            | GROUP_DIFFICULTY_HEROIC
            | GROUP_DIFFICULTY_MYTHIC => "security-high-symbolic",
            _ => "view-more-symbolic",
        }
    }

    /// A per-grouping tint class for the chip pill.
    pub fn css_class(&self) -> &'static str {
        match self.group {
            GROUP_NAME => "wr-chip-name",
            GROUP_SPEC => "wr-chip-spec",
            GROUP_ZONE | GROUP_DUNGEON | GROUP_ENCOUNTER => "wr-chip-place",
            GROUP_AFFIX => "wr-chip-affix",
            GROUP_RESULT => "wr-chip-result",
            GROUP_DIFFICULTY_LFR..=GROUP_DIFFICULTY_MYTHIC => "wr-chip-difficulty",
            _ => "wr-chip-misc",
        }
    }
}

/// Spec id → the owning class's CSS class, for class-colored player names.
pub fn class_css_class(spec_id: u16) -> Option<&'static str> {
    Some(match spec_id {
        250..=252 => "wr-class-death-knight",
        577 | 581 | 1480 => "wr-class-demon-hunter",
        102..=105 => "wr-class-druid",
        1467 | 1468 | 1473 => "wr-class-evoker",
        253..=255 => "wr-class-hunter",
        62..=64 => "wr-class-mage",
        268..=270 => "wr-class-monk",
        65 | 66 | 70 => "wr-class-paladin",
        256..=258 => "wr-class-priest",
        259..=261 => "wr-class-rogue",
        262..=264 => "wr-class-shaman",
        265..=267 => "wr-class-warlock",
        71..=73 => "wr-class-warrior",
        _ => return None,
    })
}

/// Every suggestion the entry contributes, mirroring
/// `VideoFilter.getVideoSuggestions`.
pub fn suggestions_for_entry(entry: &LibraryEntry) -> Vec<Chip> {
    let mut chips = Vec::new();

    // Generic suggestions (category independent).
    chips.push(Chip::new(
        GROUP_PROTECTION,
        if entry.protected {
            "Starred"
        } else {
            "Not Starred"
        },
    ));
    if entry.tag.is_some() {
        chips.push(Chip::new(GROUP_TAGGED, "Tagged"));
    }
    match entry.flavor {
        GameFlavor::Retail => chips.push(Chip::new(GROUP_FLAVOUR, "Retail")),
        // Era recordings persist as Classic.
        GameFlavor::Classic | GameFlavor::Era => chips.push(Chip::new(GROUP_FLAVOUR, "Classic")),
        GameFlavor::Unknown(_) => {}
    }
    if let Some(player) = &entry.player {
        if !player.name.is_empty() {
            chips.push(Chip::new(GROUP_NAME, player.name.clone()));
        }
        if let Some(spec) = player.spec_id.and_then(spec_name) {
            chips.push(Chip::new(GROUP_SPEC, spec));
        }
    }
    for combatant in &entry.combatants {
        if let (Some(name), Some(_)) = (&combatant.name, combatant.spec_id)
            && !name.is_empty()
        {
            chips.push(Chip::new(GROUP_NAME, name.clone()));
        }
    }

    // Category-specific suggestions.
    match &entry.details {
        ActivityDetails::Raid {
            zone_name,
            encounter_name,
            difficulty_id,
            ..
        } => {
            push_named(&mut chips, GROUP_ZONE, zone_name.as_deref());
            chips.push(Chip::new(
                GROUP_RESULT,
                if entry.outcome == Outcome::Win {
                    "Kill"
                } else {
                    "Wipe"
                },
            ));
            if let Some(difficulty) = difficulty_id.and_then(raid_difficulty) {
                chips.push(difficulty);
            }
            push_named(&mut chips, GROUP_ENCOUNTER, encounter_name.as_deref());
        }
        ActivityDetails::Dungeon {
            dungeon_name,
            affixes,
            upgrade_level,
            ..
        } => {
            push_named(&mut chips, GROUP_DUNGEON, dungeon_name.as_deref());
            for affix in affixes {
                if let Some(name) = affix_name(*affix) {
                    chips.push(Chip::new(GROUP_AFFIX, name));
                }
            }
            if entry.outcome != Outcome::Complete {
                chips.push(Chip::new(GROUP_RESULT, "Abandoned"));
            } else if upgrade_level.is_some_and(|level| level > 0) {
                let level = upgrade_level.unwrap_or(0);
                chips.push(Chip::new(GROUP_RESULT, format!("{level} Chests")));
                chips.push(Chip::new(GROUP_RESULT, "Timed"));
            } else {
                chips.push(Chip::new(GROUP_RESULT, "Depleted"));
            }
        }
        ActivityDetails::ArenaOrBattleground { map_name, .. }
        | ActivityDetails::SoloRounds { map_name, .. } => {
            push_named(&mut chips, GROUP_ZONE, map_name.as_deref());
            push_win_loss(&mut chips, entry.outcome);
        }
        // Clips and manual recordings carry the generic suggestions plus a
        // win/loss result when one is known.
        _ => push_win_loss(&mut chips, entry.outcome),
    }

    chips
}

fn push_named(chips: &mut Vec<Chip>, group: u16, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        chips.push(Chip::new(group, value.to_owned()));
    }
}

fn push_win_loss(chips: &mut Vec<Chip>, outcome: Outcome) {
    match outcome {
        Outcome::Win => chips.push(Chip::new(GROUP_RESULT, "Win")),
        Outcome::Loss => chips.push(Chip::new(GROUP_RESULT, "Loss")),
        _ => {}
    }
}

fn raid_difficulty(id: u32) -> Option<Chip> {
    match id {
        17 => Some(Chip::new(GROUP_DIFFICULTY_LFR, "LFR")),
        14 => Some(Chip::new(GROUP_DIFFICULTY_NORMAL, "Normal")),
        15 => Some(Chip::new(GROUP_DIFFICULTY_HEROIC, "Heroic")),
        16 => Some(Chip::new(GROUP_DIFFICULTY_MYTHIC, "Mythic")),
        _ => None,
    }
}

/// Narrow the available suggestions: case-insensitive substring on the label,
/// excluding already-selected labels.
/// Typing narrows only; it does not filter rows until a suggestion is chosen.
pub fn narrow(available: &[Chip], query: &str, selected: &[Chip], limit: usize) -> Vec<Chip> {
    let needle = query.trim().to_lowercase();
    available
        .iter()
        .filter(|chip| {
            !selected.iter().any(|s| s.label == chip.label)
                && (needle.is_empty() || chip.label.to_lowercase().contains(&needle))
        })
        .take(limit)
        .cloned()
        .collect()
}

/// A row passes when every selected chip occurs in its combined suggestion set
/// (primary + POVs) and its start is inside the date range.
pub fn row_matches(
    combined: &BTreeSet<Chip>,
    start_unix_ms: i64,
    selected: &[Chip],
    range: Option<(i64, i64)>,
) -> bool {
    range.is_none_or(|(start, end)| start_unix_ms >= start && start_unix_ms <= end)
        && selected.iter().all(|chip| combined.contains(chip))
}

/// The union of suggestions over an activity's primary entry and its POVs.
pub fn combined_suggestions<'a>(
    entries: impl IntoIterator<Item = &'a LibraryEntry>,
) -> BTreeSet<Chip> {
    entries
        .into_iter()
        .flat_map(suggestions_for_entry)
        .collect()
}

// --- Factual lookup tables ---

/// Spec id → spec name. Public so the multi-POV selector labels viewpoints
/// with the same table.
pub fn spec_name(id: u16) -> Option<String> {
    SPEC_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .map(|(_, name)| (*name).to_owned())
}

/// Affix id → affix name. Public so the library's Affixes column shows names.
pub fn affix_name(id: u32) -> Option<String> {
    AFFIX_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .map(|(_, name)| (*name).to_owned())
}

#[rustfmt::skip]
static SPEC_NAMES: &[(u16, &str)] = &[
    (250, "Blood"), (251, "Frost"), (252, "Unholy"), (577, "Havoc"), (581, "Vengeance"),
    (1480, "Devourer"), (102, "Balance"), (103, "Feral"), (104, "Guardian"), (105, "Restoration"),
    (1467, "Devastation"), (1468, "Preservation"), (1473, "Augmentation"), (253, "Beast Mastery"),
    (254, "Marksmanship"), (255, "Survival"), (62, "Arcane"), (63, "Fire"), (64, "Frost"),
    (268, "Brewmaster"), (269, "Windwalker"), (270, "Mistweaver"), (65, "Holy"), (66, "Protection"),
    (70, "Retribution"), (256, "Discipline"), (257, "Holy"), (258, "Shadow"), (259, "Assassination"),
    (260, "Outlaw"), (261, "Subtlety"), (262, "Elemental"), (263, "Enhancement"), (264, "Restoration"),
    (265, "Affliction"), (266, "Demonology"), (267, "Destruction"), (71, "Arms"), (72, "Fury"),
    (73, "Protection"),
];

#[rustfmt::skip]
static AFFIX_NAMES: &[(u32, &str)] = &[
    (1, "Overflowing"), (2, "Skittish"), (3, "Volcanic"), (4, "Necrotic"), (5, "Teeming"),
    (6, "Raging"), (7, "Bolstering"), (8, "Sanguine"), (9, "Tyrannical"), (10, "Fortified"),
    (11, "Bursting"), (12, "Grievous"), (13, "Explosive"), (14, "Quaking"), (117, "Reaping"),
    (120, "Awakened"), (121, "Prideful"), (122, "Inspiring"), (123, "Spiteful"), (124, "Storming"),
    (128, "Tormented"), (130, "Encrypted"), (131, "Shrouded"), (133, "Focused"), (134, "Entangling"),
    (135, "Afflicted"), (136, "Incorporeal"), (137, "Shielding"), (144, "Thorned"), (145, "Reckless"),
    (146, "Attuned"), (147, "Guile"), (148, "Ascendant"), (152, "Peril"), (153, "Frenzied"),
    (158, "Voidbound"), (159, "Oblivion"), (160, "Devour"), (162, "Pulsar"), (165, "Lindormi's Guidance"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use warcraft_rustcorder::domain::{Category, PlayerSummary};

    fn base(category: Category, details: ActivityDetails, outcome: Outcome) -> LibraryEntry {
        let mut entry = crate::ui::window::tests::entry(category, "T", 1_000);
        entry.details = details;
        entry.outcome = outcome;
        entry
    }

    fn labels(chips: &[Chip]) -> Vec<&str> {
        chips.iter().map(|chip| chip.label.as_str()).collect()
    }

    #[test]
    fn raid_and_dungeon_suggestions_cover_their_details_and_results() {
        let raid = ActivityDetails::Raid {
            zone_id: Some(1),
            zone_name: Some("Vault of the Incarnates".to_owned()),
            encounter_id: Some(2),
            encounter_name: Some("Raszageth".to_owned()),
            difficulty_id: Some(16),
            difficulty: Some("Mythic".to_owned()),
            pull: Some(3),
            boss_percent: None,
        };
        let mut raid = base(Category::Raids, raid, Outcome::Win);
        raid.protected = true;
        raid.player = Some(PlayerSummary {
            name: "Alice".to_owned(),
            realm: None,
            guid: None,
            spec_id: Some(64),
        });
        let dungeon = ActivityDetails::Dungeon {
            zone_id: None,
            dungeon_name: Some("Halls of Valor".to_owned()),
            map_id: None,
            keystone_level: Some(20),
            affixes: vec![9, 10, 152],
            upgrade_level: Some(2),
        };
        let timed = base(Category::MythicPlus, dungeon.clone(), Outcome::Complete);
        let abandoned = base(Category::MythicPlus, dungeon, Outcome::Abandoned);
        let cases: [(&LibraryEntry, &[&str]); 3] = [
            (
                &raid,
                &[
                    "Starred",
                    "Retail",
                    "Alice",
                    "Frost",
                    "Vault of the Incarnates",
                    "Kill",
                    "Mythic",
                    "Raszageth",
                ],
            ),
            (
                &timed,
                &[
                    "Halls of Valor",
                    "Tyrannical",
                    "Fortified",
                    "Peril",
                    "2 Chests",
                    "Timed",
                ],
            ),
            (&abandoned, &["Abandoned"]),
        ];
        for (entry, expected) in cases {
            let chips = suggestions_for_entry(entry);
            let got = labels(&chips);
            for label in expected {
                assert!(got.contains(label), "missing {label}: {got:?}");
            }
        }
    }

    #[test]
    fn and_matching_spans_correlated_povs_and_dates() {
        let arena = || ActivityDetails::ArenaOrBattleground {
            map_id: Some(1),
            map_name: Some("Nagrand Arena".to_owned()),
            team_mmr: None,
        };
        let mut alice = base(Category::TwoVTwo, arena(), Outcome::Win);
        alice.player = Some(PlayerSummary {
            name: "Alice".to_owned(),
            realm: None,
            guid: None,
            spec_id: Some(64),
        });
        let mut bob = base(Category::TwoVTwo, arena(), Outcome::Win);
        bob.player = Some(PlayerSummary {
            name: "Bob".to_owned(),
            realm: None,
            guid: None,
            spec_id: Some(577),
        });

        // Alice's chip lives only on her POV, Havoc only on Bob's; both must
        // match against the correlated union.
        let combined = combined_suggestions([&alice, &bob]);
        let selected = vec![
            Chip::new(GROUP_NAME, "Alice"),
            Chip::new(GROUP_SPEC, "Havoc"),
        ];
        assert!(row_matches(&combined, 1_000, &selected, None));

        // A chip present on neither POV fails.
        let missing = vec![Chip::new(GROUP_NAME, "Carol")];
        assert!(!row_matches(&combined, 1_000, &missing, None));

        // Date range is inclusive and only applied when both endpoints exist.
        assert!(row_matches(&combined, 1_000, &[], Some((1_000, 2_000))));
        assert!(!row_matches(&combined, 999, &[], Some((1_000, 2_000))));
    }

    #[test]
    fn narrowing_excludes_selected_and_matches_substring() {
        let available = vec![
            Chip::new(GROUP_NAME, "Alice"),
            Chip::new(GROUP_NAME, "Bob"),
            Chip::new(GROUP_SPEC, "Frost"),
        ];
        let selected = vec![Chip::new(GROUP_NAME, "Bob")];
        let narrowed = narrow(&available, "o", &selected, usize::MAX);
        // "Bob" excluded (selected); "Frost" matches the "o".
        assert_eq!(labels(&narrowed), vec!["Frost"]);
    }
}
