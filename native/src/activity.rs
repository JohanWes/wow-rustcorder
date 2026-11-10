// SPDX-License-Identifier: GPL-3.0-or-later

//! Activity state transitions and recording actions.
//!
//! Translates timestamped parsed events into automatic-recording actions and
//! recording metadata/timeline, using deterministic GTK-free state. One active
//! automatic activity is retained per flavour (`Retail`, `Classic`, `Era`);
//! PTR log sources share their base flavour's state.
//!
//! - Era log sources apply the Era rules and write `Classic` as the metadata
//!   flavour. `GameFlavor::Unknown` events are ignored.
//! - Events that would corrupt state (e.g. `CHALLENGE_MODE_*` arriving over a
//!   different in-flight category) are ignored.
//! - On a failed `Begin`, the coordinator clears the active activity with
//!   `force_end` and drops the emitted `Abandon` action.
//! - The coordinator drives data-timeout force ends: retail 10 min, classic/era
//!   2 min without new log data, ending at last-data time.
//! - `force_end` reuses the most recent config seen by `handle`, so the raid
//!   minimum-duration discard still applies to force-ended raids.

use std::collections::HashMap;

use crate::config::ActivitySettings;
use crate::domain::{
    ActivityDetails, BLOODLUST_DURATION_MS, Category, CombatantSummary, GameFlavor, MeterData,
    Outcome, PlayerSummary, RecordingId, RoundSummary, TimelineItem, TimelineKind,
};
use crate::meter::{BuffEvent, MeterAccumulator};
use crate::parser::{
    AuraType, CombatEvent, EMPTY_GUID, ParsedEvent, PlayerObservationKind, is_bloodlust_spell,
};

mod tables;

use tables::{
    CURRENT_RETAIL_ENCOUNTERS, PartyType, arena_zone_name, battleground_name, classic_arena_name,
    classic_battleground_name, classic_unique_aura, classic_unique_spec, difficulty_info,
    difficulty_order, dungeon_encounter_name, dungeon_name, dungeon_timers, md5_hex,
    mop_challenge_mode_name, mop_challenge_mode_timers, raid_lookup, raid_zone_id,
    retail_battleground_name, retail_unique_spec,
};
pub(crate) use tables::{instance_name, raid_short_name};

const RAID_DEFAULT_OVERRUN_MS: u64 = 3_000;
const PVP_DEFAULT_OVERRUN_MS: u64 = 3_000;
const MIN_RETAIL_BOSS_HP: u64 = 100_000_000;
const CHALLENGERS_PERIL_AFFIX: u32 = 152;
const CHALLENGERS_PERIL_ADJUST_MS: i64 = 90_000;
const MIN_FINAL_SEGMENT_MS: i64 = 10_000;
/// Difficulty every zone change reports while a keystone run is in progress.
const KEYSTONE_DIFFICULTY_ID: u32 = 8;
const DEATH_MARKER_BACK_OFFSET_MS: i64 = 2;
const BELOREN_ENCOUNTER_ID: u32 = 3182;
const ALLERIA_ENCOUNTER_ID: u32 = 3181;
const BELOREN_UNIT_NAME: &str = "Belo'ren";
const ALLERIA_UNIT_NAME: &str = "Alleria Windrunner";
const BELOREN_PHASE_SPELL: &str = "Rebirth";
pub(crate) const AFFILIATION_MINE: u64 = 0x1;
pub(crate) const REACTION_FRIENDLY: u64 = 0x10;
pub(crate) const CONTROL_PLAYER: u64 = 0x100;
const TYPE_PLAYER: u64 = 0x400;

pub(crate) fn is_unit_player(flags: u64) -> bool {
    flags & CONTROL_PLAYER != 0 && flags & TYPE_PLAYER != 0
}

pub(crate) fn is_unit_friendly(flags: u64) -> bool {
    flags & REACTION_FRIENDLY != 0
}

pub(crate) fn is_unit_self(flags: u64) -> bool {
    is_unit_friendly(flags) && flags & AFFILIATION_MINE != 0
}

/// Player-controlled and friendly: players and their pets/guardians alike.
pub(crate) fn is_player_controlled_friendly(flags: u64) -> bool {
    flags & CONTROL_PLAYER != 0 && is_unit_friendly(flags)
}

/// Name, realm, region from a `Name-Realm(-Region)` string.
fn ambiguate(name_realm: &str) -> (String, Option<String>, Option<String>) {
    let mut parts = name_realm.splitn(3, '-');
    let name = parts.next().unwrap_or_default().to_owned();
    let realm = parts.next().map(str::to_owned);
    let region = parts.next().map(str::to_owned);
    (name, realm, region)
}

fn win_loss(won: bool) -> Outcome {
    if won { Outcome::Win } else { Outcome::Loss }
}

