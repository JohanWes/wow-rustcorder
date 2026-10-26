// SPDX-License-Identifier: GPL-3.0-or-later

//! GTK-free application domain types.

use std::fmt;
use std::num::NonZeroU64;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

pub const BLOODLUST_DURATION_MS: u64 = 40_000;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordingId(String);

impl RecordingId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Legacy recording ids are the bare media file name.
    pub fn from_media_name(name: &str) -> Self {
        Self(name.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RecordingId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RecordingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameFlavor {
    Retail,
    Classic,
    /// Classic Era log source. Only used to tag parsed events and key per-flavour
    /// engine state; Era recordings store `Classic` in their metadata.
    Era,
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    TwoVTwo,
    ThreeVThree,
    FiveVFive,
    Skirmish,
    SoloShuffle,
    MythicPlus,
    Raids,
    Battlegrounds,
    Manual,
    Clip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Win,
    Loss,
    Complete,
    Abandoned,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeathMarkerVisibility {
    None,
    Own,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerVisibility {
    Hidden,
    Visible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "gib", rename_all = "snake_case")]
pub enum StorageLimit {
    Unlimited,
    Gib(NonZeroU64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSummary {
    pub name: String,
    pub realm: Option<String>,
    pub guid: Option<String>,
    pub spec_id: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CombatantSummary {
    pub name: Option<String>,
    pub realm: Option<String>,
    pub guid: Option<String>,
    pub region: Option<String>,
    pub spec_id: Option<u16>,
    pub team_id: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RaidDifficulty {
    Lfr,
    Normal,
    Heroic,
    Mythic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundSummary {
    pub round: u32,
    pub outcome: Outcome,
    pub start_ms: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityDetails {
    Raid {
        zone_id: Option<u32>,
        zone_name: Option<String>,
        encounter_id: Option<u32>,
        encounter_name: Option<String>,
        difficulty_id: Option<u32>,
        difficulty: Option<String>,
        pull: Option<u32>,
        boss_percent: Option<u8>,
    },
    Dungeon {
        zone_id: Option<u32>,
        dungeon_name: Option<String>,
        map_id: Option<u32>,
        keystone_level: Option<u32>,
        affixes: Vec<u32>,
        upgrade_level: Option<u8>,
    },
    ArenaOrBattleground {
        map_id: Option<u32>,
        map_name: Option<String>,
        team_mmr: Option<u32>,
    },
    SoloRounds {
        map_id: Option<u32>,
        map_name: Option<String>,
        rounds_won: Option<u8>,
        rounds_played: Option<u8>,
        rounds: Vec<RoundSummary>,
    },
    Clip {
        source_recording: RecordingId,
        source_category: Category,
        source_title: Option<String>,
    },
    Manual,
}

impl ActivityDetails {
    pub fn matches_category(&self, category: &Category) -> bool {
        matches!(
            (category, self),
            (Category::Raids, Self::Raid { .. })
                | (Category::MythicPlus, Self::Dungeon { .. })
                | (
                    Category::TwoVTwo
                        | Category::ThreeVThree
                        | Category::FiveVFive
                        | Category::Skirmish
                        | Category::Battlegrounds,
                    Self::ArenaOrBattleground { .. }
                )
                | (Category::SoloShuffle, Self::SoloRounds { .. })
                | (Category::Clip, Self::Clip { .. })
                | (Category::Manual, Self::Manual)
        )
    }
}

/// One metric delta accumulated during a playback interval. `at_ms` is the
/// media-relative end of that interval, so the UI can include only completed
/// buckets at the current playhead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterSample {
    pub at_ms: u64,
    pub amount: u64,
    pub hits: u32,
    pub overheal: u64,
    /// Smallest and largest single hit in this interval. Both combine across
    /// intervals, so a playhead-limited projection stays exact.
    #[serde(default)]
    pub min: u64,
    #[serde(default)]
    pub max: u64,
}

/// One damage-meter aggregate: a spell or target bucket for one actor and one
/// metric. Actor totals derive structurally from the spell entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterEntry {
    pub metric: MeterMetric,
    /// Spell name (or "Melee"), the interrupted/dispelled spell name, or the
    /// target name. No spell IDs: names are the persisted keys.
    pub key: String,
    /// Destination raid marker (`0x80` = skull) at event time; 0 on spell rows.
    pub marker: u8,
    /// Effective amount, or the event count for Interrupts/Dispels.
    pub amount: u64,
    pub hits: u32,
    pub overheal: u64,
    /// Smallest and largest single hit; `max == 0` means the entry carries no
    /// per-hit statistics.
    #[serde(default)]
    pub min: u64,
    #[serde(default)]
    pub max: u64,
    /// Interval deltas used to reconstruct this entry at the playhead.
    #[serde(default)]
    pub samples: Vec<MeterSample>,
    /// This spell's own per-target split. Spell rows only; empty on target
    /// rows and on the folded "Other" row.
    #[serde(default)]
    pub targets: Vec<MeterEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterMetric {
    Damage,
    DamageTaken,
    Healing,
    Interrupts,
    Dispels,
    /// Successful casts: `amount` counts events, like the other count metrics.
    Casts,
    /// BUFF auras on friendly players: `amount` is accumulated uptime in
    /// milliseconds, `hits` the number of applications.
    Buffs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterDeathEventKind {
    Damage,
    Healing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterDeathEvent {
    pub kind: MeterDeathEventKind,
    pub at_ms: u64,
    pub source_name: String,
    pub spell_name: String,
    pub amount: u64,
    /// HP the unit was left on after this event; the death log draws it as a
    /// health bar. Zero when the log never reported it.
    #[serde(default)]
    pub hp: u64,
    /// Damage wasted past zero HP; only the killing blow carries any.
    #[serde(default)]
    pub overkill: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterDeath {
    pub guid: String,
    pub name: String,
    pub at_ms: u64,
    /// Max HP of the dead unit, sizing the death log bars. Zero when the log
    /// never reported it.
    #[serde(default)]
    pub max_hp: u64,
    pub events: Vec<MeterDeathEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterActor {
    /// Join key for class colours via `LibraryEntry.combatants`.
    pub guid: String,
    pub name: String,
    pub spells: Vec<MeterEntry>,
    pub targets: Vec<MeterEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterFight {
    /// Encounter name, "Trash", "Round N", or the activity title.
    pub label: String,
    /// Media-relative after finalize; activity-relative in the engine.
    pub start_ms: u64,
    pub end_ms: u64,
    /// First eligible meter event, on the same timeline as `start_ms`.
    #[serde(default)]
    pub first_event_ms: Option<u64>,
    /// First-to-last eligible event; the shared DPS/HPS denominator.
    pub active_ms: u64,
    /// Mythic+ trash recorded before the capturing player joined combat.
    /// Overall includes these fights; Current skips them.
    #[serde(default)]
    pub ambient: bool,
    pub actors: Vec<MeterActor>,
    #[serde(default)]
    pub deaths: Vec<MeterDeath>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeterData {
    #[serde(default)]
    pub fights: Vec<MeterFight>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineKind {
    Death,
    Bloodlust,
    Encounter,
    Trash,
    Round,
    Activity,
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineShape {
    Point,
    Span,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TimelineItem {
    shape: TimelineShape,
    kind: TimelineKind,
    start_ms: u64,
    end_ms: Option<u64>,
    label: Option<String>,
    outcome: Option<Outcome>,
    player_reference: Option<String>,
}

#[derive(Deserialize)]
struct TimelineItemData {
    shape: TimelineShape,
    kind: TimelineKind,
    start_ms: u64,
    end_ms: Option<u64>,
    label: Option<String>,
    outcome: Option<Outcome>,
    player_reference: Option<String>,
}

impl TimelineItem {
    pub fn point(
        kind: TimelineKind,
        start_ms: u64,
        label: Option<String>,
        outcome: Option<Outcome>,
        player_reference: Option<String>,
    ) -> Self {
        Self {
            shape: TimelineShape::Point,
            kind,
            start_ms,
            end_ms: None,
            label,
            outcome,
            player_reference,
        }
    }

    pub fn span(
        kind: TimelineKind,
        start_ms: u64,
        end_ms: u64,
        label: Option<String>,
        outcome: Option<Outcome>,
        player_reference: Option<String>,
    ) -> Result<Self, DomainError> {
        if end_ms < start_ms {
            return Err(DomainError::TimelineEndBeforeStart { start_ms, end_ms });
        }

        Ok(Self {
            shape: TimelineShape::Span,
            kind,
            start_ms,
            end_ms: Some(end_ms),
            label,
            outcome,
            player_reference,
        })
    }

    pub fn shape(&self) -> TimelineShape {
        self.shape
    }

    pub fn kind(&self) -> &TimelineKind {
        &self.kind
    }

    pub fn start_ms(&self) -> u64 {
        self.start_ms
    }

    pub fn end_ms(&self) -> Option<u64> {
        self.end_ms
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    pub fn player_reference(&self) -> Option<&str> {
        self.player_reference.as_deref()
    }

    fn try_from_data(data: TimelineItemData) -> Result<Self, DomainError> {
        match (data.shape, data.end_ms) {
            (TimelineShape::Point, None) => Ok(Self::point(
                data.kind,
                data.start_ms,
                data.label,
                data.outcome,
                data.player_reference,
            )),
            (TimelineShape::Span, Some(end_ms)) => Self::span(
                data.kind,
                data.start_ms,
                end_ms,
                data.label,
                data.outcome,
                data.player_reference,
            ),
            (TimelineShape::Point, Some(_)) => Err(DomainError::PointHasEnd),
            (TimelineShape::Span, None) => Err(DomainError::SpanMissingEnd),
        }
    }
}

impl<'de> Deserialize<'de> for TimelineItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let data = TimelineItemData::deserialize(deserializer)?;
        Self::try_from_data(data).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFacts {
    pub fps: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: Option<Codec>,
    /// Runtime filesystem fact, deliberately absent from sidecars: the media
    /// worker validates real outputs.
    #[serde(skip, default = "default_media_has_content")]
    pub has_content: bool,
}

fn default_media_has_content() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: RecordingId,
    pub media_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub category: Category,
    pub flavor: GameFlavor,
    pub title: String,
    pub start_unix_ms: i64,
    pub duration_ms: u64,
    pub outcome: Outcome,
    pub protected: bool,
    pub tag: Option<String>,
    pub activity_hash: Option<String>,
    pub player: Option<PlayerSummary>,
    pub combatants: Vec<CombatantSummary>,
    pub details: ActivityDetails,
    pub timeline: Vec<TimelineItem>,
    pub media: MediaFacts,
}

impl LibraryEntry {
    pub fn validate(&self) -> Result<(), DomainError> {
        if !self.details.matches_category(&self.category) {
            return Err(DomainError::CategoryDetailsMismatch {
                category: self.category.clone(),
            });
        }

        if let Some(offset_ms) = self.timeline.iter().find_map(|item| {
            (item.start_ms() > self.duration_ms)
                .then_some(item.start_ms())
                .or_else(|| item.end_ms().filter(|end| *end > self.duration_ms))
        }) {
            return Err(DomainError::TimelineOutsideDuration {
                offset_ms,
                duration_ms: self.duration_ms,
            });
        }

        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelatedActivity {
    pub primary_id: RecordingId,
    pub local_pov_ids: Vec<RecordingId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RecorderStatus {
    SetupRequired,
    WaitingForWow,
    Ready,
    Recording {
        category: Category,
        title: String,
        started_unix_ms: i64,
        manual: bool,
        test: bool,
    },
    Overrunning {
        title: String,
        started_unix_ms: i64,
    },
    Finalizing {
        title: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Finalize,
    Clip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkProgress {
    pub kind: WorkKind,
    pub completed: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    OpenSettings,
    ReselectCaptureTarget,
    Retry,
    OpenLogs,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub summary: String,
    pub safe_detail: Option<String>,
    pub occurred_unix_ms: i64,
    pub recovery_action: Option<RecoveryAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainError {
    TimelineEndBeforeStart { start_ms: u64, end_ms: u64 },
    PointHasEnd,
    SpanMissingEnd,
    CategoryDetailsMismatch { category: Category },
    TimelineOutsideDuration { offset_ms: u64, duration_ms: u64 },
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimelineEndBeforeStart { start_ms, end_ms } => write!(
                formatter,
                "timeline end {end_ms} ms is before start {start_ms} ms"
            ),
            Self::PointHasEnd => formatter.write_str("timeline point cannot have an end"),
            Self::SpanMissingEnd => formatter.write_str("timeline span requires an end"),
            Self::CategoryDetailsMismatch { category } => {
                write!(
                    formatter,
                    "activity details do not match category {category:?}"
                )
            }
            Self::TimelineOutsideDuration {
                offset_ms,
                duration_ms,
            } => write!(
                formatter,
                "timeline offset {offset_ms} ms exceeds media duration {duration_ms} ms"
            ),
        }
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(category: Category, details: ActivityDetails) -> LibraryEntry {
        LibraryEntry {
            id: RecordingId::new(),
            media_path: PathBuf::from("/recordings/example.mkv"),
            sidecar_path: PathBuf::from("/recordings/example.json"),
            category,
            flavor: GameFlavor::Retail,
            title: "Example".to_owned(),
            start_unix_ms: 1_700_000_000_000,
            duration_ms: 60_000,
            outcome: Outcome::Unknown,
            protected: false,
            tag: None,
            activity_hash: Some("activity".to_owned()),
            player: None,
            combatants: Vec::new(),
            details,
            timeline: Vec::new(),
            media: MediaFacts {
                fps: Some(60),
                width: Some(1920),
                height: Some(1080),
                codec: Some(Codec::H264),
                has_content: true,
            },
        }
    }

    #[test]
    fn timeline_rejects_invalid_bounds_and_invalid_json_shapes() {
        assert_eq!(
            TimelineItem::span(TimelineKind::Encounter, 200, 100, None, None, None),
            Err(DomainError::TimelineEndBeforeStart {
                start_ms: 200,
                end_ms: 100,
            })
        );

        let invalid_span = r#"{
            "shape":"span","kind":"round","start_ms":10,"end_ms":null,
            "label":null,"outcome":null,"player_reference":null
        }"#;
        assert!(serde_json::from_str::<TimelineItem>(invalid_span).is_err());

        let valid = TimelineItem::span(
            TimelineKind::Round,
            10,
            20,
            Some("Round 1".to_owned()),
            Some(Outcome::Win),
            None,
        )
        .expect("valid span");
        let encoded = serde_json::to_string(&valid).expect("serialize span");
        assert_eq!(
            serde_json::from_str::<TimelineItem>(&encoded).expect("deserialize span"),
            valid
        );
    }

    #[test]
    fn category_and_details_must_match() {
        let raid = ActivityDetails::Raid {
            zone_id: Some(1),
            zone_name: Some("Example Raid".to_owned()),
            encounter_id: Some(2),
            encounter_name: Some("Example Boss".to_owned()),
            difficulty_id: Some(16),
            difficulty: Some("Mythic".to_owned()),
            pull: Some(3),
            boss_percent: Some(42),
        };

        assert!(entry(Category::Raids, raid.clone()).validate().is_ok());
        assert_eq!(
            entry(Category::MythicPlus, raid).validate(),
            Err(DomainError::CategoryDetailsMismatch {
                category: Category::MythicPlus,
            })
        );

        let mut outside = entry(Category::Manual, ActivityDetails::Manual);
        outside.timeline.push(TimelineItem::point(
            TimelineKind::Death,
            60_001,
            None,
            None,
            None,
        ));
        assert_eq!(
            outside.validate(),
            Err(DomainError::TimelineOutsideDuration {
                offset_ms: 60_001,
                duration_ms: 60_000,
            })
        );
    }
}