pub(crate) fn relative_ms(started_at_ms: i64, at_ms: i64) -> u64 {
    (at_ms - started_at_ms).max(0) as u64
}
/// One logical recording in flight, emitted by `Begin` and completed by
/// `take_finished` after a `Complete`/`Abandon`/`Discard` action. End-time
/// fields are `None` until the activity finishes.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingDraft {
    pub id: RecordingId,
    pub category: Category,
    pub flavor: GameFlavor,
    /// Occurrence start of the activity (combat-log event time).
    pub started_at_ms: i64,
    pub overrun_ms: u64,
    pub details: ActivityDetails,
    pub player: Option<PlayerSummary>,
    pub combatants: Vec<CombatantSummary>,
    pub timeline: Vec<TimelineItem>,
    pub outcome: Option<Outcome>,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: Option<u64>,
    pub title: Option<String>,
    pub activity_hash: Option<String>,
    pub meter: MeterData,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ActivityAction {
    Begin {
        draft: Box<RecordingDraft>,
    },
    /// Ended normally. Outcome and end time are on the finished draft.
    Complete {
        id: RecordingId,
    },
    /// Force-ended (user or data timeout, at the supplied time) or superseded
    /// by another activity event (arena start during an activity, raid
    /// encounter during Mythic+, battleground zone-in): zero overrun and a
    /// loss-style outcome.
    Abandon {
        id: RecordingId,
    },
    Discard {
        id: RecordingId,
        reason: DiscardReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    /// Raid duration (including overrun) below the configured minimum.
    BelowMinDuration,
    /// The recording player could not be identified or has no combatant/name,
    /// so there is no metadata worth keeping the video for.
    IncompleteMetadata,
}

/// Deterministic activity engine. No filesystem, process, GTK, sleeps, global
/// singletons, or wall-clock reads: all times come from events or arguments.
#[derive(Default)]
pub struct ActivityEngine {
    retail: FlavorState,
    classic: FlavorState,
    era: FlavorState,
    finished: Vec<RecordingDraft>,
    config: Option<ActivitySettings>,
}

impl ActivityEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, event: ParsedEvent, config: &ActivitySettings) -> Vec<ActivityAction> {
        self.config = Some(config.clone());
        let ParsedEvent {
            flavor,
            occurred_at_ms,
            event,
        } = event;
        let (state, rules) = match flavor {
            GameFlavor::Retail => (&mut self.retail, Rules::Retail),
            GameFlavor::Classic => (&mut self.classic, Rules::Classic),
            GameFlavor::Era => (&mut self.era, Rules::Era),
            GameFlavor::Unknown(_) => return Vec::new(),
        };
        let mut actions = Vec::new();
        handle_event(
            state,
            rules,
            &event,
            occurred_at_ms,
            config,
            &mut self.finished,
            &mut actions,
        );
        actions
    }

    /// Force-end the flavour's active automatic activity. Returns no action
    /// when that flavour has none, so it can never end the wrong flavour.
    pub fn force_end(&mut self, flavor: GameFlavor, occurred_at_ms: i64) -> Vec<ActivityAction> {
        let state = match flavor {
            GameFlavor::Retail => &mut self.retail,
            GameFlavor::Classic => &mut self.classic,
            GameFlavor::Era => &mut self.era,
            GameFlavor::Unknown(_) => return Vec::new(),
        };
        let Some(active) = state.active.take() else {
            return Vec::new();
        };
        let config = self.config.clone().unwrap_or_default();
        let mut actions = Vec::new();
        finish(
            active,
            occurred_at_ms,
            EndKind::Abandon,
            &config,
            &mut self.finished,
            &mut actions,
        );
        actions
    }

    /// Take the finished draft for a `Complete`/`Abandon`/`Discard` action.
    pub fn take_finished(&mut self, id: &RecordingId) -> Option<RecordingDraft> {
        let position = self.finished.iter().position(|draft| &draft.id == id)?;
        Some(self.finished.remove(position))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rules {
    Retail,
    Classic,
    Era,
}

#[derive(Default)]
struct FlavorState {
    active: Option<ActiveActivity>,
}

struct ActiveActivity {
    id: RecordingId,
    category: Category,
    flavor: GameFlavor,
    started_at_ms: i64,
    overrun_ms: u64,
    combatants: Combatants,
    player_guid: Option<String>,
    timeline: Vec<TimelineItem>,
    meter: MeterAccumulator,
    kind: ActiveKind,
}

impl ActiveActivity {
    fn new(
        category: Category,
        flavor: GameFlavor,
        started_at_ms: i64,
        overrun_ms: u64,
        meter: MeterAccumulator,
        kind: ActiveKind,
    ) -> Self {
        Self {
            id: RecordingId::new(),
            category,
            flavor,
            started_at_ms,
            overrun_ms,
            combatants: Combatants::default(),
            player_guid: None,
            timeline: Vec::new(),
            meter,
            kind,
        }
    }
}

enum ActiveKind {
    Raid(RaidState),
    Challenge(ChallengeState),
    Arena(ArenaState),
    Battleground { zone_id: u32 },
    SoloShuffle(ShuffleState),
}

struct RaidState {
    encounter_id: u32,
    encounter_name: String,
    difficulty_id: u32,
    current_hp: u64,
    max_hp: u64,
    boss_unit_name: &'static str,
    boss_unit_active: bool,
}

struct ChallengeState {
    zone_id: u32,
    map_id: u32,
    level: u32,
    affixes: Vec<u32>,
    cm_duration_ms: Option<u64>,
    segments: Vec<CmSegment>,
}

struct CmSegment {
    kind: TimelineKind,
    start_ms: i64,
    end_ms: Option<i64>,
    label: Option<String>,
    result: Option<bool>,
}

struct ArenaState {
    zone_id: u32,
}

struct ShuffleState {
    zone_id: u32,
    rounds: Vec<ShuffleRound>,
}

struct ShuffleRound {
    start_ms: i64,
    end_ms: Option<i64>,
    result: bool,
    combatants: Combatants,
    player_guid: Option<String>,
    has_death: bool,
    item_emitted: bool,
}

impl ShuffleRound {
    fn new(start_ms: i64) -> Self {
        Self {
            start_ms,
            end_ms: None,
            result: false,
            combatants: Combatants::default(),
            player_guid: None,
            has_death: false,
            item_emitted: false,
        }
    }
}

/// Insertion-ordered combatant map matching JS `Map` semantics.
#[derive(Default)]
struct Combatants {
    entries: Vec<CombatantState>,
    index: HashMap<String, usize>,
}

impl Combatants {
    fn get(&self, guid: &str) -> Option<&CombatantState> {
        self.index
            .get(guid)
            .map(|position| &self.entries[*position])
    }

    fn contains(&self, guid: &str) -> bool {
        self.index.contains_key(guid)
    }

    /// Insert or replace, keeping the original position on replacement.
    /// Returns true when the GUID is new.
    fn upsert(&mut self, combatant: CombatantState) -> bool {
        if let Some(position) = self.index.get(&combatant.guid) {
            self.entries[*position] = combatant;
            return false;
        }
        self.index
            .insert(combatant.guid.clone(), self.entries.len());
        self.entries.push(combatant);
        true
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn iter(&self) -> impl Iterator<Item = &CombatantState> {
        self.entries.iter()
    }
}

#[derive(Clone, Default)]
struct CombatantState {
    guid: String,
    team_id: Option<u8>,
    spec_id: Option<u16>,
    name: Option<String>,
    realm: Option<String>,
    region: Option<String>,
}

impl CombatantState {
    /// A GUID is not required: the map key guarantees one.
    fn is_fully_defined(&self) -> bool {
        self.team_id.is_some()
            && self.name.is_some()
            && self.realm.is_some()
            && self.spec_id.is_some()
    }
}

#[derive(Clone, Copy)]
enum EndKind {
    Complete(Outcome),
    Abandon,
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    state: &mut FlavorState,
    rules: Rules,
    event: &CombatEvent,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    match event {
        CombatEvent::ZoneChanged {
            zone_id,
            difficulty_id,
        } => {
            if rules == Rules::Retail
                && left_challenge(state.active.as_ref(), *zone_id, *difficulty_id)
            {
                // Same ending as the CHALLENGE_MODE_END the game only logs
                // once the next key starts.
                handle_challenge_end(state, rules, false, 0, at_ms, config, finished, actions);
                return;
            }
            handle_zone_change(state, rules, *zone_id, at_ms, config, finished, actions);
        }
        CombatEvent::EncounterStarted {
            encounter_id,
            name,
            difficulty_id,
        } => handle_encounter_start(
            state,
            rules,
            *encounter_id,
            name,
            *difficulty_id,
            at_ms,
            config,
            finished,
            actions,
        ),
        CombatEvent::EncounterEnded {
            difficulty_id,
            success,
        } => handle_encounter_end(
            state,
            rules,
            *difficulty_id,
            *success,
            at_ms,
            config,
            finished,
            actions,
        ),
        CombatEvent::ChallengeStarted {
            zone_id,
            map_id,
            level,
            affixes,
        } => handle_challenge_start(
            state, rules, *zone_id, *map_id, *level, affixes, at_ms, config, actions,
        ),
        CombatEvent::ChallengeEnded {
            success,
            duration_ms,
        } => handle_challenge_end(
            state,
            rules,
            *success,
            *duration_ms,
            at_ms,
            config,
            finished,
            actions,
        ),
        CombatEvent::ArenaStarted {
            zone_id,
            match_type,
        } => handle_arena_start(
            state, rules, *zone_id, match_type, at_ms, config, finished, actions,
        ),
        CombatEvent::ArenaEnded { winning_team_id } => handle_arena_end(
            state,
            rules,
            *winning_team_id,
            at_ms,
            config,
            finished,
            actions,
        ),
        CombatEvent::Combatant {
            guid,
            team_id,
            spec_id,
        } => handle_combatant_info(state, rules, guid, *team_id, *spec_id),
        CombatEvent::PlayerObserved {
            kind,
            aura_type,
            spell_id,
            guid,
            name,
            flags,
            target_guid,
            target_name,
            target_flags,
            spell_name,
            owner_guid,
        } => handle_player_observed(
            state,
            rules,
            *kind,
            *spell_id,
            guid,
            name,
            *flags,
            target_guid,
            target_name,
            *target_flags,
            *aura_type,
            spell_name,
            owner_guid.as_deref(),
            at_ms,
        ),
        CombatEvent::UnitDied {
            guid,
            name,
            flags,
            unconscious,
        } => handle_unit_died(
            state,
            rules,
            guid,
            name,
            *flags,
            *unconscious,
            at_ms,
            config,
            finished,
            actions,
        ),
        CombatEvent::Damage {
            source_guid,
            source_name,
            source_flags,
            source_owner_guid,
            dest_guid,
            dest_name,
            dest_flags,
            dest_raid_marker,
            spell_name,
            amount,
            dest_current_hp,
            dest_max_hp,
        } => {
            // Boss HP keeps flowing from the same event: destination HP is
            // only set when the advanced block identified the destination.
            if let (Some(current), Some(maximum)) = (dest_current_hp, dest_max_hp) {
                handle_boss_health(state, rules, dest_name, *current, *maximum);
            }
            if let Some(active) = state.active.as_mut() {
                if is_player_controlled_friendly(*source_flags)
                    && let Some(owner) = source_owner_guid
                {
                    active.meter.record_owner(source_guid, owner, None);
                }
                if let (Some(current), Some(maximum)) = (dest_current_hp, dest_max_hp) {
                    active.meter.note_hp(dest_guid, *current, *maximum, at_ms);
                }
                active.meter.damage(
                    source_guid,
                    source_name,
                    *source_flags,
                    dest_guid,
                    dest_name,
                    *dest_flags,
                    *dest_raid_marker,
                    spell_name,
                    *amount,
                    at_ms,
                );
            }
        }
        CombatEvent::Heal {
            source_guid,
            source_name,
            source_flags,
            dest_guid,
            dest_name,
            dest_flags,
            dest_raid_marker,
            spell_name,
            amount,
            overheal,
            dest_current_hp,
            dest_max_hp,
        } => {
            if let Some(active) = state.active.as_mut() {
                if let (Some(current), Some(maximum)) = (dest_current_hp, dest_max_hp) {
                    active.meter.note_hp(dest_guid, *current, *maximum, at_ms);
                }
                active.meter.heal(
                    source_guid,
                    source_name,
                    *source_flags,
                    dest_guid,
                    dest_name,
                    *dest_flags,
                    *dest_raid_marker,
                    spell_name,
                    *amount,
                    *overheal,
                    at_ms,
                );
            }
        }
        CombatEvent::Support {
            metric,
            supporter_guid,
            source_guid,
            dest_name,
            dest_raid_marker,
            spell_name,
            amount,
            overheal,
        } => {
            if let Some(active) = state.active.as_mut() {
                active.meter.support(
                    *metric,
                    supporter_guid,
                    source_guid,
                    dest_name,
                    *dest_raid_marker,
                    spell_name,
                    *amount,
                    *overheal,
                    at_ms,
                );
            }
        }
        CombatEvent::Interrupt {
            source_guid,
            source_name,
            source_flags,
            dest_name,
            dest_raid_marker,
            spell_name,
        } => {
            if let Some(active) = state.active.as_mut() {
                active.meter.interrupt(
                    source_guid,
                    source_name,
                    *source_flags,
                    dest_name,
                    *dest_raid_marker,
                    spell_name,
                    at_ms,
                );
            }
        }
        CombatEvent::Dispel {
            source_guid,
            source_name,
            source_flags,
            dest_name,
            dest_raid_marker,
            spell_name,
        } => {
            if let Some(active) = state.active.as_mut() {
                active.meter.dispel(
                    source_guid,
                    source_name,
                    *source_flags,
                    dest_name,
                    *dest_raid_marker,
                    spell_name,
                    at_ms,
                );
            }
        }
        CombatEvent::Summon {
            source_guid,
            source_name,
            source_flags,
            pet_guid,
        } => {
            if is_player_controlled_friendly(*source_flags)
                && let Some(active) = state.active.as_mut()
            {
                active
                    .meter
                    .record_owner(pet_guid, source_guid, Some(source_name));
            }
        }
        CombatEvent::BossCast {
            source_name,
            spell_name,
        } => handle_boss_cast(state, rules, source_name, spell_name),
    }
}

// --- Shared activity helpers ---

fn allow_record(config: &ActivitySettings, category: &Category) -> bool {
    match category {
        Category::TwoVTwo => config.record_two_v_two,
        Category::ThreeVThree => config.record_three_v_three,
        Category::FiveVFive => config.record_five_v_five,
        Category::Skirmish => config.record_skirmish,
        Category::SoloShuffle => config.record_solo_shuffle,
        Category::MythicPlus => config.record_dungeons,
        Category::Raids => config.record_raids,
        Category::Battlegrounds => config.record_battlegrounds,
        _ => false,
    }
}

fn begin(
    state: &mut FlavorState,
    active: ActiveActivity,
    config: &ActivitySettings,
    actions: &mut Vec<ActivityAction>,
) {
    if !allow_record(config, &active.category) {
        return;
    }
    let draft = Box::new(draft_for(&active));
    state.active = Some(active);
    actions.push(ActivityAction::Begin { draft });
}

fn draft_for(active: &ActiveActivity) -> RecordingDraft {
    RecordingDraft {
        id: active.id.clone(),
        category: active.category.clone(),
        flavor: active.flavor.clone(),
        started_at_ms: active.started_at_ms,
        overrun_ms: active.overrun_ms,
        details: initial_details(active),
        player: None,
        combatants: Vec::new(),
        timeline: Vec::new(),
        outcome: None,
        ended_at_ms: None,
        duration_ms: None,
        title: None,
        activity_hash: None,
        meter: MeterData::default(),
    }
}

fn initial_details(active: &ActiveActivity) -> ActivityDetails {
    match &active.kind {
        ActiveKind::Raid(raid) => ActivityDetails::Raid {
            zone_id: Some(raid_zone_id(raid.encounter_id)),
            zone_name: Some(raid_lookup(raid.encounter_id).short_name.to_string()),
            encounter_id: Some(raid.encounter_id),
            encounter_name: Some(raid.encounter_name.clone()),
            difficulty_id: Some(raid.difficulty_id),
            difficulty: difficulty_info(raid.difficulty_id).map(|info| info.short.to_string()),
            pull: None,
            boss_percent: None,
        },
        ActiveKind::Challenge(challenge) => ActivityDetails::Dungeon {
            zone_id: Some(challenge.zone_id),
            dungeon_name: Some(dungeon_name(
                &active.flavor,
                challenge.zone_id,
                challenge.map_id,
            )),
            map_id: Some(challenge.map_id),
            keystone_level: Some(challenge.level),
            affixes: challenge.affixes.clone(),
            upgrade_level: None,
        },
        ActiveKind::Arena(arena) => ActivityDetails::ArenaOrBattleground {
            map_id: Some(arena.zone_id),
            map_name: arena_zone_name(&active.flavor, arena.zone_id),
            team_mmr: None,
        },
        ActiveKind::Battleground { zone_id } => ActivityDetails::ArenaOrBattleground {
            map_id: Some(*zone_id),
            map_name: Some(battleground_name(*zone_id).to_string()),
            team_mmr: None,
        },
        ActiveKind::SoloShuffle(shuffle) => ActivityDetails::SoloRounds {
            map_id: Some(shuffle.zone_id),
            map_name: arena_zone_name(&active.flavor, shuffle.zone_id),
            rounds_won: None,
            rounds_played: None,
            rounds: Vec::new(),
        },
    }
}

fn active_zone_id(active: &ActiveActivity) -> Option<u32> {
    match &active.kind {
        ActiveKind::Arena(arena) => Some(arena.zone_id),
        ActiveKind::Battleground { zone_id } => Some(*zone_id),
        ActiveKind::SoloShuffle(shuffle) => Some(shuffle.zone_id),
        _ => None,
    }
}

/// A key in progress always reports the keystone difficulty, also after
/// walking out and back in. Entering its dungeon at any other difficulty means
/// the group gave the key up and reset the instance.
fn left_challenge(active: Option<&ActiveActivity>, zone_id: u32, difficulty_id: u32) -> bool {
    matches!(
        active.map(|active| &active.kind),
        Some(ActiveKind::Challenge(challenge))
            if challenge.zone_id == zone_id && difficulty_id != KEYSTONE_DIFFICULTY_ID
    )
}

fn is_arena_category(category: &Category) -> bool {
    matches!(
        category,
        Category::TwoVTwo
            | Category::ThreeVThree
            | Category::FiveVFive
            | Category::Skirmish
            | Category::SoloShuffle
    )
}

/// Player-flag filtering, player-GUID assignment, create-or-update with
/// name/realm/region fill-in. Returns the combatant's position when recorded.
fn process_combatant(
    combatants: &mut Combatants,
    player_guid: &mut Option<String>,
    guid: &str,
    name_realm: &str,
    flags: u64,
    allow_new: bool,
) -> Option<usize> {
    if guid == EMPTY_GUID || !is_unit_player(flags) {
        return None;
    }
    if player_guid.is_none() && is_unit_self(flags) {
        *player_guid = Some(guid.to_string());
    }
    let existing = combatants.index.get(guid).copied();
    let position = existing.or(allow_new.then_some(combatants.entries.len()))?;
    if combatants
        .get(guid)
        .is_some_and(|combatant| combatant.name.is_some())
    {
        return Some(position);
    }
    let (name, realm, region) = ambiguate(name_realm);
    let mut combatant = combatants
        .get(guid)
        .cloned()
        .unwrap_or_else(|| CombatantState {
            guid: guid.to_string(),
            ..CombatantState::default()
        });
    combatant.name = Some(name);
    combatant.realm = realm;
    combatant.region = region;
    combatants.upsert(combatant);
    Some(position)
}

/// Classic arenas derive the category from the combatant count on every add.
fn update_arena_category(active: &mut ActiveActivity) {
    if !matches!(active.kind, ActiveKind::Arena(_)) || active.flavor != GameFlavor::Classic {
        return;
    }
    let size = active.combatants.len();
    active.category = if size < 5 {
        Category::TwoVTwo
    } else if size < 7 {
        Category::ThreeVThree
    } else {
        Category::FiveVFive
    };
}

/// The combatant map and player GUID that combatant events write: the current
/// round's for solo shuffle, the activity's otherwise.
fn roster(active: &ActiveActivity) -> (&Combatants, Option<&String>) {
    if let ActiveKind::SoloShuffle(shuffle) = &active.kind
        && let Some(round) = shuffle.rounds.last()
    {
        return (&round.combatants, round.player_guid.as_ref());
    }
    (&active.combatants, active.player_guid.as_ref())
}

fn roster_mut(active: &mut ActiveActivity) -> (&mut Combatants, &mut Option<String>) {
    if let ActiveKind::SoloShuffle(shuffle) = &mut active.kind
        && let Some(round) = shuffle.rounds.last_mut()
    {
        return (&mut round.combatants, &mut round.player_guid);
    }
    (&mut active.combatants, &mut active.player_guid)
}

// --- Encounter handling (raids and Mythic+ boss segments) ---

#[allow(clippy::too_many_arguments)]
fn handle_encounter_start(
    state: &mut FlavorState,
    rules: Rules,
    encounter_id: u32,
    name: &str,
    difficulty_id: u32,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    if rules == Rules::Retail {
        // The difficulty covers dungeon bosses the table has no name for.
        let known_dungeon = dungeon_encounter_name(encounter_id).is_some()
            || difficulty_info(difficulty_id).is_some_and(|info| info.party == PartyType::Party);
        if state.active.is_none() && known_dungeon {
            // Regular dungeon, or a Mythic+ below the recording threshold.
            return;
        }
        if state.active.is_some() && !known_dungeon {
            // Active Mythic+ but not a dungeon encounter: abandon it and start
            // the raid encounter (abandoned key into raid pull).
            let active = state.active.take().expect("checked above");
            finish(active, at_ms, EndKind::Abandon, config, finished, actions);
        }
        if state.active.is_none() {
            if config.current_raid_only && !CURRENT_RETAIL_ENCOUNTERS.contains(&encounter_id) {
                return;
            }
            let Some(info) = difficulty_info(difficulty_id) else {
                return;
            };
            let Some(actual) = info.order() else {
                return;
            };
            if actual < difficulty_order(&config.min_raid_difficulty) {
                return;
            }
            start_raid(
                state,
                rules,
                encounter_id,
                name,
                difficulty_id,
                at_ms,
                config,
                actions,
            );
            return;
        }
        let active = state.active.as_mut().expect("checked above");
        if !matches!(active.kind, ActiveKind::Challenge(_)) {
            return;
        }
        // Mythic+ boss encounter segment: close the open segment, then push a
        // boss segment labelled with the encounter name. The meter fight is
        // cut at the same transition.
        close_open_segment(active, at_ms);
        let label = dungeon_encounter_name(encounter_id)
            .unwrap_or(name)
            .to_string();
        if let ActiveKind::Challenge(challenge) = &mut active.kind {
            challenge.segments.push(CmSegment {
                kind: TimelineKind::Encounter,
                start_ms: at_ms,
                end_ms: None,
                label: Some(label.clone()),
                result: None,
            });
        }
        active.meter.cut(at_ms, label);
        return;
    }

    // Classic/Era base handler.
    if state.active.is_some() {
        return;
    }
    start_raid(
        state,
        rules,
        encounter_id,
        name,
        difficulty_id,
        at_ms,
        config,
        actions,
    );
}

#[allow(clippy::too_many_arguments)]
fn start_raid(
    state: &mut FlavorState,
    rules: Rules,
    encounter_id: u32,
    name: &str,
    difficulty_id: u32,
    at_ms: i64,
    config: &ActivitySettings,
    actions: &mut Vec<ActivityAction>,
) {
    let Some(info) = difficulty_info(difficulty_id) else {
        return;
    };
    if info.party != PartyType::Raid {
        return;
    }
    // Era activities record the Classic flavour.
    let flavor = match rules {
        Rules::Retail => GameFlavor::Retail,
        Rules::Classic | Rules::Era => GameFlavor::Classic,
    };
    let (boss_unit_name, boss_unit_active) = match encounter_id {
        BELOREN_ENCOUNTER_ID => (BELOREN_UNIT_NAME, false),
        ALLERIA_ENCOUNTER_ID => (ALLERIA_UNIT_NAME, true),
        _ => ("", true),
    };
    let active = ActiveActivity::new(
        Category::Raids,
        flavor,
        at_ms,
        RAID_DEFAULT_OVERRUN_MS,
        MeterAccumulator::new(at_ms, None),
        ActiveKind::Raid(RaidState {
            encounter_id,
            encounter_name: name.to_string(),
            difficulty_id,
            current_hp: 1,
            max_hp: 1,
            boss_unit_name,
            boss_unit_active,
        }),
    );
    begin(state, active, config, actions);
}

#[allow(clippy::too_many_arguments)]
fn handle_encounter_end(
    state: &mut FlavorState,
    rules: Rules,
    difficulty_id: u32,
    success: bool,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if rules == Rules::Retail && matches!(active.kind, ActiveKind::Challenge(_)) {
        // Mythic+ boss encounter ended: record its result, close its span and
        // start a fresh trash segment, cutting the meter fight with it.
        if let ActiveKind::Challenge(challenge) = &mut active.kind
            && let Some(segment) = challenge.segments.last_mut()
        {
            segment.result = Some(success);
        }
        close_open_segment(active, at_ms);
        if let ActiveKind::Challenge(challenge) = &mut active.kind {
            challenge.segments.push(CmSegment {
                kind: TimelineKind::Trash,
                start_ms: at_ms,
                end_ms: None,
                label: None,
                result: None,
            });
        }
        active.meter.cut_to_trash(at_ms);
        return;
    }
    let Some(info) = difficulty_info(difficulty_id) else {
        return;
    };
    if info.party != PartyType::Raid {
        return;
    }
    if success {
        active.overrun_ms = u64::from(config.raid_overrun_seconds) * 1_000;
    }
    let outcome = win_loss(success);
    let active = state.active.take().expect("checked above");
    finish(
        active,
        at_ms,
        EndKind::Complete(outcome),
        config,
        finished,
        actions,
    );
}

/// Close a currently open challenge segment into a timeline span. Event
/// times are monotonic in practice; the span end is clamped defensively.
fn close_open_segment(active: &mut ActiveActivity, at_ms: i64) {
    let started_at_ms = active.started_at_ms;
    let item = {
        let ActiveKind::Challenge(challenge) = &mut active.kind else {
            return;
        };
        let Some(segment) = challenge.segments.last_mut() else {
            return;
        };
        if segment.end_ms.is_some() {
            return;
        }
        segment.end_ms = Some(at_ms);
        segment_item(started_at_ms, segment)
    };
    active.timeline.push(item);
}

fn segment_item(started_at_ms: i64, segment: &CmSegment) -> TimelineItem {
    let start = relative_ms(started_at_ms, segment.start_ms);
    let end = relative_ms(started_at_ms, segment.end_ms.unwrap_or(segment.start_ms)).max(start);
    let outcome = segment.result.map(win_loss);
    TimelineItem::span(
        segment.kind.clone(),
        start,
        end,
        segment.label.clone(),
        outcome,
        None,
    )
    .expect("clamped span bounds")
}

// --- Challenge mode ---

#[allow(clippy::too_many_arguments)]
fn handle_challenge_start(
    state: &mut FlavorState,
    rules: Rules,
    zone_id: u32,
    map_id: u32,
    level: u32,
    affixes: &[u32],
    at_ms: i64,
    config: &ActivitySettings,
    actions: &mut Vec<ActivityAction>,
) {
    if rules == Rules::Era {
        return;
    }
    if state.active.is_some() {
        // A subsequent start for the in-flight dungeon is ignored, and a
        // challenge start over another category is not a recorded shape.
        // Either way the active activity stays.
        return;
    }
    match rules {
        Rules::Retail => {
            if dungeon_timers(map_id).is_none() {
                return;
            }
            if level < config.min_keystone_level {
                return;
            }
        }
        Rules::Classic => {
            if mop_challenge_mode_name(map_id).is_none() || !config.record_challenge_modes {
                return;
            }
        }
        Rules::Era => return,
    }
    let flavor = match rules {
        Rules::Retail => GameFlavor::Retail,
        _ => GameFlavor::Classic,
    };
    // Classic challenge modes always record level 0 and no affixes, and have
    // no initial trash segment (one fight labelled by the activity title).
    let (level, affixes, segments, meter) = match rules {
        Rules::Retail => (
            level,
            affixes.to_vec(),
            vec![CmSegment {
                kind: TimelineKind::Trash,
                start_ms: at_ms,
                end_ms: None,
                label: None,
                result: None,
            }],
            MeterAccumulator::trash(at_ms),
        ),
        _ => (
            0,
            Vec::new(),
            Vec::new(),
            MeterAccumulator::new(at_ms, None),
        ),
    };
    let active = ActiveActivity::new(
        Category::MythicPlus,
        flavor,
        at_ms,
        0,
        meter,
        ActiveKind::Challenge(ChallengeState {
            zone_id,
            map_id,
            level,
            affixes,
            cm_duration_ms: None,
            segments,
        }),
    );
    begin(state, active, config, actions);
}

#[allow(clippy::too_many_arguments)]
fn handle_challenge_end(
    state: &mut FlavorState,
    rules: Rules,
    success: bool,
    duration_ms: u64,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    if rules == Rules::Era {
        return;
    }
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if !matches!(active.kind, ActiveKind::Challenge(_)) {
        return;
    }
    if success && rules == Rules::Retail {
        active.overrun_ms = u64::from(config.dungeon_overrun_seconds) * 1_000;
    }
    let started_at_ms = active.started_at_ms;
    let mut emitted = None;
    if let ActiveKind::Challenge(challenge) = &mut active.kind {
        // The classic handler always passes a zero challenge duration.
        challenge.cm_duration_ms = Some(if rules == Rules::Retail {
            duration_ms
        } else {
            0
        });
        // Close the last segment, then drop it when shorter than ten seconds.
        if let Some(last) = challenge.segments.last_mut() {
            last.end_ms = Some(at_ms);
        }
        if let Some(last) = challenge.segments.last() {
            let length = last.end_ms.unwrap_or(last.start_ms) - last.start_ms;
            if length < MIN_FINAL_SEGMENT_MS {
                challenge.segments.pop();
            } else {
                emitted = Some(segment_item(started_at_ms, last));
            }
        }
    }
    if let Some(item) = emitted {
        active.timeline.push(item);
    }
    let outcome = match rules {
        Rules::Retail => {
            if success {
                Outcome::Complete
            } else {
                Outcome::Abandoned
            }
        }
        // Classic challenge modes always record success.
        _ => Outcome::Complete,
    };
    let active = state.active.take().expect("checked above");
    finish(
        active,
        at_ms,
        EndKind::Complete(outcome),
        config,
        finished,
        actions,
    );
}

// --- Arenas, battlegrounds, solo shuffle ---

#[allow(clippy::too_many_arguments)]
fn handle_arena_start(
    state: &mut FlavorState,
    rules: Rules,
    zone_id: u32,
    match_type: &str,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    if rules != Rules::Retail {
        return;
    }
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.category != Category::SoloShuffle)
    {
        // Arena start over a non-shuffle activity ends it (never a shuffle round).
        let active = state.active.take().expect("checked above");
        finish(active, at_ms, EndKind::Abandon, config, finished, actions);
    }
    let category = match match_type {
        "Rated Solo Shuffle" => Category::SoloShuffle,
        "2v2" => Category::TwoVTwo,
        // 3v3 retail war games are logged as 5v5.
        "3v3" | "5v5" => Category::ThreeVThree,
        "Skirmish" => Category::Skirmish,
        _ => return,
    };
    if state.active.is_none() && category == Category::SoloShuffle {
        let active = ActiveActivity::new(
            Category::SoloShuffle,
            GameFlavor::Retail,
            at_ms,
            PVP_DEFAULT_OVERRUN_MS,
            MeterAccumulator::new(at_ms, Some("Round 1".to_owned())),
            ActiveKind::SoloShuffle(ShuffleState {
                zone_id,
                rounds: vec![ShuffleRound::new(at_ms)],
            }),
        );
        begin(state, active, config, actions);
    } else if state.active.is_some() && category == Category::SoloShuffle {
        // New round of the existing shuffle. A previous round that never ended
        // is emitted as an unended round point.
        let active = state.active.as_mut().expect("checked above");
        let started_at_ms = active.started_at_ms;
        let mut pending = None;
        let mut round_number = 0;
        if let ActiveKind::SoloShuffle(shuffle) = &mut active.kind {
            let index = shuffle.rounds.len() - 1;
            if let Some(round) = shuffle.rounds.last_mut()
                && round.end_ms.is_none()
                && !round.item_emitted
            {
                round.item_emitted = true;
                pending = Some(round_point(started_at_ms, index, round));
            }
            shuffle.rounds.push(ShuffleRound::new(at_ms));
            round_number = shuffle.rounds.len();
        }
        if let Some(item) = pending {
            active.timeline.push(item);
        }
        // A new round cuts the meter fight at the existing round transition.
        active.meter.cut(at_ms, format!("Round {round_number}"));
    } else {
        let active = ActiveActivity::new(
            category,
            GameFlavor::Retail,
            at_ms,
            PVP_DEFAULT_OVERRUN_MS,
            MeterAccumulator::new(at_ms, None),
            ActiveKind::Arena(ArenaState { zone_id }),
        );
        begin(state, active, config, actions);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_arena_end(
    state: &mut FlavorState,
    rules: Rules,
    winning_team_id: u32,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    if rules != Rules::Retail {
        return;
    }
    let Some(active) = state.active.as_ref() else {
        return;
    };
    if matches!(active.kind, ActiveKind::SoloShuffle(_)) {
        // End of game always records a win; the round score is the detail.
        let active = state.active.take().expect("checked above");
        finish(
            active,
            at_ms,
            EndKind::Complete(Outcome::Win),
            config,
            finished,
            actions,
        );
        return;
    }
    let result = arena_result(active, winning_team_id);
    let outcome = win_loss(result);
    let active = state.active.take().expect("checked above");
    finish(
        active,
        at_ms,
        EndKind::Complete(outcome),
        config,
        finished,
        actions,
    );
}

/// False when the player is unknown.
fn arena_result(active: &ActiveActivity, winning_team_id: u32) -> bool {
    let Some(guid) = roster(active).1 else {
        return false;
    };
    let Some(player) = active.combatants.get(guid) else {
        return false;
    };
    player
        .team_id
        .is_some_and(|team| u32::from(team) == winning_team_id)
}

fn handle_zone_change(
    state: &mut FlavorState,
    rules: Rules,
    zone_id: u32,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    match rules {
        Rules::Retail => retail_zone_change(state, zone_id, at_ms, config, finished, actions),
        Rules::Classic => classic_zone_change(state, zone_id, at_ms, config, finished, actions),
        // The Era handler does not subscribe to zone changes.
        Rules::Era => {}
    }
}

fn retail_zone_change(
    state: &mut FlavorState,
    zone_id: u32,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    let is_zone_bg = retail_battleground_name(zone_id).is_some();
    let Some(active_ref) = state.active.as_ref() else {
        if is_zone_bg {
            start_battleground(state, zone_id, GameFlavor::Retail, at_ms, config, actions);
        }
        return;
    };
    let is_activity_bg = matches!(active_ref.kind, ActiveKind::Battleground { .. });
    if is_zone_bg && is_activity_bg {
        // Internal battleground zone change.
        return;
    }
    if !is_zone_bg && is_activity_bg {
        end_on_death_count(state, at_ms, config, finished, actions);
        return;
    }
    if is_arena_category(&active_ref.category) {
        if Some(zone_id) == active_zone_id(active_ref) {
            return;
        }
        // Zone change out of arena/shuffle: loss outcome.
        let active = state.active.take().expect("checked above");
        finish(
            active,
            at_ms,
            EndKind::Complete(Outcome::Loss),
            config,
            finished,
            actions,
        );
        return;
    }
    if is_zone_bg {
        // Zoned into a battleground over another activity.
        let active = state.active.take().expect("checked above");
        finish(active, at_ms, EndKind::Abandon, config, finished, actions);
        start_battleground(state, zone_id, GameFlavor::Retail, at_ms, config, actions);
    }
}

fn classic_zone_change(
    state: &mut FlavorState,
    zone_id: u32,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    if let Some(active_ref) = state.active.as_ref() {
        let activity_zone = active_zone_id(active_ref).unwrap_or(0);
        if matches!(
            active_ref.kind,
            ActiveKind::Arena(_) | ActiveKind::Battleground { .. }
        ) && zone_id != activity_zone
        {
            end_on_death_count(state, at_ms, config, finished, actions);
        }
        return;
    }
    if classic_battleground_name(zone_id).is_some() {
        start_battleground(state, zone_id, GameFlavor::Classic, at_ms, config, actions);
    } else if classic_arena_name(zone_id).is_some() {
        // Classic arenas start as 2v2; the roster size adjusts the category.
        let active = ActiveActivity::new(
            Category::TwoVTwo,
            GameFlavor::Classic,
            at_ms,
            PVP_DEFAULT_OVERRUN_MS,
            MeterAccumulator::new(at_ms, None),
            ActiveKind::Arena(ArenaState { zone_id }),
        );
        begin(state, active, config, actions);
    }
}

fn start_battleground(
    state: &mut FlavorState,
    zone_id: u32,
    flavor: GameFlavor,
    at_ms: i64,
    config: &ActivitySettings,
    actions: &mut Vec<ActivityAction>,
) {
    if state.active.is_some() {
        return;
    }
    let active = ActiveActivity::new(
        Category::Battlegrounds,
        flavor,
        at_ms,
        PVP_DEFAULT_OVERRUN_MS,
        MeterAccumulator::new(at_ms, None),
        ActiveKind::Battleground { zone_id },
    );
    begin(state, active, config, actions);
}

/// Battlegrounds, and classic arenas (the player is always team 1), have no
/// result event: the recorded result is always the death-count estimate.
fn end_on_death_count(
    state: &mut FlavorState,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    let Some(active) = state.active.take() else {
        return;
    };
    let outcome = battleground_estimate(&active);
    finish(
        active,
        at_ms,
        EndKind::Complete(outcome),
        config,
        finished,
        actions,
    );
}

/// Winner is the team with the least deaths (best effort estimate).
fn battleground_estimate(active: &ActiveActivity) -> Outcome {
    let friends_dead = death_count(active, true);
    let enemies_dead = death_count(active, false);
    win_loss(friends_dead < enemies_dead)
}

/// Deaths are stored as timeline points: friendly deaths carry `Loss`, enemy
/// deaths carry `Win`.
fn death_count(active: &ActiveActivity, friendly: bool) -> usize {
    let want = win_loss(!friendly);
    active
        .timeline
        .iter()
        .filter(|item| matches!(item.kind(), TimelineKind::Death) && item.outcome() == Some(want))
        .count()
}

// --- Combatants, player observation, deaths, boss health ---

fn handle_combatant_info(
    state: &mut FlavorState,
    rules: Rules,
    guid: &str,
    team_id: Option<u8>,
    spec_id: Option<u16>,
) {
    let Some(active) = state.active.as_mut() else {
        return;
    };
    match rules {
        Rules::Retail => {
            let target = roster_mut(active).0;
            if target
                .get(guid)
                .is_some_and(CombatantState::is_fully_defined)
            {
                return;
            }
            target.upsert(CombatantState {
                guid: guid.to_string(),
                team_id,
                spec_id,
                ..CombatantState::default()
            });
        }
        Rules::Classic => {
            let target = roster_mut(active).0;
            if target.contains(guid) {
                return;
            }
            target.upsert(CombatantState {
                guid: guid.to_string(),
                ..CombatantState::default()
            });
        }
        Rules::Era => {
            roster_mut(active).0.upsert(CombatantState {
                guid: guid.to_string(),
                team_id,
                spec_id,
                ..CombatantState::default()
            });
        }
    }
    update_arena_category(active);
}

#[allow(clippy::too_many_arguments)]
fn handle_player_observed(
    state: &mut FlavorState,
    rules: Rules,
    kind: PlayerObservationKind,
    spell_id: u32,
    guid: &str,
    name: &str,
    flags: u64,
    target_guid: &str,
    target_name: &str,
    target_flags: u64,
    aura_type: Option<AuraType>,
    spell_name: &str,
    owner_guid: Option<&str>,
    at_ms: i64,
) {
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if let Some(owner) = owner_guid {
        active.meter.record_owner(guid, owner, None);
    }
    // BUFF uptime lives with the buffed unit, not the caster: shared buffs
    // and external cooldowns must land on the player who is powered by them.
    let buff = match kind {
        PlayerObservationKind::AuraApplied => Some(BuffEvent::Applied),
        PlayerObservationKind::AuraRefreshed => Some(BuffEvent::Refreshed),
        PlayerObservationKind::AuraRemoved => Some(BuffEvent::Removed),
        PlayerObservationKind::CastSucceeded => None,
    };
    if let (Some(event), Some(AuraType::Buff)) = (buff, aura_type) {
        active.meter.buff(
            event,
            target_guid,
            target_name,
            target_flags,
            spell_name,
            at_ms,
        );
    }
    if kind == PlayerObservationKind::CastSucceeded {
        active.meter.cast(guid, name, flags, spell_name, at_ms);
    }
    if kind == PlayerObservationKind::CastSucceeded && is_bloodlust_spell(spell_id) {
        let start_ms = relative_ms(active.started_at_ms, at_ms);
        let duplicate = active
            .timeline
            .iter()
            .any(|item| item.kind() == &TimelineKind::Bloodlust && item.start_ms() == start_ms);
        if !duplicate {
            let item = TimelineItem::span(
                TimelineKind::Bloodlust,
                start_ms,
                start_ms.saturating_add(BLOODLUST_DURATION_MS),
                Some(spell_name.to_owned()),
                None,
                None,
            )
            .expect("bloodlust duration is positive");
            active.timeline.push(item);
        }
    }
    // Removals and refreshes only bookkeep the meter; they identified no new
    // player that the applied/cast events before them had not.
    if matches!(
        kind,
        PlayerObservationKind::CastSucceeded | PlayerObservationKind::AuraApplied
    ) {
        match rules {
            Rules::Retail => {
                if kind == PlayerObservationKind::CastSucceeded
                    && let ActiveKind::Raid(raid) = &mut active.kind
                {
                    update_boss_status(raid, false, name, spell_name);
                }
                let allow_new =
                    matches!(active.kind, ActiveKind::Battleground { .. }) || is_unit_self(flags);
                let (combatants, player_guid) = roster_mut(active);
                let index =
                    process_combatant(combatants, player_guid, guid, name, flags, allow_new);
                if kind == PlayerObservationKind::CastSucceeded
                    && matches!(active.kind, ActiveKind::Battleground { .. })
                    && let Some(combatant) =
                        index.and_then(|i| roster_mut(active).0.entries.get_mut(i))
                    && combatant.spec_id.is_none()
                    && let Some(spec) = retail_unique_spec(spell_name)
                {
                    combatant.spec_id = Some(spec);
                }
            }
            Rules::Classic => {
                let already_know = roster(active).0.contains(guid);
                let Some(index) = process_classic_combatant(
                    active,
                    guid,
                    name,
                    flags,
                    target_guid,
                    target_name,
                    target_flags,
                ) else {
                    return;
                };
                // First enemy spotted in an arena: the gates just opened, so the
                // activity start moves to this event.
                if matches!(active.kind, ActiveKind::Arena(_)) && !already_know {
                    let target = roster_mut(active).0;
                    let is_enemy = target
                        .entries
                        .get(index)
                        .is_some_and(|combatant| combatant.team_id == Some(0));
                    if is_enemy {
                        let enemies = target
                            .iter()
                            .filter(|combatant| combatant.team_id == Some(0))
                            .count();
                        if enemies == 1 {
                            active.started_at_ms = at_ms;
                        }
                    }
                }
                let combatant = &mut roster_mut(active).0.entries[index];
                if combatant.spec_id.is_none() {
                    let spec = if kind == PlayerObservationKind::CastSucceeded {
                        classic_unique_spec(spell_name)
                    } else {
                        classic_unique_aura(spell_name)
                    };
                    if spec.is_some() {
                        combatant.spec_id = spec;
                    }
                }
            }
            Rules::Era => {
                let (combatants, player_guid) = roster_mut(active);
                let index = process_combatant(combatants, player_guid, guid, name, flags, false);
                if kind == PlayerObservationKind::CastSucceeded
                    && let Some(combatant) =
                        index.and_then(|i| roster_mut(active).0.entries.get_mut(i))
                    && combatant.spec_id.is_none()
                    && let Some(spec) = classic_unique_spec(spell_name)
                {
                    combatant.spec_id = Some(spec);
                }
            }
        }
    }
}

fn process_classic_combatant(
    active: &mut ActiveActivity,
    guid: &str,
    name: &str,
    flags: u64,
    target_guid: &str,
    target_name: &str,
    target_flags: u64,
) -> Option<usize> {
    let (combatants, _) = roster(active);
    let src_identified = combatants.contains(guid);
    let dest_identified = combatants.contains(target_guid);
    if matches!(active.kind, ActiveKind::Arena(_))
        && !is_unit_self(flags)
        && !src_identified
        && !dest_identified
    {
        // Arena combatants are only identified by interaction with an already
        // identified unit, crawling out from the player.
        return None;
    }
    let (combatants, player_guid) = roster_mut(active);
    if src_identified && !dest_identified {
        process_combatant(
            combatants,
            player_guid,
            target_guid,
            target_name,
            target_flags,
            true,
        );
    }
    let index = process_combatant(combatants, player_guid, guid, name, flags, true)?;
    // Classic has no team IDs; friendly units are assigned team 1.
    let team = if is_unit_friendly(flags) { 1 } else { 0 };
    if let Some(combatant) = combatants.entries.get_mut(index) {
        combatant.team_id = Some(team);
    }
    update_arena_category(active);
    Some(index)
}

#[allow(clippy::too_many_arguments)]
fn handle_unit_died(
    state: &mut FlavorState,
    rules: Rules,
    guid: &str,
    name: &str,
    flags: u64,
    unconscious: bool,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if !is_unit_player(flags) || unconscious {
        return;
    }
    let friendly = is_unit_friendly(flags);
    if friendly {
        active.meter.death(guid, name, at_ms);
    }
    if is_unit_self(flags) {
        active.meter.host_died();
    }
    let relative = relative_ms(active.started_at_ms, at_ms - DEATH_MARKER_BACK_OFFSET_MS);
    let (plain_name, _, _) = ambiguate(name);
    let outcome = win_loss(!friendly);

    if matches!(active.kind, ActiveKind::SoloShuffle(_)) {
        let started_at_ms = active.started_at_ms;
        let mut items = Vec::new();
        if let ActiveKind::SoloShuffle(shuffle) = &mut active.kind {
            let round_index = shuffle.rounds.len().saturating_sub(1);
            let mut decided = false;
            if let Some(round) = shuffle.rounds.last_mut()
                && !round.has_death
            {
                // The first player death of a round decides it; later
                // deaths in the round are dropped entirely.
                let player_team = round
                    .player_guid
                    .as_ref()
                    .and_then(|guid| round.combatants.get(guid))
                    .and_then(|player| player.team_id);
                if let Some(player_team) = player_team {
                    let winning_team = if !friendly {
                        player_team
                    } else if player_team == 0 {
                        1
                    } else {
                        0
                    };
                    round.has_death = true;
                    round.end_ms = Some(at_ms);
                    round.result = player_team == winning_team;
                    round.item_emitted = true;
                    decided = true;
                }
            }
            if decided {
                if let Some(round) = shuffle.rounds.last() {
                    items.push(round_span(started_at_ms, round_index, round));
                }
                items.push(TimelineItem::point(
                    TimelineKind::Death,
                    relative,
                    Some(plain_name),
                    Some(outcome),
                    None,
                ));
            }
        }
        active.timeline.extend(items);
        return;
    }

    active.timeline.push(TimelineItem::point(
        TimelineKind::Death,
        relative,
        Some(plain_name),
        Some(outcome),
        None,
    ));

    if rules == Rules::Classic && matches!(active.kind, ActiveKind::Arena(_)) {
        process_classic_arena_death(state, at_ms, config, finished, actions);
    }
}

fn process_classic_arena_death(
    state: &mut FlavorState,
    at_ms: i64,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    let Some(active) = state.active.as_ref() else {
        return;
    };
    let mut total_friends = 0usize;
    let mut total_enemies = 0usize;
    for combatant in active.combatants.iter() {
        if combatant.team_id == Some(1) {
            total_friends += 1;
        } else {
            total_enemies += 1;
        }
    }
    let dead_friends = death_count(active, true);
    if total_friends.saturating_sub(dead_friends) < 1 {
        end_on_death_count(state, at_ms, config, finished, actions);
        return;
    }
    let dead_enemies = death_count(active, false);
    if total_enemies.saturating_sub(dead_enemies) < 1 {
        end_on_death_count(state, at_ms, config, finished, actions);
    }
}

fn handle_boss_health(
    state: &mut FlavorState,
    rules: Rules,
    name: &str,
    current: u64,
    maximum: u64,
) {
    if rules != Rules::Retail {
        return;
    }
    let Some(active) = state.active.as_mut() else {
        return;
    };
    let ActiveKind::Raid(raid) = &mut active.kind else {
        return;
    };
    if !raid.boss_unit_active {
        return;
    }
    if !raid.boss_unit_name.is_empty() {
        if name != raid.boss_unit_name {
            return;
        }
        raid.max_hp = maximum;
        raid.current_hp = current;
        return;
    }
    // Below 100M max HP the unit is assumed not to be a boss (retail only).
    if maximum < MIN_RETAIL_BOSS_HP {
        return;
    }
    if maximum < raid.max_hp {
        return;
    }
    raid.max_hp = maximum;
    raid.current_hp = current;
}

fn handle_boss_cast(state: &mut FlavorState, rules: Rules, source_name: &str, spell_name: &str) {
    if rules != Rules::Retail {
        return;
    }
    let Some(active) = state.active.as_mut() else {
        return;
    };
    let ActiveKind::Raid(raid) = &mut active.kind else {
        return;
    };
    update_boss_status(raid, true, source_name, spell_name);
}

/// Belo'ren (and future similar bosses) only count damage in the egg phase,
/// bracketed by `Rebirth` cast start/success.
fn update_boss_status(
    raid: &mut RaidState,
    cast_started: bool,
    source_name: &str,
    spell_name: &str,
) {
    if source_name == BELOREN_UNIT_NAME && spell_name == BELOREN_PHASE_SPELL {
        raid.boss_unit_active = cast_started;
    }
}

// --- Finishing ---

fn finish(
    mut active: ActiveActivity,
    ended_at_ms: i64,
    end: EndKind,
    config: &ActivitySettings,
    finished: &mut Vec<RecordingDraft>,
    actions: &mut Vec<ActivityAction>,
) {
    finalize_open_items(&mut active);
    let outcome = match end {
        EndKind::Complete(outcome) => outcome,
        EndKind::Abandon => abandon_outcome(&active),
    };
    if matches!(end, EndKind::Abandon) {
        active.overrun_ms = 0;
    }

    // Metadata completeness: the recording player must be identified with a
    // named combatant, and zone-based activities need a nonzero zone;
    // otherwise metadata cannot be built and the video is dropped.
    let zone_ok = match &active.kind {
        ActiveKind::Arena(arena) => arena.zone_id != 0,
        ActiveKind::Battleground { zone_id } => *zone_id != 0,
        ActiveKind::SoloShuffle(shuffle) => shuffle.zone_id != 0,
        ActiveKind::Challenge(challenge) => challenge.zone_id != 0,
        ActiveKind::Raid(_) => true,
    };
    let draft = build_draft(active, outcome, ended_at_ms);
    let id = draft.id.clone();
    let action = if draft.player.is_none() || !zone_ok {
        ActivityAction::Discard {
            id,
            reason: DiscardReason::IncompleteMetadata,
        }
    } else if draft.category == Category::Raids
        && (draft.duration_ms.unwrap_or_default() as i64)
            < i64::from(config.min_raid_duration_seconds) * 1_000
    {
        ActivityAction::Discard {
            id,
            reason: DiscardReason::BelowMinDuration,
        }
    } else {
        match end {
            EndKind::Complete(_) => ActivityAction::Complete { id },
            EndKind::Abandon => ActivityAction::Abandon { id },
        }
    };
    finished.push(draft);
    actions.push(action);
}

fn abandon_outcome(active: &ActiveActivity) -> Outcome {
    match &active.kind {
        ActiveKind::Challenge(_) => Outcome::Abandoned,
        ActiveKind::Battleground { .. } => battleground_estimate(active),
        _ => Outcome::Loss,
    }
}

/// Close any open challenge segment as zero length and emit unstarted or
/// unended solo-shuffle rounds as points.
fn finalize_open_items(active: &mut ActiveActivity) {
    let started_at_ms = active.started_at_ms;
    let mut items = Vec::new();
    match &mut active.kind {
        ActiveKind::Challenge(challenge) => {
            if let Some(segment) = challenge.segments.last_mut()
                && segment.end_ms.is_none()
            {
                segment.end_ms = Some(segment.start_ms);
                items.push(segment_item(started_at_ms, segment));
            }
        }
        ActiveKind::SoloShuffle(shuffle) => {
            for (index, round) in shuffle.rounds.iter_mut().enumerate() {
                if !round.item_emitted {
                    round.item_emitted = true;
                    items.push(round_point(started_at_ms, index, round));
                }
            }
        }
        _ => {}
    }
    for item in items {
        active.timeline.push(item);
    }
}

fn player_summary(active: &ActiveActivity) -> Option<PlayerSummary> {
    let (combatants, guid) = roster(active);
    let combatant = combatants.get(guid?)?;
    Some(PlayerSummary {
        name: combatant.name.clone()?,
        realm: combatant.realm.clone(),
        guid: Some(combatant.guid.clone()),
        spec_id: combatant.spec_id,
    })
}

fn combatant_summaries(active: &ActiveActivity) -> Vec<CombatantSummary> {
    // Solo shuffle records only the combatants from the final round, and
    // battlegrounds record none at all (the player is still required).
    if matches!(active.kind, ActiveKind::Battleground { .. }) {
        return Vec::new();
    }
    roster(active)
        .0
        .iter()
        .map(|combatant| CombatantSummary {
            name: combatant.name.clone(),
            realm: combatant.realm.clone(),
            guid: Some(combatant.guid.clone()),
            region: combatant.region.clone(),
            spec_id: combatant.spec_id,
            team_id: combatant.team_id,
        })
        .collect()
}

fn build_draft(active: ActiveActivity, outcome: Outcome, ended_at_ms: i64) -> RecordingDraft {
    let duration_ms = relative_ms(active.started_at_ms, ended_at_ms) + active.overrun_ms;
    let player = player_summary(&active);
    let combatants = combatant_summaries(&active);
    let activity_hash = activity_hash(&active, outcome);
    let title = title_for(&active, outcome, player.as_ref());
    let details = build_details(&active);
    let mut timeline = active.timeline.clone();
    timeline.sort_by_key(|item| item.start_ms());
    // Draining resolves pet ownership and bounds rows; unlabelled fights
    // (raid/arena/battleground) take the activity title.
    let names = combatant_names(&active);
    let meter = active
        .meter
        .drain(ended_at_ms, active.started_at_ms, &title, &names);
    RecordingDraft {
        id: active.id.clone(),
        category: active.category.clone(),
        flavor: active.flavor.clone(),
        started_at_ms: active.started_at_ms,
        overrun_ms: active.overrun_ms,
        details,
        player,
        combatants,
        timeline,
        outcome: Some(outcome),
        ended_at_ms: Some(ended_at_ms),
        duration_ms: Some(duration_ms),
        title: Some(title),
        activity_hash: Some(activity_hash),
        meter,
    }
}

/// GUID-to-name map for meter rows whose own events never named them (pet
/// owners, supporters), from the same combatant map the summaries use. Names
/// are put back in the log's `Name-Realm-Region` form, so a player reads the
/// same in every fight.
fn combatant_names(active: &ActiveActivity) -> HashMap<String, String> {
    roster(active)
        .0
        .iter()
        .filter_map(|combatant| {
            let mut name = combatant.name.clone()?;
            for part in [&combatant.realm, &combatant.region].into_iter().flatten() {
                name.push('-');
                name.push_str(part);
            }
            Some((combatant.guid.clone(), name))
        })
        .collect()
}

fn build_details(active: &ActiveActivity) -> ActivityDetails {
    match &active.kind {
        ActiveKind::Raid(raid) => {
            let boss_percent = if raid.max_hp == 0 {
                100
            } else {
                ((100.0 * raid.current_hp as f64) / raid.max_hp as f64)
                    .round()
                    .min(100.0) as u8
            };
            ActivityDetails::Raid {
                zone_id: Some(raid_zone_id(raid.encounter_id)),
                zone_name: Some(raid_lookup(raid.encounter_id).short_name.to_string()),
                encounter_id: Some(raid.encounter_id),
                encounter_name: Some(raid.encounter_name.clone()),
                difficulty_id: Some(raid.difficulty_id),
                difficulty: difficulty_info(raid.difficulty_id).map(|info| info.short.to_string()),
                pull: None,
                boss_percent: Some(boss_percent),
            }
        }
        ActiveKind::Challenge(challenge) => ActivityDetails::Dungeon {
            zone_id: Some(challenge.zone_id),
            dungeon_name: Some(dungeon_name(
                &active.flavor,
                challenge.zone_id,
                challenge.map_id,
            )),
            map_id: Some(challenge.map_id),
            keystone_level: Some(challenge.level),
            affixes: challenge.affixes.clone(),
            upgrade_level: Some(upgrade_level(active, challenge)),
        },
        ActiveKind::Arena(arena) => ActivityDetails::ArenaOrBattleground {
            map_id: Some(arena.zone_id),
            map_name: arena_zone_name(&active.flavor, arena.zone_id),
            team_mmr: None,
        },
        ActiveKind::Battleground { zone_id } => ActivityDetails::ArenaOrBattleground {
            map_id: Some(*zone_id),
            map_name: Some(battleground_name(*zone_id).to_string()),
            team_mmr: None,
        },
        ActiveKind::SoloShuffle(shuffle) => {
            let started_at_ms = active.started_at_ms;
            let rounds: Vec<RoundSummary> = shuffle
                .rounds
                .iter()
                .enumerate()
                .map(|(index, round)| RoundSummary {
                    round: (index + 1) as u32,
                    outcome: win_loss(round.result),
                    start_ms: relative_ms(started_at_ms, round.start_ms),
                    duration_ms: round
                        .end_ms
                        .map(|end| end.saturating_sub(round.start_ms).max(0) as u64),
                })
                .collect();
            let rounds_won = rounds
                .iter()
                .filter(|round| round.outcome == Outcome::Win)
                .count() as u8;
            ActivityDetails::SoloRounds {
                map_id: Some(shuffle.zone_id),
                map_name: arena_zone_name(&active.flavor, shuffle.zone_id),
                rounds_won: Some(rounds_won),
                rounds_played: Some(rounds.len() as u8),
                rounds,
            }
        }
    }
}

/// Keystone upgrade from the challenge duration, compared against the raw
/// table values: retail tables are seconds, classic MoP tables are minutes (so
/// a completed classic run always scores +3).
fn upgrade_level(active: &ActiveActivity, challenge: &ChallengeState) -> u8 {
    let timers = if active.flavor == GameFlavor::Classic {
        mop_challenge_mode_timers(challenge.map_id)
    } else {
        dungeon_timers(challenge.map_id)
    };
    let Some(timers) = timers else {
        return 0;
    };
    let cm_duration_ms = challenge.cm_duration_ms.unwrap_or(0);
    if cm_duration_ms == 0 && active.flavor == GameFlavor::Retail {
        // Run didn't complete (abandoned, not a deplete).
        return 0;
    }
    let mut effective_ms = cm_duration_ms as i64;
    if challenge.affixes.contains(&CHALLENGERS_PERIL_AFFIX) {
        effective_ms -= CHALLENGERS_PERIL_ADJUST_MS;
    }
    let duration_for_result = effective_ms as f64 / 1_000.0;
    for (index, timer) in timers.iter().enumerate().rev() {
        if duration_for_result <= *timer {
            return (index + 1) as u8;
        }
    }
    0
}

fn title_for(active: &ActiveActivity, outcome: Outcome, player: Option<&PlayerSummary>) -> String {
    let base = match &active.kind {
        ActiveKind::Raid(raid) => {
            let lookup = raid_lookup(raid.encounter_id);
            let difficulty = difficulty_info(raid.difficulty_id)
                .map(|info| info.short)
                .unwrap_or("");
            let result_text = if outcome == Outcome::Win {
                "Kill"
            } else {
                "Wipe"
            };
            let encounter = format!("{} [{}] ({})", raid.encounter_name, difficulty, result_text);
            if lookup.name == "Unknown Raid" {
                encounter
            } else {
                format!("{}, {}", lookup.name, encounter)
            }
        }
        ActiveKind::Challenge(challenge) => {
            let name = dungeon_name(&active.flavor, challenge.zone_id, challenge.map_id);
            let result_text = if outcome == Outcome::Complete {
                format!("+{}", upgrade_level(active, challenge))
            } else {
                "Abandoned".to_string()
            };
            format!("{} +{} ({})", name, challenge.level, result_text)
        }
        ActiveKind::Arena(arena) => {
            let category_text = match active.category {
                Category::TwoVTwo => "2v2",
                Category::ThreeVThree => "3v3",
                Category::FiveVFive => "5v5",
                _ => "Skirmish",
            };
            // An unknown zone interpolates as "undefined" in the title.
            let zone = arena_zone_name(&active.flavor, arena.zone_id)
                .unwrap_or_else(|| "undefined".to_string());
            let result_text = if outcome == Outcome::Win {
                "Win"
            } else {
                "Loss"
            };
            format!("{} {} ({})", category_text, zone, result_text)
        }
        ActiveKind::Battleground { zone_id } => {
            let result_text = if outcome == Outcome::Win {
                "Win"
            } else {
                "Loss"
            };
            format!("{} ({})", battleground_name(*zone_id), result_text)
        }
        ActiveKind::SoloShuffle(shuffle) => {
            let zone = arena_zone_name(&active.flavor, shuffle.zone_id)
                .unwrap_or_else(|| "undefined".to_string());
            let won = shuffle.rounds.iter().filter(|round| round.result).count();
            let lost = shuffle.rounds.len() - won;
            format!("Solo Shuffle {} ({}-{})", zone, won, lost)
        }
    };
    match player {
        Some(player) => format!("{} - {}", player.name, base),
        None => base,
    }
}

/// MD5 of category, flavour, result and the sorted combatant names,
/// concatenated without a separator before the names. Solo shuffle hashes no
/// names (its activity-level map stays empty).
fn activity_hash(active: &ActiveActivity, outcome: Outcome) -> String {
    let category = category_hash_name(&active.category);
    let flavor = match active.flavor {
        GameFlavor::Retail => "Retail",
        _ => "Classic",
    };
    let result = match outcome {
        Outcome::Win | Outcome::Complete => "true",
        _ => "false",
    };
    let mut names: Vec<String> = active
        .combatants
        .iter()
        .filter_map(|combatant| combatant.name.clone())
        .filter(|name| !name.is_empty())
        .collect();
    // JS default sort orders by UTF-16 code units.
    names.sort_by_key(|name| name.encode_utf16().collect::<Vec<u16>>());
    let input = format!("{} {} {}{}", category, flavor, result, names.join(" "));
    md5_hex(input.as_bytes())
}

fn category_hash_name(category: &Category) -> &'static str {
    match category {
        Category::TwoVTwo => "2v2",
        Category::ThreeVThree => "3v3",
        Category::FiveVFive => "5v5",
        Category::Skirmish => "Skirmish",
        Category::SoloShuffle => "Solo Shuffle",
        Category::MythicPlus => "Mythic+",
        Category::Raids => "Raids",
        Category::Battlegrounds => "Battlegrounds",
        Category::Manual => "Manual",
        Category::Clip => "Clips",
    }
}

fn round_point(started_at_ms: i64, index: usize, round: &ShuffleRound) -> TimelineItem {
    TimelineItem::point(
        TimelineKind::Round,
        relative_ms(started_at_ms, round.start_ms),
        Some(format!("Round {}", index + 1)),
        Some(win_loss(round.result)),
        None,
    )
}

fn round_span(started_at_ms: i64, index: usize, round: &ShuffleRound) -> TimelineItem {
    let start = relative_ms(started_at_ms, round.start_ms);
    let end = relative_ms(started_at_ms, round.end_ms.unwrap_or(round.start_ms)).max(start);
    TimelineItem::span(
        TimelineKind::Round,
        start,
        end,
        Some(format!("Round {}", index + 1)),
        Some(win_loss(round.result)),
        None,
    )
    .expect("clamped span bounds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RaidDifficulty;

    const SELF_FLAGS: u64 = 0x511;
    const FRIENDLY_FLAGS: u64 = 0x512;
    const ENEMY_FLAGS: u64 = 0x548;

    /// Test fixture bundling the engine with the flavor and settings every
    /// event needs, so each feed is a single call instead of a five-argument
    /// `handle` sprawl.
    struct Engine {
        engine: ActivityEngine,
        config: ActivitySettings,
        flavor: GameFlavor,
    }

    impl Engine {
        fn new(flavor: GameFlavor) -> Self {
            Self {
                engine: ActivityEngine::new(),
                config: ActivitySettings::default(),
                flavor,
            }
        }

        fn feed(&mut self, at_ms: i64, event: CombatEvent) -> Vec<ActivityAction> {
            self.engine.handle(
                ParsedEvent {
                    flavor: self.flavor.clone(),
                    occurred_at_ms: at_ms,
                    event,
                },
                &self.config,
            )
        }

        fn take_finished(&mut self, id: &RecordingId) -> Option<RecordingDraft> {
            self.engine.take_finished(id)
        }

        fn force_end(&mut self, flavor: GameFlavor, occurred_at_ms: i64) -> Vec<ActivityAction> {
            self.engine.force_end(flavor, occurred_at_ms)
        }

        /// The in-flight activity's timeline, in push order.
        fn timeline(&self) -> &[TimelineItem] {
            let state = match self.flavor {
                GameFlavor::Retail => &self.engine.retail,
                GameFlavor::Classic => &self.engine.classic,
                _ => &self.engine.era,
            };
            state
                .active
                .as_ref()
                .map_or(&[], |active| active.timeline.as_slice())
        }
    }

    fn encounter_start(encounter_id: u32, name: &str, difficulty_id: u32) -> CombatEvent {
        CombatEvent::EncounterStarted {
            encounter_id,
            name: name.to_string(),
            difficulty_id,
        }
    }

    fn encounter_end(difficulty_id: u32, success: bool) -> CombatEvent {
        CombatEvent::EncounterEnded {
            difficulty_id,
            success,
        }
    }

    fn combatant(guid: &str, team_id: Option<u8>, spec_id: Option<u16>) -> CombatEvent {
        CombatEvent::Combatant {
            guid: guid.to_string(),
            team_id,
            spec_id,
        }
    }

    fn cast(guid: &str, name: &str, flags: u64, spell: &str) -> CombatEvent {
        cast_at(guid, name, flags, EMPTY_GUID, "nil", 0, spell)
    }

    fn bloodlust_cast(guid: &str, name: &str, flags: u64) -> CombatEvent {
        let mut event = cast(guid, name, flags, "Fury of the Aspects");
        let CombatEvent::PlayerObserved { spell_id, .. } = &mut event else {
            unreachable!();
        };
        *spell_id = 390386;
        event
    }

    #[allow(clippy::too_many_arguments)]
    fn cast_at(
        guid: &str,
        name: &str,
        flags: u64,
        target_guid: &str,
        target_name: &str,
        target_flags: u64,
        spell: &str,
    ) -> CombatEvent {
        CombatEvent::PlayerObserved {
            kind: PlayerObservationKind::CastSucceeded,
            aura_type: None,
            spell_id: 0,
            guid: guid.to_string(),
            name: name.to_string(),
            flags,
            target_guid: target_guid.to_string(),
            target_name: target_name.to_string(),
            target_flags,
            spell_name: spell.to_string(),
            owner_guid: None,
        }
    }

    fn died(guid: &str, name: &str, flags: u64) -> CombatEvent {
        CombatEvent::UnitDied {
            guid: guid.to_string(),
            name: name.to_string(),
            flags,
            unconscious: false,
        }
    }

    fn begins(actions: &[ActivityAction]) -> usize {
        actions
            .iter()
            .filter(|action| matches!(action, ActivityAction::Begin { .. }))
            .count()
    }

    #[test]
    fn bloodlust_cast_adds_one_40_second_span_to_the_active_recording() {
        let mut engine = Engine::new(GameFlavor::Retail);
        let start = 100_000;
        let begin = engine.feed(
            start,
            CombatEvent::ChallengeStarted {
                zone_id: 2286,
                map_id: 377,
                level: 10,
                affixes: vec![],
            },
        );
        assert_eq!(begins(&begin), 1);

        for _ in 0..2 {
            engine.feed(
                start + 12_345,
                bloodlust_cast("Player-1-A", "Evoker-Realm", FRIENDLY_FLAGS),
            );
        }
        // The duplicate cast adds nothing.
        assert_eq!(
            engine.timeline(),
            [TimelineItem::span(
                TimelineKind::Bloodlust,
                12_345,
                52_345,
                Some("Fury of the Aspects".to_owned()),
                None,
                None,
            )
            .unwrap()]
        );
    }

    #[test]
    fn retail_raid_kill_produces_golden_metadata() {
        let mut engine = Engine::new(GameFlavor::Retail);

        let actions = engine.feed(100_000, encounter_start(2587, "Eranog", 16));
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft } = &actions[0] else {
            panic!("expected Begin");
        };
        assert_eq!(draft.started_at_ms, 100_000);
        assert_eq!(draft.category, Category::Raids);
        assert_eq!(draft.overrun_ms, RAID_DEFAULT_OVERRUN_MS);
        assert!(draft.timeline.is_empty());
        let id = draft.id.clone();

        engine.feed(100_500, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            100_600,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        engine.feed(110_000, died("Player-2-A", "Beta-Realm", FRIENDLY_FLAGS));
        assert_eq!(
            engine.timeline(),
            [TimelineItem::point(
                TimelineKind::Death,
                9_998,
                Some("Beta".to_string()),
                Some(Outcome::Loss),
                None,
            )]
        );

        let end = engine.feed(130_000, encounter_end(16, true));
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);

        let finished = engine.take_finished(&id).expect("finished draft");
        assert_eq!(finished.outcome, Some(Outcome::Win));
        assert_eq!(finished.ended_at_ms, Some(130_000));
        // 30 s of combat plus the configured 15 s kill overrun.
        assert_eq!(finished.overrun_ms, 15_000);
        assert_eq!(finished.duration_ms, Some(45_000));
        assert_eq!(
            finished.title.as_deref(),
            Some("Alpha - Vault of the Incarnates, Eranog [M] (Kill)")
        );
        assert_eq!(
            finished.activity_hash.as_deref(),
            Some("159d8df5e1ef99d5f039d31f01dd4706")
        );
        assert_eq!(
            finished.player,
            Some(PlayerSummary {
                name: "Alpha".to_string(),
                realm: Some("Realm".to_string()),
                guid: Some("Player-1-A".to_string()),
                spec_id: Some(71),
            })
        );
        assert_eq!(finished.combatants.len(), 1);
        assert_eq!(finished.timeline.len(), 1);
        assert_eq!(
            finished.details,
            ActivityDetails::Raid {
                zone_id: Some(14030),
                zone_name: Some("Vault".to_string()),
                encounter_id: Some(2587),
                encounter_name: Some("Eranog".to_string()),
                difficulty_id: Some(16),
                difficulty: Some("M".to_string()),
                pull: None,
                boss_percent: Some(100),
            }
        );
        assert!(engine.take_finished(&id).is_none());
    }

    #[test]
    fn short_raid_wipe_is_discarded() {
        let mut engine = Engine::new(GameFlavor::Retail);

        let actions = engine.feed(0, encounter_start(2587, "Eranog", 16));
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        // 5 s wipe + 3 s default overrun = 8 s < the 15 s minimum.
        let end = engine.feed(5_000, encounter_end(16, false));
        assert_eq!(
            end,
            vec![ActivityAction::Discard {
                id: id.clone(),
                reason: DiscardReason::BelowMinDuration,
            }]
        );
        assert_eq!(
            engine.take_finished(&id).unwrap().outcome,
            Some(Outcome::Loss)
        );
    }

    #[test]
    fn raid_without_identified_player_is_discarded() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.feed(0, encounter_start(2587, "Eranog", 16));
        let end = engine.feed(60_000, encounter_end(16, true));
        assert!(matches!(
            end.as_slice(),
            [ActivityAction::Discard {
                reason: DiscardReason::IncompleteMetadata,
                ..
            }]
        ));
    }

    #[test]
    fn raid_below_min_difficulty_is_ignored() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.config.min_raid_difficulty = RaidDifficulty::Heroic;
        let actions = engine.feed(0, encounter_start(2587, "Eranog", 17));
        assert!(actions.is_empty());
    }

    #[test]
    fn disabled_category_never_begins() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.config.record_raids = false;
        let start = engine.feed(0, encounter_start(2587, "Eranog", 16));
        let end = engine.feed(60_000, encounter_end(16, true));
        assert!(start.is_empty() && end.is_empty());
    }

    #[test]
    fn midnight_season_two_content_is_recordable() {
        for (zone_id, map_id) in [
            (2521, 399),
            (2813, 587),
            (2825, 586),
            (2859, 584),
            (2923, 585),
            (2993, 588),
            (1877, 250),
            (1762, 249),
        ] {
            let mut engine = Engine::new(GameFlavor::Retail);
            engine.config.current_raid_only = true;
            let actions = engine.feed(
                0,
                CombatEvent::ChallengeStarted {
                    zone_id,
                    map_id,
                    level: 10,
                    affixes: Vec::new(),
                },
            );
            assert_eq!(begins(&actions), 1, "map {map_id} was not recordable");
        }

        for encounter_id in [3470, 3445, 3455, 3497, 3420, 3421, 3429, 3492, 3379, 3513] {
            let mut engine = Engine::new(GameFlavor::Retail);
            engine.config.current_raid_only = true;
            let actions = engine.feed(0, encounter_start(encounter_id, "Midnight Season 2", 16));
            assert_eq!(
                begins(&actions),
                1,
                "raid encounter {encounter_id} was not recordable"
            );
        }
    }

    #[test]
    fn mythic_plus_completion_builds_segments_and_upgrade() {
        let mut engine = Engine::new(GameFlavor::Retail);

        let actions = engine.feed(
            0,
            CombatEvent::ChallengeStarted {
                zone_id: 2526,
                map_id: 402,
                level: 10,
                affixes: vec![9, 152],
            },
        );
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        assert_eq!(draft.category, Category::MythicPlus);

        engine.feed(1_000, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            1_100,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );

        // Boss pull at 60 s closes the opening trash segment.
        engine.feed(60_000, encounter_start(2562, "Vexamus", 8));
        assert_eq!(
            engine.timeline(),
            [TimelineItem::span(TimelineKind::Trash, 0, 60_000, None, None, None).unwrap()]
        );
        engine.feed(120_000, encounter_end(8, true));
        assert_eq!(
            engine.timeline()[1],
            TimelineItem::span(
                TimelineKind::Encounter,
                60_000,
                120_000,
                Some("Vexamus".to_string()),
                Some(Outcome::Win),
                None
            )
            .unwrap()
        );

        // End 125 s later: the trailing 5 s trash segment is dropped.
        let end = engine.feed(
            125_000,
            CombatEvent::ChallengeEnded {
                success: true,
                duration_ms: 1_400_000,
            },
        );
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Complete));
        assert_eq!(finished.ended_at_ms, Some(125_000));
        assert_eq!(finished.overrun_ms, 5_000);
        assert_eq!(finished.timeline.len(), 2);
        // 1400 s minus the 90 s Challenger's Peril adjustment beats the 1488 s
        // two-chest timer for map 402.
        assert_eq!(
            finished.details,
            ActivityDetails::Dungeon {
                zone_id: Some(2526),
                dungeon_name: Some("Algeth'ar Academy".to_string()),
                map_id: Some(402),
                keystone_level: Some(10),
                affixes: vec![9, 152],
                upgrade_level: Some(2),
            }
        );
        assert_eq!(
            finished.title.as_deref(),
            Some("Alpha - Algeth'ar Academy +10 (+2)")
        );
    }

    #[test]
    fn mythic_plus_abandon_records_abandoned_outcome() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.feed(
            0,
            CombatEvent::ChallengeStarted {
                zone_id: 2526,
                map_id: 402,
                level: 10,
                affixes: vec![9],
            },
        );
        engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        let end = engine.feed(
            600_000,
            CombatEvent::ChallengeEnded {
                success: false,
                duration_ms: 0,
            },
        );
        let [ActivityAction::Complete { id }] = end.as_slice() else {
            panic!("expected Complete, got {end:?}");
        };
        let finished = engine.take_finished(id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Abandoned));
        assert_eq!(finished.overrun_ms, 0);
        assert!(matches!(
            finished.details,
            ActivityDetails::Dungeon {
                upgrade_level: Some(0),
                ..
            }
        ));
        assert_eq!(
            finished.title.as_deref(),
            Some("Alpha - Algeth'ar Academy +10 (Abandoned)")
        );
    }

    #[test]
    fn raid_encounter_over_mythic_plus_hands_off() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.feed(
            0,
            CombatEvent::ChallengeStarted {
                zone_id: 2526,
                map_id: 402,
                level: 10,
                affixes: vec![9],
            },
        );
        engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        let handoff = engine.feed(60_000, encounter_start(2587, "Eranog", 16));
        let [
            ActivityAction::Abandon { id },
            ActivityAction::Begin { draft },
        ] = handoff.as_slice()
        else {
            panic!("expected Abandon, Begin, got {handoff:?}");
        };
        assert_eq!(draft.category, Category::Raids);
        let abandoned = engine.take_finished(id).unwrap();
        assert_eq!(abandoned.outcome, Some(Outcome::Abandoned));
        assert_eq!(abandoned.ended_at_ms, Some(60_000));
    }

    #[test]
    fn retail_key_ends_on_coming_back_to_a_reset_dungeon() {
        let zone = |zone_id, difficulty_id| CombatEvent::ZoneChanged {
            zone_id,
            difficulty_id,
        };
        let mut engine = Engine::new(GameFlavor::Retail);
        let begin = engine.feed(
            0,
            CombatEvent::ChallengeStarted {
                zone_id: 2526,
                map_id: 402,
                level: 10,
                affixes: vec![9],
            },
        );
        let ActivityAction::Begin { draft } = &begin[0] else {
            panic!("expected Begin, got {begin:?}");
        };
        let id = draft.id.clone();
        engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        // Walking out of a running key and back in keeps it going.
        assert!(engine.feed(60_000, zone(2444, 0)).is_empty());
        assert!(engine.feed(75_000, zone(2526, 8)).is_empty());
        assert!(engine.feed(300_000, zone(2444, 0)).is_empty());
        let end = engine.feed(320_000, zone(2526, 23));
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Abandoned));
        assert_eq!(finished.ended_at_ms, Some(320_000));
    }

    #[test]
    fn retail_key_survives_a_boss_missing_from_the_table() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.feed(
            0,
            CombatEvent::ChallengeStarted {
                zone_id: 2526,
                map_id: 402,
                level: 10,
                affixes: vec![9],
            },
        );
        let pull = engine.feed(
            60_000,
            CombatEvent::EncounterStarted {
                encounter_id: 4_000_000,
                name: "New Boss".to_string(),
                difficulty_id: KEYSTONE_DIFFICULTY_ID,
            },
        );
        assert!(pull.is_empty(), "the key was ended: {pull:?}");
    }

    #[test]
    fn retail_arena_win_and_loss() {
        for (winning_team, expected) in [(0u32, Outcome::Win), (1u32, Outcome::Loss)] {
            let mut engine = Engine::new(GameFlavor::Retail);
            let actions = engine.feed(
                0,
                CombatEvent::ArenaStarted {
                    zone_id: 1672,
                    match_type: "2v2".to_string(),
                },
            );
            let ActivityAction::Begin { draft, .. } = &actions[0] else {
                panic!("expected Begin");
            };
            let id = draft.id.clone();
            assert_eq!(draft.category, Category::TwoVTwo);
            engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
            engine.feed(
                200,
                cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
            );
            let end = engine.feed(
                240_000,
                CombatEvent::ArenaEnded {
                    winning_team_id: winning_team,
                },
            );
            assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
            let result_text = if expected == Outcome::Win {
                "Win"
            } else {
                "Loss"
            };
            let finished = engine.take_finished(&id).unwrap();
            assert_eq!(finished.outcome, Some(expected));
            assert_eq!(
                finished.title.as_deref(),
                Some(format!("Alpha - 2v2 Blade's Edge ({result_text})").as_str())
            );
        }
    }

    #[test]
    fn solo_shuffle_rounds_and_completion() {
        let mut engine = Engine::new(GameFlavor::Retail);
        let start = CombatEvent::ArenaStarted {
            zone_id: 1672,
            match_type: "Rated Solo Shuffle".to_string(),
        };

        let actions = engine.feed(0, start.clone());
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();

        engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        // Enemy death decides round one as a win: round span plus death point.
        engine.feed(30_000, died("Player-9-B", "Foe-Realm", ENEMY_FLAGS));
        let decided = [
            TimelineItem::span(
                TimelineKind::Round,
                0,
                30_000,
                Some("Round 1".to_string()),
                Some(Outcome::Win),
                None,
            )
            .unwrap(),
            TimelineItem::point(
                TimelineKind::Death,
                29_998,
                Some("Foe".to_string()),
                Some(Outcome::Win),
                None,
            ),
        ];
        assert_eq!(engine.timeline(), decided);
        // A second death in the same round is dropped entirely.
        engine.feed(31_000, died("Player-8-B", "Ally-Realm", FRIENDLY_FLAGS));
        assert_eq!(engine.timeline(), decided);

        // Round two: no duplicate Begin, fresh round roster.
        let round_two = engine.feed(60_000, start);
        assert_eq!(begins(&round_two), 0);
        engine.feed(60_100, combatant("Player-1-A", Some(1), Some(71)));
        engine.feed(
            60_200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );

        let end = engine.feed(90_000, CombatEvent::ArenaEnded { winning_team_id: 0 });
        // The undecided round two is kept as a point, and the game completes
        // as a win.
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);

        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Win));
        let round_two = finished.timeline.last().unwrap();
        assert_eq!(round_two.kind(), &TimelineKind::Round);
        assert_eq!(round_two.label(), Some("Round 2"));
        assert_eq!(
            finished.title.as_deref(),
            Some("Alpha - Solo Shuffle Blade's Edge (1-1)")
        );
        let ActivityDetails::SoloRounds {
            rounds_won,
            rounds_played,
            rounds,
            ..
        } = &finished.details
        else {
            panic!("expected SoloRounds");
        };
        assert_eq!(*rounds_won, Some(1));
        assert_eq!(*rounds_played, Some(2));
        assert_eq!(
            rounds
                .iter()
                .map(|round| (round.round, round.outcome))
                .collect::<Vec<_>>(),
            vec![(1, Outcome::Win), (2, Outcome::Loss)]
        );
    }

    #[test]
    fn retail_battleground_estimates_result_from_deaths() {
        let mut engine = Engine::new(GameFlavor::Retail);
        let actions = engine.feed(
            0,
            CombatEvent::ZoneChanged {
                zone_id: 30,
                difficulty_id: 0,
            },
        );
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        assert_eq!(draft.category, Category::Battlegrounds);

        engine.feed(
            100,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        for (at_ms, guid, name, flags) in [
            (10_000, "Player-2-A", "Beta-Realm", FRIENDLY_FLAGS),
            (11_000, "Player-3-A", "Gamma-Realm", FRIENDLY_FLAGS),
            (12_000, "Player-9-B", "Foe-Realm", ENEMY_FLAGS),
        ] {
            engine.feed(at_ms, died(guid, name, flags));
        }
        let end = engine.feed(
            600_000,
            CombatEvent::ZoneChanged {
                zone_id: 1,
                difficulty_id: 0,
            },
        );
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Loss));
        assert_eq!(finished.ended_at_ms, Some(600_000));
        assert!(finished.combatants.is_empty());
        assert_eq!(finished.player.as_ref().map(|p| p.spec_id), Some(Some(71)));
        assert_eq!(
            finished.title.as_deref(),
            Some("Alpha - Alterac Valley (Loss)")
        );
    }

    #[test]
    fn classic_raid_kill() {
        let mut engine = Engine::new(GameFlavor::Classic);
        let actions = engine.feed(0, encounter_start(1107, "Anub'Rekhan", 9));
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        engine.feed(100, combatant("Player-1-A", None, None));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        let end = engine.feed(60_000, encounter_end(9, true));
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Win));
        assert_eq!(finished.flavor, GameFlavor::Classic);
        assert_eq!(finished.player.as_ref().map(|p| p.spec_id), Some(Some(71)));
        assert_eq!(
            finished.title.as_deref(),
            Some("Alpha - Naxxramas, Anub'Rekhan [40] (Kill)")
        );
    }

    #[test]
    fn classic_arena_death_driven_end() {
        let mut engine = Engine::new(GameFlavor::Classic);
        let actions = engine.feed(
            0,
            CombatEvent::ZoneChanged {
                zone_id: 559,
                difficulty_id: 0,
            },
        );
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        assert_eq!(draft.category, Category::TwoVTwo);

        engine.feed(
            1_000,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        // Enemies become known by interacting with the identified player.
        for (at_ms, guid, name) in [
            (5_000, "Player-9-B", "Foe-Realm"),
            (6_000, "Player-8-B", "Bane-Realm"),
        ] {
            engine.feed(
                at_ms,
                cast_at(
                    guid,
                    name,
                    ENEMY_FLAGS,
                    "Player-1-A",
                    "Alpha-Realm",
                    SELF_FLAGS,
                    "Mortal Strike",
                ),
            );
        }
        engine.feed(30_000, died("Player-9-B", "Foe-Realm", ENEMY_FLAGS));
        let end = engine.feed(40_000, died("Player-8-B", "Bane-Realm", ENEMY_FLAGS));
        // Second enemy death empties their team.
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Win));
        assert_eq!(finished.ended_at_ms, Some(40_000));
        // First enemy sighting at 5 s restarted the activity clock.
        assert_eq!(finished.started_at_ms, 5_000);
        assert_eq!(finished.combatants.len(), 3);
    }

    #[test]
    fn classic_challenge_mode_completes() {
        let mut engine = Engine::new(GameFlavor::Classic);
        let actions = engine.feed(
            0,
            CombatEvent::ChallengeStarted {
                zone_id: 994,
                map_id: 60,
                level: 1,
                affixes: Vec::new(),
            },
        );
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        engine.feed(100, combatant("Player-1-A", None, None));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        let end = engine.feed(
            900_000,
            CombatEvent::ChallengeEnded {
                success: false,
                duration_ms: 900_000,
            },
        );
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Complete));
        assert_eq!(
            finished.details,
            ActivityDetails::Dungeon {
                zone_id: Some(994),
                dungeon_name: Some("Mogu'shan Palace".to_string()),
                map_id: Some(60),
                keystone_level: Some(0),
                affixes: Vec::new(),
                upgrade_level: Some(3),
            }
        );
    }

    #[test]
    fn era_raid_records_classic_flavor() {
        let mut engine = Engine::new(GameFlavor::Era);
        let actions = engine.feed(0, encounter_start(1107, "Anub'Rekhan", 9));
        assert_eq!(begins(&actions), 1);
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        assert_eq!(draft.flavor, GameFlavor::Classic);
        engine.feed(100, combatant("Player-1-A", None, None));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        let end = engine.feed(60_000, encounter_end(9, true));
        assert_eq!(end, vec![ActivityAction::Complete { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Win));
        assert_eq!(finished.flavor, GameFlavor::Classic);
    }

    #[test]
    fn interleaved_flavors_stay_independent() {
        let mut engine = Engine::new(GameFlavor::Retail);
        engine.feed(0, encounter_start(2587, "Eranog", 16));
        // A classic battleground begins its own activity.
        engine.flavor = GameFlavor::Classic;
        let classic = engine.feed(
            1_000,
            CombatEvent::ZoneChanged {
                zone_id: 30,
                difficulty_id: 0,
            },
        );
        assert_eq!(begins(&classic), 1);
        engine.flavor = GameFlavor::Unknown("ptr_x".to_string());
        let unknown = engine.feed(2_000, encounter_end(16, true));
        assert!(unknown.is_empty());
        // The retail raid is still in flight and only retail can end it.
        assert!(engine.force_end(GameFlavor::Era, 3_000).is_empty());
        let ended = engine.force_end(GameFlavor::Retail, 120_000);
        assert!(matches!(
            ended.as_slice(),
            [ActivityAction::Discard {
                reason: DiscardReason::IncompleteMetadata,
                ..
            }]
        ));
    }

    #[test]
    fn force_end_emits_final_action_once() {
        let mut engine = Engine::new(GameFlavor::Retail);
        assert!(engine.force_end(GameFlavor::Retail, 0).is_empty());
        let actions = engine.feed(0, encounter_start(2587, "Eranog", 16));
        let ActivityAction::Begin { draft, .. } = &actions[0] else {
            panic!("expected Begin");
        };
        let id = draft.id.clone();
        engine.feed(100, combatant("Player-1-A", Some(0), Some(71)));
        engine.feed(
            200,
            cast("Player-1-A", "Alpha-Realm", SELF_FLAGS, "Mortal Strike"),
        );
        let ended = engine.force_end(GameFlavor::Retail, 120_000);
        assert_eq!(ended, vec![ActivityAction::Abandon { id: id.clone() }]);
        let finished = engine.take_finished(&id).unwrap();
        assert_eq!(finished.outcome, Some(Outcome::Loss));
        assert_eq!(finished.ended_at_ms, Some(120_000));
        assert_eq!(finished.overrun_ms, 0);
        assert_eq!(finished.duration_ms, Some(120_000));
        assert!(engine.force_end(GameFlavor::Retail, 130_000).is_empty());
    }

    #[test]
    fn md5_matches_reference_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }
}
