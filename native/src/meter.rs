// SPDX-License-Identifier: GPL-3.0-or-later

//! Damage-meter aggregation for one automatic activity.
//!
//! Rows are keyed by the raw source GUID; pet-to-owner attribution is resolved
//! once at `drain`, so ownership discovered after a pet's first damage still
//! merges retroactively. The ownership map is activity-scoped and dropped with
//! the accumulator. No spell IDs are involved anywhere.

use std::collections::{BTreeMap, HashMap};

use crate::activity::{
    is_player_controlled_friendly, is_unit_friendly, is_unit_player, is_unit_self, relative_ms,
};
use crate::domain::{
    MeterActor, MeterData, MeterDeath, MeterDeathEvent, MeterDeathEventKind, MeterEntry,
    MeterFight, MeterMetric, MeterSample,
};

/// Maximum spell or target rows kept per actor per metric; the remainder folds
/// into a single "Other" row so totals stay exact.
const MAX_BREAKDOWN_ROWS: usize = 16;
/// Persisted playback resolution for meter samples and UI refreshes.
pub const SAMPLE_INTERVAL_MS: u64 = 500;

/// Total hostile-damage silence separating Mythic+ trash pulls. Chain pulls
/// inside this window remain one fight, matching the host staying in combat.
const PULL_GAP_MS: i64 = 6_000;
/// Events kept per unit for the death log breakdown.
const DEATH_LOG_EVENTS: usize = 20;
/// A cast row above one per second of fight, over at least this many casts, is
/// the game's own book-keeping rather than a human pressing a button.
const HIDDEN_CAST_FLOOR: u32 = 20;
const OTHER_KEY: &str = "Other";
const METRICS: [MeterMetric; 7] = [
    MeterMetric::Damage,
    MeterMetric::DamageTaken,
    MeterMetric::Healing,
    MeterMetric::Interrupts,
    MeterMetric::Dispels,
    MeterMetric::Casts,
    MeterMetric::Buffs,
];

/// Metrics shown as occurrence times rather than per-hit statistics. Buffs'
/// `amount` measures uptime in milliseconds, but `hits` counts applications,
/// which is what the occurrence list lists.
pub fn is_count_metric(metric: MeterMetric) -> bool {
    matches!(
        metric,
        MeterMetric::Interrupts | MeterMetric::Dispels | MeterMetric::Casts | MeterMetric::Buffs
    )
}

/// An empty or all-zero GUID: the log's "no unit".
fn is_nil_guid(guid: &str) -> bool {
    guid.chars().all(|character| character == '0')
}

/// Aura lifecycle for buff-uptime tracking. `SPELL_AURA_BROKEN` is not
/// tracked, so a broken buff's span closes at the fight end instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuffEvent {
    Applied,
    Refreshed,
    Removed,
}

pub(crate) struct MeterAccumulator {
    started_at_ms: i64,
    fights: Vec<RawFight>,
    owners: HashMap<String, OwnedBy>,
    /// Mythic+ trash is split into host-current and ambient group fights.
    segmented: bool,
    /// Lingering MINE effects must not re-engage a dead host in the same pull.
    host_dead: bool,
    death_history: HashMap<String, Vec<RawDeathEvent>>,
    /// Last known HP per unit, sizing the death log bars.
    hp: HashMap<String, UnitHp>,
}

/// `at_ms` marks the event the HP describes, so an event whose own line
/// carried no HP (swings report the source, not the victim) is derived from
/// its amount instead of reusing a stale reading.
struct UnitHp {
    /// HP standing before the event at `at_ms`, which is what overkill is
    /// measured against; the log's own reading is already post-event.
    before: u64,
    current: u64,
    max: u64,
    at_ms: i64,
}

struct OwnedBy {
    guid: String,
    /// Owner display name when the ownership signal carries one (summons).
    name: Option<String>,
}

struct RawFight {
    label: Option<String>,
    start_ms: i64,
    end_ms: i64,
    first_event_ms: Option<i64>,
    last_event_ms: Option<i64>,
    last_bucket_end_ms: i64,
    ambient: bool,
    actors: Vec<RawActor>,
    actor_index: HashMap<String, usize>,
    recent: HashMap<(String, String, MeterMetric), RecentRecord>,
    /// `(buffed guid, buff name)` → application time of the span still open.
    open_buffs: HashMap<(String, String), i64>,
    deaths: Vec<RawDeath>,
}

type SpellTargetKey = (MeterMetric, String, String, u8);

/// The last damage or heal per `(source, target, metric)`; with the map key
/// it names the rows a following support event takes its share from.
struct RecentRecord {
    spell: String,
    marker: u8,
    at_ms: i64,
    bucket_end_ms: i64,
    remaining: u64,
    remaining_overheal: u64,
}

struct RawDeathEvent {
    kind: MeterDeathEventKind,
    at_ms: i64,
    /// Destination HP after the event, 0 when the log never reported it.
    hp: u64,
    /// Damage wasted past zero HP; only the killing blow carries any.
    overkill: u64,
    source_name: String,
    spell_name: String,
    amount: u64,
}

struct RawDeath {
    guid: String,
    max_hp: u64,
    name: String,
    at_ms: i64,
    events: Vec<RawDeathEvent>,
}

impl RawFight {
    fn new(start_ms: i64, label: Option<String>, ambient: bool) -> Self {
        Self {
            label,
            start_ms,
            end_ms: start_ms,
            first_event_ms: None,
            last_event_ms: None,
            last_bucket_end_ms: start_ms,
            ambient,
            actors: Vec::new(),
            actor_index: HashMap::new(),
            recent: HashMap::new(),
            open_buffs: HashMap::new(),
            deaths: Vec::new(),
        }
    }

    fn actor(&mut self, guid: &str, name: &str) -> &mut RawActor {
        if let Some(&position) = self.actor_index.get(guid) {
            let actor = &mut self.actors[position];
            // A support or buff row can precede the unit's own first event.
            if actor.name.is_empty() {
                actor.name = name.to_owned();
            }
            return actor;
        }
        self.actor_index.insert(guid.to_owned(), self.actors.len());
        self.actors.push(RawActor {
            guid: guid.to_owned(),
            name: name.to_owned(),
            spells: HashMap::new(),
            targets: HashMap::new(),
            spell_targets: HashMap::new(),
        });
        self.actors.last_mut().expect("just pushed")
    }
}

struct RawActor {
    guid: String,
    name: String,
    spells: HashMap<(MeterMetric, String), RawEntry>,
    targets: HashMap<(MeterMetric, String, u8), RawEntry>,
    /// `(metric, spell, target, marker)`: the cross tabulation behind the
    /// per-spell target list.
    spell_targets: HashMap<SpellTargetKey, RawEntry>,
}

#[derive(Default)]
struct RawValue {
    amount: u64,
    hits: u32,
    overheal: u64,
    /// Per-hit extremes; `max == 0` while no hit has landed.
    min: u64,
    max: u64,
}

#[derive(Default)]
struct RawEntry {
    total: RawValue,
    samples: BTreeMap<i64, RawValue>,
}

impl RawEntry {
    fn add(&mut self, amount: u64, overheal: u64, bucket_end_ms: i64) {
        for value in [
            &mut self.total,
            self.samples.entry(bucket_end_ms).or_default(),
        ] {
            value.amount += amount;
            value.hits += 1;
            value.overheal += overheal;
            value.min = if value.hits == 1 {
                amount
            } else {
                value.min.min(amount)
            };
            value.max = value.max.max(amount);
        }
    }

    fn add_transfer(&mut self, amount: u64, overheal: u64, bucket_end_ms: i64) {
        self.total.amount += amount;
        self.total.overheal += overheal;
        let sample = self.samples.entry(bucket_end_ms).or_default();
        sample.amount += amount;
        sample.overheal += overheal;
    }

    /// Buff-uptime credit: a measured span, not an event, so no hits or
    /// per-hit extremes — the occurrence list counts applications.
    fn add_uptime(&mut self, amount: u64, bucket_end_ms: i64) {
        self.total.amount += amount;
        if amount > 0 {
            self.samples.entry(bucket_end_ms).or_default().amount += amount;
        }
    }

    fn subtract_transfer(&mut self, amount: u64, overheal: u64, bucket_end_ms: i64) {
        self.total.amount = self.total.amount.saturating_sub(amount);
        self.total.overheal = self.total.overheal.saturating_sub(overheal);
        if let Some(sample) = self.samples.get_mut(&bucket_end_ms) {
            sample.amount = sample.amount.saturating_sub(amount);
            sample.overheal = sample.overheal.saturating_sub(overheal);
        }
    }
}

impl MeterAccumulator {
    pub(crate) fn new(start_ms: i64, initial_label: Option<String>) -> Self {
        Self {
            started_at_ms: start_ms,
            fights: vec![RawFight::new(start_ms, initial_label, false)],
            owners: HashMap::new(),
            segmented: false,
            host_dead: false,
            death_history: HashMap::new(),
            hp: HashMap::new(),
        }
    }

    /// Retail Mythic+ starts outside the capturing player's combat while still
    /// retaining the group's opening damage for Overall.
    pub(crate) fn trash(start_ms: i64) -> Self {
        Self {
            fights: vec![RawFight::new(start_ms, Some("Trash".to_owned()), true)],
            segmented: true,
            ..Self::new(start_ms, None)
        }
    }

    fn open_fight(&mut self) -> &mut RawFight {
        self.fights
            .last_mut()
            .expect("the open fight is always present")
    }

    /// Record pet-to-owner attribution. `owner_name` is carried when the
    /// signal names it (summons); otherwise the owner is named at drain from
    /// the combatant map.
    pub(crate) fn record_owner(
        &mut self,
        pet_guid: &str,
        owner_guid: &str,
        owner_name: Option<&str>,
    ) {
        if pet_guid.is_empty() || owner_guid.is_empty() || pet_guid == owner_guid {
            return;
        }
        // Every pet event re-reports its owner; a later unnamed signal must
        // not drop the name a summon carried.
        if let Some(owned) = self.owners.get_mut(pet_guid)
            && owned.guid == owner_guid
        {
            if owned.name.is_none() {
                owned.name = owner_name.map(str::to_owned);
            }
            return;
        }
        self.owners.insert(
            pet_guid.to_owned(),
            OwnedBy {
                guid: owner_guid.to_owned(),
                name: owner_name.map(str::to_owned),
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn damage(
        &mut self,
        source_guid: &str,
        source_name: &str,
        source_flags: u64,
        dest_guid: &str,
        dest_name: &str,
        dest_flags: u64,
        marker: u8,
        spell_name: &str,
        amount: u64,
        at_ms: i64,
    ) {
        self.observe_damage(at_ms, source_flags, dest_flags);
        if is_unit_friendly(dest_flags) && is_unit_player(dest_flags) {
            self.remember_death_event(
                dest_guid,
                MeterDeathEventKind::Damage,
                source_name,
                spell_name,
                amount,
                at_ms,
            );
            self.record(
                dest_guid,
                dest_name,
                MeterMetric::DamageTaken,
                spell_name,
                source_name,
                0,
                amount,
                0,
                at_ms,
            );
        }
        // Known summons include CONTROL_NPC guardians whose flags are not
        // friendly. Their owner was accepted only from a friendly
        // player-controlled summoner.
        if (!is_player_controlled_friendly(source_flags) && !self.owners.contains_key(source_guid))
            || is_unit_friendly(dest_flags)
        {
            return;
        }
        self.record(
            source_guid,
            source_name,
            MeterMetric::Damage,
            spell_name,
            dest_name,
            marker,
            amount,
            0,
            at_ms,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn heal(
        &mut self,
        source_guid: &str,
        source_name: &str,
        source_flags: u64,
        dest_guid: &str,
        dest_name: &str,
        dest_flags: u64,
        marker: u8,
        spell_name: &str,
        amount: u64,
        overheal: u64,
        at_ms: i64,
    ) {
        self.observe_heal(at_ms, source_flags);
        let effective = amount.saturating_sub(overheal);
        if is_unit_friendly(dest_flags) && is_unit_player(dest_flags) {
            self.remember_death_event(
                dest_guid,
                MeterDeathEventKind::Healing,
                source_name,
                spell_name,
                effective,
                at_ms,
            );
        }
        if !is_player_controlled_friendly(source_flags) {
            return;
        }
        // Healing Done is effective healing; overheal is retained separately.
        self.record(
            source_guid,
            source_name,
            MeterMetric::Healing,
            spell_name,
            dest_name,
            marker,
            effective,
            overheal,
            at_ms,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn interrupt(
        &mut self,
        source_guid: &str,
        source_name: &str,
        source_flags: u64,
        dest_name: &str,
        marker: u8,
        spell_name: &str,
        at_ms: i64,
    ) {
        if !is_player_controlled_friendly(source_flags) {
            return;
        }
        self.record(
            source_guid,
            source_name,
            MeterMetric::Interrupts,
            spell_name,
            dest_name,
            marker,
            1,
            0,
            at_ms,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispel(
        &mut self,
        source_guid: &str,
        source_name: &str,
        source_flags: u64,
        dest_name: &str,
        marker: u8,
        spell_name: &str,
        at_ms: i64,
    ) {
        if !is_player_controlled_friendly(source_flags) {
            return;
        }
        self.record(
            source_guid,
            source_name,
            MeterMetric::Dispels,
            spell_name,
            dest_name,
            marker,
            1,
            0,
            at_ms,
        );
    }

    /// One successful cast by a friendly player. Pets and guardians are
    /// excluded: their casts are the game acting, not a button press, and
    /// merging them into their owner is what made the row unreadable. Casts
    /// are counted, never timed: they land outside combat too, so they must
    /// not widen the fight's DPS window.
    pub(crate) fn cast(
        &mut self,
        source_guid: &str,
        source_name: &str,
        source_flags: u64,
        spell_name: &str,
        at_ms: i64,
    ) {
        if !is_unit_player(source_flags) || !is_unit_friendly(source_flags) {
            return;
        }
        // An area effect re-logs its owner's cast from a nil source carrying
        // the owner's own flags; counting it doubles the button press.
        if is_nil_guid(source_guid) {
            return;
        }
        self.record(
            source_guid,
            source_name,
            MeterMetric::Casts,
            spell_name,
            "",
            0,
            1,
            0,
            at_ms,
        );
    }

    /// Move support-contributed damage or effective healing from the actor who
    /// produced the base event to the supporting player. Support rows follow
    /// their base event in the combat log; a missing base is ignored.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn support(
        &mut self,
        metric: MeterMetric,
        supporter_guid: &str,
        source_guid: &str,
        dest_name: &str,
        marker: u8,
        support_spell: &str,
        amount: u64,
        overheal: u64,
        at_ms: i64,
    ) {
        if supporter_guid.is_empty() || supporter_guid == source_guid {
            return;
        }
        let effective = if metric == MeterMetric::Healing {
            amount.saturating_sub(overheal)
        } else {
            amount
        };
        let fight = self.open_fight();
        let Some(&source_index) = fight.actor_index.get(source_guid) else {
            return;
        };
        let key = (source_guid.to_owned(), dest_name.to_owned(), metric);
        let Some(recent) = fight.recent.get_mut(&key) else {
            return;
        };
        if at_ms.saturating_sub(recent.at_ms) > 1_000 {
            return;
        }
        let transferred = effective.min(recent.remaining);
        let transferred_overheal = overheal.min(recent.remaining_overheal);
        if transferred == 0 && transferred_overheal == 0 {
            return;
        }
        recent.remaining -= transferred;
        recent.remaining_overheal -= transferred_overheal;
        let bucket_end_ms = recent.bucket_end_ms;
        {
            let source = &mut fight.actors[source_index];
            let target = (metric, dest_name.to_owned(), recent.marker);
            if let Some(entry) = source.targets.get_mut(&target) {
                entry.subtract_transfer(transferred, transferred_overheal, bucket_end_ms);
            }
            let spell_target = (metric, recent.spell.clone(), target.1, recent.marker);
            if let Some(entry) = source.spell_targets.get_mut(&spell_target) {
                entry.subtract_transfer(transferred, transferred_overheal, bucket_end_ms);
            }
            if let Some(entry) = source.spells.get_mut(&(metric, spell_target.1)) {
                entry.subtract_transfer(transferred, transferred_overheal, bucket_end_ms);
            }
        }
        let supporter = fight.actor(supporter_guid, "");
        supporter
            .spells
            .entry((metric, support_spell.to_owned()))
            .or_default()
            .add_transfer(transferred, transferred_overheal, bucket_end_ms);
        supporter
            .targets
            .entry((metric, dest_name.to_owned(), marker))
            .or_default()
            .add_transfer(transferred, transferred_overheal, bucket_end_ms);
        supporter
            .spell_targets
            .entry((
                metric,
                support_spell.to_owned(),
                dest_name.to_owned(),
                marker,
            ))
            .or_default()
            .add_transfer(transferred, transferred_overheal, bucket_end_ms);
    }

    /// HP left after an event, plus the damage wasted past zero. The log's own
    /// reading is used when this event carried one, otherwise the previous
    /// reading moves by the event's amount. Overkill is the shortfall against
    /// the HP standing before the hit, which matches the log's own field.
    fn hp_after(
        &mut self,
        guid: &str,
        kind: MeterDeathEventKind,
        amount: u64,
        at_ms: i64,
    ) -> (u64, u64) {
        let Some(hp) = self.hp.get_mut(guid) else {
            return (0, 0);
        };
        let before = if hp.at_ms == at_ms {
            hp.before
        } else {
            hp.current
        };
        let overkill = match kind {
            MeterDeathEventKind::Damage => amount.saturating_sub(before),
            MeterDeathEventKind::Healing => 0,
        };
        if hp.at_ms != at_ms {
            hp.before = hp.current;
            hp.current = match kind {
                MeterDeathEventKind::Damage => hp.current.saturating_sub(amount),
                MeterDeathEventKind::Healing => (hp.current + amount).min(hp.max),
            };
            hp.at_ms = at_ms;
        }
        (hp.current, overkill)
    }

    fn remember_death_event(
        &mut self,
        guid: &str,
        kind: MeterDeathEventKind,
        source_name: &str,
        spell_name: &str,
        amount: u64,
        at_ms: i64,
    ) {
        if guid.is_empty() || amount == 0 {
            return;
        }
        let (hp, overkill) = self.hp_after(guid, kind, amount, at_ms);
        let event = RawDeathEvent {
            kind,
            at_ms,
            hp,
            overkill,
            source_name: source_name.to_owned(),
            spell_name: spell_name.to_owned(),
            amount,
        };
        if let Some(events) = self.death_history.get_mut(guid) {
            if events.len() == DEATH_LOG_EVENTS {
                events.remove(0);
            }
            events.push(event);
        } else {
            self.death_history.insert(guid.to_owned(), vec![event]);
        }
    }

    /// Advanced-block destination HP, which the log reports after the event.
    pub(crate) fn note_hp(&mut self, guid: &str, current: u64, max: u64, at_ms: i64) {
        if guid.is_empty() || max == 0 {
            return;
        }
        if let Some(hp) = self.hp.get_mut(guid) {
            hp.before = hp.current;
            hp.current = current;
            hp.max = max;
            hp.at_ms = at_ms;
        } else {
            self.hp.insert(
                guid.to_owned(),
                UnitHp {
                    before: current,
                    current,
                    max,
                    at_ms,
                },
            );
        }
    }

    pub(crate) fn death(&mut self, guid: &str, name: &str, at_ms: i64) {
        let events = self.death_history.remove(guid).unwrap_or_default();
        let max_hp = self.hp.get(guid).map_or(0, |hp| hp.max);
        self.open_fight().deaths.push(RawDeath {
            guid: guid.to_owned(),
            max_hp,
            name: name.to_owned(),
            at_ms,
            events,
        });
    }

    /// Close the open fight and start a fixed Current segment such as a boss
    /// encounter or arena round.
    pub(crate) fn cut(&mut self, at_ms: i64, new_label: String) {
        self.segmented = false;
        self.host_dead = false;
        self.death_history.clear();
        self.begin_fight(at_ms, Some(new_label), false);
    }

    /// A boss encounter ended; group damage remains in Overall until the host
    /// joins the next trash pull.
    pub(crate) fn cut_to_trash(&mut self, at_ms: i64) {
        self.segmented = true;
        self.host_dead = false;
        self.death_history.clear();
        self.begin_fight(at_ms, Some("Trash".to_owned()), true);
    }

    /// A dead host no longer produces fresh engagement signals, but the
    /// group's open fight continues until hostile damage stops.
    pub(crate) fn host_died(&mut self) {
        if self.segmented {
            self.host_dead = true;
        }
    }

    fn begin_fight(&mut self, at_ms: i64, label: Option<String>, ambient: bool) {
        // Spans close first: their credit bucket can round past `at_ms`, and
        // a fight must not end before the sample its uptime lands in.
        self.close_open_buffs(at_ms);
        let fight = self.open_fight();
        fight.end_ms = at_ms.max(fight.last_bucket_end_ms);
        self.fights.push(RawFight::new(at_ms, label, ambient));
    }

    /// Hostile damage is the only reliable combat-state signal in the saved
    /// log. Group-wide silence splits pulls; MINE on either side starts Current.
    fn observe_damage(&mut self, at_ms: i64, source_flags: u64, dest_flags: u64) {
        if !self.segmented || is_unit_friendly(source_flags) == is_unit_friendly(dest_flags) {
            return;
        }
        let (ambient, separated) = {
            let fight = self.open_fight();
            (
                fight.ambient,
                fight
                    .last_event_ms
                    .is_some_and(|last| at_ms.saturating_sub(last) > PULL_GAP_MS),
            )
        };
        // A new pull or an enemy hitting the host proves that stale effects
        // from the death boundary no longer describe the host's combat state.
        if separated || is_unit_self(dest_flags) {
            self.host_dead = false;
        }
        let mine = !self.host_dead && (is_unit_self(source_flags) || is_unit_self(dest_flags));
        // Only a real combat gap ends the fight a death belongs to; the host
        // joining an ongoing pull splits the fight but keeps its history.
        if separated {
            self.death_history.clear();
        }
        if separated || mine && ambient {
            self.begin_fight(at_ms, Some("Trash".to_owned()), !mine);
        }
        let fight = self.open_fight();
        fight.first_event_ms = Some(fight.first_event_ms.map_or(at_ms, |first| first.min(at_ms)));
        fight.last_event_ms = Some(fight.last_event_ms.map_or(at_ms, |last| last.max(at_ms)));
    }

    /// Healing a party member already fighting engages a healer host. Healing
    /// alone never opens or extends a pull, avoiding downtime inflation.
    fn observe_heal(&mut self, at_ms: i64, source_flags: u64) {
        if !self.segmented || self.host_dead || !is_unit_self(source_flags) {
            return;
        }
        let last_event_ms = self
            .fights
            .last()
            .filter(|fight| fight.ambient)
            .and_then(|fight| fight.last_event_ms)
            .filter(|last| at_ms.saturating_sub(*last) <= PULL_GAP_MS);
        if let Some(last_event_ms) = last_event_ms {
            self.begin_fight(at_ms, Some("Trash".to_owned()), false);
            self.open_fight().last_event_ms = Some(last_event_ms);
        }
    }

    /// Close the open fight at the activity end, resolve ownership, bound the
    /// breakdown rows, and produce the persisted shape. `names` maps GUIDs to
    /// combatant names for owners that never appear as sources themselves.
    pub(crate) fn drain(
        mut self,
        ended_at_ms: i64,
        started_at_ms: i64,
        fallback_label: &str,
        names: &HashMap<String, String>,
    ) -> MeterData {
        self.close_open_buffs(ended_at_ms);
        let fight = self.open_fight();
        fight.end_ms = ended_at_ms.max(fight.last_bucket_end_ms);
        MeterData {
            fights: self
                .fights
                .iter()
                .map(|fight| fight.finish(started_at_ms, fallback_label, &self.owners, names))
                .collect(),
        }
    }

    /// The 500 ms playback bucket an event at `at_ms` lands in.
    fn bucket_end(&self, at_ms: i64) -> i64 {
        let interval_ms = SAMPLE_INTERVAL_MS as i64;
        let elapsed_ms = at_ms.saturating_sub(self.started_at_ms).max(1);
        self.started_at_ms + elapsed_ms.saturating_add(interval_ms - 1) / interval_ms * interval_ms
    }

    /// Credit every still-open buff span to the fight it was applied in. A
    /// buff outliving its fight stops there; the log re-logs nothing at the
    /// boundary, so carrying it over would need state the next fight has no
    /// row for.
    fn close_open_buffs(&mut self, at_ms: i64) {
        let bucket_end_ms = self.bucket_end(at_ms);
        let fight = self.open_fight();
        if !fight.open_buffs.is_empty() {
            fight.last_bucket_end_ms = fight.last_bucket_end_ms.max(bucket_end_ms);
        }
        for ((guid, spell), opened) in std::mem::take(&mut fight.open_buffs) {
            fight
                .actor(&guid, "")
                .spells
                .entry((MeterMetric::Buffs, spell))
                .or_default()
                .add_uptime(at_ms.saturating_sub(opened).max(0) as u64, bucket_end_ms);
        }
    }

    /// BUFF-uptime bookkeeping for a friendly player. Applications count as
    /// occurrences (the UI lists when each landed); each removal credits its
    /// span's milliseconds. A refresh re-logs the application without
    /// restarting the span, matching how the aura persists.
    pub(crate) fn buff(
        &mut self,
        event: BuffEvent,
        dest_guid: &str,
        dest_name: &str,
        dest_flags: u64,
        spell_name: &str,
        at_ms: i64,
    ) {
        if !is_unit_player(dest_flags) || !is_unit_friendly(dest_flags) {
            return;
        }
        if is_nil_guid(dest_guid) {
            return;
        }
        match event {
            BuffEvent::Removed => self.close_buff(dest_guid, spell_name, at_ms),
            BuffEvent::Applied | BuffEvent::Refreshed => {
                // A re-application without a removal means the log dropped
                // one; closing first keeps the measured span exact. A
                // refresh without an open span (applied before the activity
                // or before the last boundary) starts measuring from the
                // first evidence this fight has.
                let open = if event == BuffEvent::Applied {
                    self.close_buff(dest_guid, spell_name, at_ms);
                    true
                } else {
                    !self
                        .open_fight()
                        .open_buffs
                        .contains_key(&(dest_guid.to_owned(), spell_name.to_owned()))
                };
                self.record(
                    dest_guid,
                    dest_name,
                    MeterMetric::Buffs,
                    spell_name,
                    "",
                    0,
                    0,
                    0,
                    at_ms,
                );
                if open {
                    self.open_fight()
                        .open_buffs
                        .insert((dest_guid.to_owned(), spell_name.to_owned()), at_ms);
                }
            }
        }
    }

    /// Credit an open buff's `[opened, at_ms)` span and forget it. Removals
    /// without an open span (applied before the activity) are ignored.
    fn close_buff(&mut self, dest_guid: &str, spell_name: &str, at_ms: i64) {
        let bucket_end_ms = self.bucket_end(at_ms);
        let fight = self.open_fight();
        fight.last_bucket_end_ms = fight.last_bucket_end_ms.max(bucket_end_ms);
        let Some(opened) = fight
            .open_buffs
            .remove(&(dest_guid.to_owned(), spell_name.to_owned()))
        else {
            return;
        };
        fight
            .actor(dest_guid, "")
            .spells
            .entry((MeterMetric::Buffs, spell_name.to_owned()))
            .or_default()
            .add_uptime(at_ms.saturating_sub(opened).max(0) as u64, bucket_end_ms);
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        source_guid: &str,
        source_name: &str,
        metric: MeterMetric,
        spell_name: &str,
        dest_name: &str,
        marker: u8,
        amount: u64,
        overheal: u64,
        at_ms: i64,
    ) {
        let bucket_end_ms = self.bucket_end(at_ms);
        let segmented = self.segmented;
        let fight = self.open_fight();
        // Casts and buffs land outside combat too, so they must not widen
        // the fight's DPS window the way a damaging event does.
        if !segmented
            && !matches!(
                metric,
                MeterMetric::DamageTaken | MeterMetric::Casts | MeterMetric::Buffs
            )
        {
            fight.first_event_ms =
                Some(fight.first_event_ms.map_or(at_ms, |first| first.min(at_ms)));
            fight.last_event_ms = Some(fight.last_event_ms.map_or(at_ms, |last| last.max(at_ms)));
        }
        fight.last_bucket_end_ms = fight.last_bucket_end_ms.max(bucket_end_ms);
        let actor = fight.actor(source_guid, source_name);
        actor
            .spells
            .entry((metric, spell_name.to_owned()))
            .or_default()
            .add(amount, overheal, bucket_end_ms);
        // A cast's target says nothing worth a row: most are self or nothing.
        // Buff rows are keyed on the buffed unit; the aura source would only
        // split shared buffs across casters.
        if matches!(metric, MeterMetric::Casts | MeterMetric::Buffs) {
            return;
        }
        actor
            .targets
            .entry((metric, dest_name.to_owned(), marker))
            .or_default()
            .add(amount, overheal, bucket_end_ms);
        actor
            .spell_targets
            .entry((metric, spell_name.to_owned(), dest_name.to_owned(), marker))
            .or_default()
            .add(amount, overheal, bucket_end_ms);
        if matches!(metric, MeterMetric::Damage | MeterMetric::Healing) {
            fight.recent.insert(
                (source_guid.to_owned(), dest_name.to_owned(), metric),
                RecentRecord {
                    spell: spell_name.to_owned(),
                    marker,
                    at_ms,
                    bucket_end_ms,
                    remaining: amount,
                    remaining_overheal: overheal,
                },
            );
        }
    }
}

impl RawFight {
    fn finish(
        &self,
        started_at_ms: i64,
        fallback_label: &str,
        owners: &HashMap<String, OwnedBy>,
        names: &HashMap<String, String>,
    ) -> MeterFight {
        let start = relative_ms(started_at_ms, self.start_ms);
        let end = relative_ms(started_at_ms, self.end_ms).max(start);
        let active_ms = match (self.first_event_ms, self.last_event_ms) {
            (Some(first), Some(last)) => (last - first).max(0) as u64,
            _ => 0,
        };
        MeterFight {
            label: self
                .label
                .clone()
                .unwrap_or_else(|| fallback_label.to_owned()),
            start_ms: start,
            end_ms: end,
            first_event_ms: self
                .first_event_ms
                .map(|at_ms| relative_ms(started_at_ms, at_ms)),
            active_ms,
            ambient: self.ambient,
            actors: merged_actors(self, owners, names, started_at_ms),
            deaths: self
                .deaths
                .iter()
                .map(|death| MeterDeath {
                    guid: death.guid.clone(),
                    max_hp: death.max_hp,
                    name: death.name.clone(),
                    at_ms: relative_ms(started_at_ms, death.at_ms),
                    events: death
                        .events
                        .iter()
                        .map(|event| MeterDeathEvent {
                            kind: event.kind,
                            at_ms: relative_ms(started_at_ms, event.at_ms),
                            source_name: event.source_name.clone(),
                            spell_name: event.spell_name.clone(),
                            amount: event.amount,
                            hp: event.hp,
                            overkill: event.overkill,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Follow the ownership chain (pet of a pet) to the top-level owner.
fn owner_of<'a>(guid: &'a str, owners: &'a HashMap<String, OwnedBy>) -> &'a str {
    let mut current = guid;
    for _ in 0..owners.len() {
        match owners.get(current) {
            Some(owned) if owned.guid != current => current = &owned.guid,
            _ => return current,
        }
    }
    current
}

/// Resolve raw rows to owners and merge, in first-appearance order. Unknown
/// pets (no ownership on record) stay their own rows.
fn merged_actors(
    fight: &RawFight,
    owners: &HashMap<String, OwnedBy>,
    names: &HashMap<String, String>,
    started_at_ms: i64,
) -> Vec<MeterActor> {
    let mut actors: Vec<MeterActor> = Vec::new();
    for raw in &fight.actors {
        let resolved = owner_of(&raw.guid, owners);
        if let Some(actor) = actors.iter_mut().find(|actor| actor.guid == resolved) {
            append_entries(actor, raw, started_at_ms);
        } else {
            let name = if resolved != raw.guid {
                owners
                    .get(&raw.guid)
                    .and_then(|owned| owned.name.clone())
                    .or_else(|| names.get(resolved).cloned())
                    .unwrap_or_else(|| raw.name.clone())
            } else if raw.name.is_empty() {
                // A supporter outside the group is never named by the log.
                names
                    .get(resolved)
                    .cloned()
                    .unwrap_or_else(|| "Unknown".to_owned())
            } else {
                raw.name.clone()
            };
            actors.push(MeterActor {
                guid: resolved.to_owned(),
                name,
                spells: entries(&raw.spells, &raw.spell_targets, started_at_ms),
                targets: entries_targets(&raw.targets, started_at_ms),
            });
        }
    }
    // Hidden book-keeping casts (soul fragment pickups, proc "casts") share
    // SPELL_CAST_SUCCESS with real button presses and the log gives nothing to
    // tell them apart. Nothing a player presses sustains a cast a second for a
    // whole fight, so that rate is the cut.
    // ponytail: rate heuristic; a spell list is the upgrade if it misfires.
    let seconds = (fight.end_ms - fight.start_ms).max(0) as u64 / 1_000;
    for actor in &mut actors {
        actor.spells.retain(|entry| {
            entry.metric != MeterMetric::Casts
                || entry.hits < HIDDEN_CAST_FLOOR
                || u64::from(entry.hits) <= seconds
        });
    }
    for actor in &mut actors {
        actor.spells = bounded(std::mem::take(&mut actor.spells));
        actor.targets = bounded(std::mem::take(&mut actor.targets));
        for spell in &mut actor.spells {
            spell.targets = bounded(std::mem::take(&mut spell.targets));
        }
    }
    actors
}

/// Collect raw spell entries into `MeterEntry`s; spell rows never carry a
/// marker, and each carries its own slice of the target cross tabulation.
fn entries(
    raw: &HashMap<(MeterMetric, String), RawEntry>,
    cross: &HashMap<SpellTargetKey, RawEntry>,
    started_at_ms: i64,
) -> Vec<MeterEntry> {
    raw.iter()
        .map(|((metric, key), entry)| {
            let mut spell = meter_entry(*metric, key.clone(), 0, entry, started_at_ms);
            spell.targets = cross
                .iter()
                .filter(|((cross_metric, spell_name, _, _), _)| {
                    cross_metric == metric && spell_name == key
                })
                .map(|((_, _, target, marker), entry)| {
                    meter_entry(*metric, target.clone(), *marker, entry, started_at_ms)
                })
                .collect();
            spell
        })
        .collect()
}

/// Collect raw target entries, keyed by `(name, marker)`.
fn entries_targets(
    raw: &HashMap<(MeterMetric, String, u8), RawEntry>,
    started_at_ms: i64,
) -> Vec<MeterEntry> {
    raw.iter()
        .map(|((metric, key, marker), entry)| {
            meter_entry(*metric, key.clone(), *marker, entry, started_at_ms)
        })
        .collect()
}

fn meter_entry(
    metric: MeterMetric,
    key: String,
    marker: u8,
    entry: &RawEntry,
    started_at_ms: i64,
) -> MeterEntry {
    MeterEntry {
        metric,
        key,
        marker,
        amount: entry.total.amount,
        hits: entry.total.hits,
        overheal: entry.total.overheal,
        min: entry.total.min,
        max: entry.total.max,
        targets: Vec::new(),
        samples: entry
            .samples
            .iter()
            .map(|(at_ms, value)| MeterSample {
                at_ms: relative_ms(started_at_ms, *at_ms),
                amount: value.amount,
                hits: value.hits,
                overheal: value.overheal,
                min: value.min,
                max: value.max,
            })
            .collect(),
    }
}

fn append_entries(actor: &mut MeterActor, raw: &RawActor, started_at_ms: i64) {
    for spell in entries(&raw.spells, &raw.spell_targets, started_at_ms) {
        append(&mut actor.spells, spell);
    }
    for target in entries_targets(&raw.targets, started_at_ms) {
        append(&mut actor.targets, target);
    }
}

fn append(entries: &mut Vec<MeterEntry>, entry: MeterEntry) {
    if let Some(existing) = entries.iter_mut().find(|existing| {
        existing.metric == entry.metric
            && existing.key == entry.key
            && existing.marker == entry.marker
    }) {
        existing.amount += entry.amount;
        existing.hits += entry.hits;
        existing.overheal += entry.overheal;
        merge_extremes(&mut existing.min, &mut existing.max, entry.min, entry.max);
        merge_samples(&mut existing.samples, &entry.samples);
        for target in entry.targets {
            append(&mut existing.targets, target);
        }
    } else {
        entries.push(entry);
    }
}

/// Combine per-hit extremes; `max == 0` marks an entry that has none, so a
/// zero `min` from such an entry must not win.
fn merge_extremes(min: &mut u64, max: &mut u64, other_min: u64, other_max: u64) {
    if other_max == 0 {
        return;
    }
    *min = if *max == 0 {
        other_min
    } else {
        (*min).min(other_min)
    };
    *max = (*max).max(other_max);
}

fn merge_samples(into: &mut Vec<MeterSample>, from: &[MeterSample]) {
    for sample in from {
        if let Some(existing) = into.iter_mut().find(|item| item.at_ms == sample.at_ms) {
            existing.amount += sample.amount;
            existing.hits += sample.hits;
            existing.overheal += sample.overheal;
            merge_extremes(&mut existing.min, &mut existing.max, sample.min, sample.max);
        } else {
            into.push(sample.clone());
        }
    }
    into.sort_by_key(|sample| sample.at_ms);
}

/// Bound each metric's rows to `MAX_BREAKDOWN_ROWS` largest contributors,
/// folding the remainder into one "Other" row so totals stay exact.
fn bounded(entries: Vec<MeterEntry>) -> Vec<MeterEntry> {
    let mut groups: [Vec<MeterEntry>; METRICS.len()] = Default::default();
    for entry in entries {
        if let Some(index) = METRICS.iter().position(|metric| *metric == entry.metric) {
            groups[index].push(entry);
        }
    }
    let mut result = Vec::new();
    for (metric, mut group) in METRICS.into_iter().zip(groups) {
        group.sort_by(|a, b| {
            b.amount
                .cmp(&a.amount)
                .then_with(|| a.key.cmp(&b.key))
                .then_with(|| a.marker.cmp(&b.marker))
        });
        // Keep the top MAX_BREAKDOWN_ROWS - 1 rows and fold the remainder
        // into one "Other" row, so the list never exceeds MAX_BREAKDOWN_ROWS
        // rows while totals stay exact.
        if group.len() > MAX_BREAKDOWN_ROWS {
            let rest = group.split_off(MAX_BREAKDOWN_ROWS - 1);
            let mut other = MeterEntry {
                metric,
                key: OTHER_KEY.to_owned(),
                marker: 0,
                amount: 0,
                hits: 0,
                overheal: 0,
                min: 0,
                max: 0,
                // A fold of many spells has no single target list worth
                // drilling into.
                targets: Vec::new(),
                samples: Vec::new(),
            };
            for entry in rest {
                other.amount += entry.amount;
                other.hits += entry.hits;
                other.overheal += entry.overheal;
                merge_extremes(&mut other.min, &mut other.max, entry.min, entry.max);
                merge_samples(&mut other.samples, &entry.samples);
            }
            group.push(other);
        }
        result.extend(group);
    }
    result
}

pub struct MeterProjection {
    pub elapsed_ms: u64,
    pub actors: Vec<ProjectedActor>,
    pub deaths: Vec<MeterDeath>,
}

pub struct ProjectedActor {
    pub guid: String,
    pub name: String,
    pub spells: Vec<ProjectedEntry>,
    pub targets: Vec<ProjectedEntry>,
}

pub struct ProjectedEntry {
    pub metric: MeterMetric,
    pub key: String,
    pub marker: u8,
    pub amount: u64,
    pub hits: u32,
    pub overheal: u64,
    /// Per-hit extremes up to the playhead; `max == 0` means the entry carries
    /// none, which is what a sidecar written before spell detail existed does.
    pub min: u64,
    pub max: u64,
    /// This spell's own target split, projected to the same playhead.
    pub targets: Vec<ProjectedEntry>,
    /// Occurrence times up to the playhead, sample-bucket accurate. Filled for
    /// count metrics only; the amount metrics would list every hit.
    pub times: Vec<u64>,
}

pub fn fight_index_at(fights: &[MeterFight], position_ms: u64) -> Option<usize> {
    fights
        .iter()
        .rposition(|fight| !fight.ambient && fight.start_ms <= position_ms)
}

pub fn project_current(fights: &[MeterFight], position_ms: u64) -> Option<MeterProjection> {
    project_fight(
        fights.get(fight_index_at(fights, position_ms)?)?,
        position_ms,
    )
}

pub fn project_overall(fights: &[MeterFight], position_ms: u64) -> MeterProjection {
    let mut projection = MeterProjection {
        elapsed_ms: 0,
        actors: Vec::new(),
        deaths: Vec::new(),
    };
    for fight in fights.iter().filter(|fight| fight.start_ms <= position_ms) {
        let Some(partial) = project_fight(fight, position_ms) else {
            continue;
        };
        projection.elapsed_ms += partial.elapsed_ms;
        for actor in partial.actors {
            if let Some(existing) = projection
                .actors
                .iter_mut()
                .find(|existing| existing.guid == actor.guid)
            {
                merge_projected_entries(&mut existing.spells, actor.spells);
                merge_projected_entries(&mut existing.targets, actor.targets);
            } else {
                projection.actors.push(actor);
            }
        }
        projection.deaths.extend(partial.deaths);
    }
    projection
}

pub fn has_untimed_totals(fights: &[MeterFight]) -> bool {
    fights
        .iter()
        .flat_map(|fight| &fight.actors)
        .flat_map(|actor| actor.spells.iter().chain(&actor.targets))
        .any(|entry| entry.amount > 0 && entry.samples.is_empty())
}

fn project_fight(fight: &MeterFight, position_ms: u64) -> Option<MeterProjection> {
    let limit = position_ms.min(fight.end_ms);
    let elapsed_ms = fight.first_event_ms.map_or(0, |first| {
        limit
            .clamp(first, first.saturating_add(fight.active_ms))
            .saturating_sub(first)
    });
    let actors = fight
        .actors
        .iter()
        .filter_map(|actor| {
            let spells = project_entries(&actor.spells, limit);
            let targets = project_entries(&actor.targets, limit);
            (!spells.is_empty() || !targets.is_empty()).then(|| ProjectedActor {
                guid: actor.guid.clone(),
                name: actor.name.clone(),
                spells,
                targets,
            })
        })
        .collect();
    Some(MeterProjection {
        elapsed_ms,
        actors,
        deaths: fight
            .deaths
            .iter()
            .filter(|death| death.at_ms <= limit)
            .cloned()
            .collect(),
    })
}

fn project_entries(entries: &[MeterEntry], position_ms: u64) -> Vec<ProjectedEntry> {
    entries
        .iter()
        .filter_map(|entry| {
            let mut projected = ProjectedEntry {
                metric: entry.metric,
                key: entry.key.clone(),
                marker: entry.marker,
                amount: 0,
                hits: 0,
                overheal: 0,
                min: 0,
                max: 0,
                targets: project_entries(&entry.targets, position_ms),
                times: Vec::new(),
            };
            for sample in entry
                .samples
                .iter()
                .take_while(|sample| sample.at_ms <= position_ms)
            {
                projected.amount += sample.amount;
                projected.hits += sample.hits;
                projected.overheal += sample.overheal;
                if is_count_metric(entry.metric) {
                    projected
                        .times
                        .extend(std::iter::repeat_n(sample.at_ms, sample.hits as usize));
                }
                merge_extremes(
                    &mut projected.min,
                    &mut projected.max,
                    sample.min,
                    sample.max,
                );
            }
            (projected.hits > 0 || projected.amount > 0 || projected.overheal > 0)
                .then_some(projected)
        })
        .collect()
}

fn merge_projected_entries(into: &mut Vec<ProjectedEntry>, from: Vec<ProjectedEntry>) {
    for entry in from {
        if let Some(existing) = into.iter_mut().find(|existing| {
            existing.metric == entry.metric
                && existing.key == entry.key
                && existing.marker == entry.marker
        }) {
            existing.amount += entry.amount;
            existing.hits += entry.hits;
            existing.overheal += entry.overheal;
            merge_extremes(&mut existing.min, &mut existing.max, entry.min, entry.max);
            existing.times.extend(entry.times);
            existing.times.sort_unstable();
            merge_projected_entries(&mut existing.targets, entry.targets);
        } else {
            into.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{AFFILIATION_MINE, CONTROL_PLAYER, REACTION_FRIENDLY};

    const PLAYER: u64 = CONTROL_PLAYER | REACTION_FRIENDLY;
    const SELF: u64 = PLAYER | AFFILIATION_MINE;
    const ALLY: u64 = PLAYER | 0x2;
    const MOB: u64 = 0x10a48;
    const FRIENDLY_PLAYER: u64 = PLAYER | 0x400;

    // Fixed-actor damage helpers. For hostile destinations the GUID is never
    // consulted, so the name doubles as the GUID.

    /// Player-0-A damaging `dest` with no target marker.
    fn hit(
        meter: &mut MeterAccumulator,
        dest: &str,
        dest_flags: u64,
        spell: &str,
        amount: u64,
        at_ms: i64,
    ) {
        meter.damage(
            "Player-0-A",
            "A",
            PLAYER,
            dest,
            dest,
            dest_flags,
            0,
            spell,
            amount,
            at_ms,
        );
    }

    /// The unowned pet staying its own row.
    fn pet_hit(meter: &mut MeterAccumulator, dest: &str, spell: &str, amount: u64, at_ms: i64) {
        meter.damage(
            "Pet-0-1", "Imp", PLAYER, dest, dest, 0, 0, spell, amount, at_ms,
        );
    }

    /// The host damaging trash or boss targets.
    fn host_hit(meter: &mut MeterAccumulator, dest: &str, spell: &str, amount: u64, at_ms: i64) {
        meter.damage(
            "Player-0-HOST",
            "Host",
            SELF,
            dest,
            dest,
            MOB,
            0,
            spell,
            amount,
            at_ms,
        );
    }

    /// A group ally damaging trash or boss targets.
    fn ally_hit(meter: &mut MeterAccumulator, dest: &str, spell: &str, amount: u64, at_ms: i64) {
        meter.damage(
            "Player-0-ALLY",
            "Ally",
            ALLY,
            dest,
            dest,
            MOB,
            0,
            spell,
            amount,
            at_ms,
        );
    }

    /// A hostile mob hitting a friendly player: the death-log input, whose
    /// victim GUID (keyed by the death log) differs from the display name.
    fn mob_hit(
        meter: &mut MeterAccumulator,
        victim_guid: &str,
        victim_name: &str,
        spell: &str,
        amount: u64,
        at_ms: i64,
    ) {
        meter.damage(
            "Creature-0-MOB",
            "Mob",
            MOB,
            victim_guid,
            victim_name,
            FRIENDLY_PLAYER,
            0,
            spell,
            amount,
            at_ms,
        );
    }

    /// The host healing a group member with "Heal", all effective.
    fn heal_host(meter: &mut MeterAccumulator, dest: &str, amount: u64, at_ms: i64) {
        meter.heal(
            "Player-0-HOST",
            "Host",
            SELF,
            dest,
            dest,
            0,
            0,
            "Heal",
            amount,
            0,
            at_ms,
        );
    }

    /// Player-0-A carrying an aura themselves.
    fn buff(meter: &mut MeterAccumulator, event: BuffEvent, spell: &str, at_ms: i64) {
        meter.buff(event, "Player-0-A", "A", FRIENDLY_PLAYER, spell, at_ms);
    }

    /// The support wiring under test: Player-0-B boosting Player-0-A's
    /// damage on the boss.
    fn support(meter: &mut MeterAccumulator, spell: &str, amount: u64, at_ms: i64) {
        meter.support(
            MeterMetric::Damage,
            "Player-0-B",
            "Player-0-A",
            "Boss",
            0,
            spell,
            amount,
            0,
            at_ms,
        );
    }

    /// Per-hit extremes and the per-spell target split, both limited to the
    /// playhead like every other projected figure.
    #[test]
    fn spell_detail_carries_extremes_and_targets() {
        let mut meter = MeterAccumulator::new(0, None);
        hit(&mut meter, "Boss", 0, "Bolt", 100, 1_000);
        hit(&mut meter, "Add", 0, "Bolt", 40, 1_000);
        hit(&mut meter, "Boss", 0, "Bolt", 300, 4_000);
        let data = meter.drain(5_000, 0, "Fight", &HashMap::new());
        let fight = &data.fights[0];

        let early = project_current(std::slice::from_ref(fight), 2_000).expect("fight at 2s");
        let bolt = &early.actors[0].spells[0];
        assert_eq!(
            (bolt.amount, bolt.hits, bolt.min, bolt.max),
            (140, 2, 40, 100)
        );
        let mut targets: Vec<(&str, u64)> = bolt
            .targets
            .iter()
            .map(|entry| (entry.key.as_str(), entry.amount))
            .collect();
        targets.sort();
        assert_eq!(targets, [("Add", 40), ("Boss", 100)]);

        let late = project_current(std::slice::from_ref(fight), 5_000).expect("fight at 5s");
        let bolt = &late.actors[0].spells[0];
        assert_eq!((bolt.amount, bolt.min, bolt.max), (440, 40, 300));
        assert_eq!(
            bolt.targets
                .iter()
                .find(|entry| entry.key == "Boss")
                .map(|entry| entry.amount),
            Some(400)
        );
    }

    #[test]
    fn pet_damage_before_summon_merges_into_owner() {
        let mut meter = MeterAccumulator::new(0, None);
        pet_hit(&mut meter, "Boss", "Firebolt", 100, 5_000);
        meter.record_owner("Pet-0-1", "Player-0-A", None);
        let mut names = HashMap::new();
        names.insert("Player-0-A".to_owned(), "Warlock".to_owned());
        let data = meter.drain(10_000, 0, "Fight", &names);
        let fight = &data.fights[0];
        assert_eq!(fight.actors.len(), 1);
        assert_eq!(fight.actors[0].guid, "Player-0-A");
        assert_eq!(fight.actors[0].name, "Warlock");
        assert_eq!(fight.actors[0].spells.len(), 1);
        assert_eq!(fight.actors[0].spells[0].key, "Firebolt");
        assert_eq!(fight.actors[0].spells[0].amount, 100);
    }

    #[test]
    fn friendly_fire_damage_is_excluded() {
        let mut meter = MeterAccumulator::new(0, None);
        meter.damage(
            "Player-0-A",
            "A",
            PLAYER,
            "Player-0-B",
            "Player-0-B",
            REACTION_FRIENDLY,
            0,
            "Hit",
            50,
            1_000,
        );
        hit(&mut meter, "Boss", 0, "Hit", 80, 2_000);
        let data = meter.drain(3_000, 0, "Fight", &HashMap::new());
        let fight = &data.fights[0];
        assert_eq!(fight.actors.len(), 1);
        assert_eq!(fight.actors[0].spells.len(), 1);
        assert_eq!(fight.actors[0].spells[0].amount, 80);
    }

    #[test]
    fn owned_control_npc_damage_merges_into_player() {
        let mut meter = MeterAccumulator::new(0, None);
        meter.record_owner("Creature-0-GHOUL", "Player-0-A", Some("Death Knight"));
        meter.damage(
            "Creature-0-GHOUL",
            "Lesser Ghoul",
            0xa28,
            "Boss",
            "Boss",
            0,
            0,
            "Sweeping Claws",
            75,
            1_000,
        );
        let data = meter.drain(2_000, 0, "Fight", &HashMap::new());
        let actor = &data.fights[0].actors[0];
        assert_eq!(actor.guid, "Player-0-A");
        assert_eq!(actor.name, "Death Knight");
        assert_eq!(actor.spells[0].amount, 75);
    }

    #[test]
    fn support_transfer_preserves_total_and_samples() {
        let mut meter = MeterAccumulator::new(0, None);
        meter.damage(
            "Player-0-A",
            "Dealer",
            PLAYER,
            "Boss",
            "Boss",
            0,
            0,
            "Strike",
            100,
            1_200,
        );
        support(&mut meter, "Ebon Might", 30, 1_200);
        support(&mut meter, "Prescience", 10, 1_200);
        support(&mut meter, "Stale support", 10, 2_201);
        let mut names = HashMap::new();
        names.insert("Player-0-B".to_owned(), "Supporter".to_owned());
        let data = meter.drain(3_000, 0, "Fight", &names);
        let fight = &data.fights[0];
        let dealer = fight
            .actors
            .iter()
            .find(|actor| actor.guid == "Player-0-A")
            .unwrap();
        let supporter = fight
            .actors
            .iter()
            .find(|actor| actor.guid == "Player-0-B")
            .unwrap();
        assert_eq!(supporter.name, "Supporter");
        assert_eq!(
            dealer.spells.iter().map(|entry| entry.amount).sum::<u64>(),
            60
        );
        assert_eq!(
            supporter
                .spells
                .iter()
                .map(|entry| entry.amount)
                .sum::<u64>(),
            40
        );
        assert_eq!(
            fight
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .map(|entry| entry.amount)
                .sum::<u64>(),
            100
        );
        let projected = project_current(&data.fights, 2_000).unwrap();
        assert_eq!(
            projected
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .map(|entry| entry.amount)
                .sum::<u64>(),
            100
        );
    }

    #[test]
    fn target_rows_key_on_name_and_marker() {
        let mut meter = MeterAccumulator::new(0, None);
        meter.damage(
            "Player-0-A",
            "A",
            PLAYER,
            "Boss",
            "Boss",
            0,
            0x80,
            "Hit",
            10,
            1_000,
        );
        hit(&mut meter, "Boss", 0, "Hit", 20, 2_000);
        meter.damage(
            "Player-0-A",
            "A",
            PLAYER,
            "Boss",
            "Boss",
            0,
            0x80,
            "Hit",
            30,
            3_000,
        );
        let data = meter.drain(4_000, 0, "Fight", &HashMap::new());
        let targets = &data.fights[0].actors[0].targets;
        assert_eq!(targets.len(), 2);
        let skull = targets.iter().find(|target| target.marker == 0x80).unwrap();
        assert_eq!(skull.key, "Boss");
        assert_eq!(skull.amount, 40);
        let unmarked = targets.iter().find(|target| target.marker == 0).unwrap();
        assert_eq!(unmarked.key, "Boss");
        assert_eq!(unmarked.amount, 20);
    }

    #[test]
    fn other_folding_preserves_totals() {
        let mut meter = MeterAccumulator::new(0, None);
        let mut total = 0;
        for index in 0..20i64 {
            let amount = (index + 1) as u64;
            total += amount;
            let spell = format!("Spell {index}");
            hit(&mut meter, "Boss", 0, &spell, amount, 1_000 + index);
            let spell = format!("Pet Spell {index}");
            pet_hit(&mut meter, "Boss", &spell, amount, 1_000 + index);
        }
        meter.record_owner("Pet-0-1", "Player-0-A", None);
        let data = meter.drain(5_000, 0, "Fight", &HashMap::new());
        let spells = &data.fights[0].actors[0].spells;
        assert_eq!(spells.len(), MAX_BREAKDOWN_ROWS);
        let sum: u64 = spells.iter().map(|spell| spell.amount).sum();
        assert_eq!(sum, total * 2);
        assert_eq!(spells.last().unwrap().key, "Other");
    }

    /// Casts count the player's own button presses: no pets, no hidden
    /// book-keeping casts, and no widening of the active window every rate
    /// divides by.
    #[test]
    fn casts_count_player_button_presses_only() {
        let mut meter = MeterAccumulator::new(0, None);
        meter.cast(
            "Player-0-A",
            "A",
            FRIENDLY_PLAYER,
            "Arcane Intellect",
            1_000,
        );
        // A pet's own casts are the game acting, never a button press.
        meter.cast("Pet-0-1", "Imp", PLAYER, "Firebolt", 5_000);
        hit(&mut meter, "Boss", MOB, "Frostbolt", 50_000, 10_000);
        hit(&mut meter, "Boss", MOB, "Frostbolt", 50_000, 15_000);
        meter.cast("Player-0-A", "A", FRIENDLY_PLAYER, "Frostbolt", 20_000);
        // Book-keeping casts the game fires for itself, well above any rate a
        // player presses a button at.
        for index in 0..40 {
            meter.cast(
                "Player-0-A",
                "A",
                FRIENDLY_PLAYER,
                "Soul Fragment",
                1_000 + index * 500,
            );
        }
        let data = meter.drain(30_000, 0, "Fight", &HashMap::new());
        let fight = &data.fights[0];
        assert_eq!(fight.active_ms, 5_000);
        let casts: Vec<&MeterEntry> = fight.actors[0]
            .spells
            .iter()
            .filter(|entry| entry.metric == MeterMetric::Casts)
            .collect();
        assert_eq!(casts.len(), 2);
        assert_eq!(casts.iter().map(|entry| entry.amount).sum::<u64>(), 2);
        let keys: Vec<&str> = fight
            .actors
            .iter()
            .flat_map(|actor| &actor.spells)
            .map(|entry| entry.key.as_str())
            .collect();
        assert!(!keys.contains(&"Firebolt"));
        assert!(!keys.contains(&"Soul Fragment"));
    }

    /// Uptime is measured in the spans between applications and removals;
    /// refreshes add occurrences without restarting the span, and the
    /// projection lists application times like every count metric.
    #[test]
    fn buffs_measure_uptime_and_count_applications() {
        let mut meter = MeterAccumulator::new(0, None);
        buff(&mut meter, BuffEvent::Applied, "Bloodlust", 1_000);
        buff(&mut meter, BuffEvent::Refreshed, "Bloodlust", 2_000);
        buff(&mut meter, BuffEvent::Removed, "Bloodlust", 4_000);
        buff(&mut meter, BuffEvent::Applied, "Bloodlust", 5_000);
        let data = meter.drain(10_000, 0, "Fight", &HashMap::new());
        let buffs = |fights: &[MeterFight], position_ms: u64| {
            project_overall(fights, position_ms)
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .find(|entry| entry.metric == MeterMetric::Buffs)
                .map(|entry| (entry.amount, entry.hits, entry.times.clone()))
                .expect("buff row survives the projection")
        };
        // Mid-fight: the closed 3 s span, two applications.
        assert_eq!(buffs(&data.fights, 4_500), (3_000, 2, vec![1_000, 2_000]));
        // At the end: both spans, three applications.
        assert_eq!(
            buffs(&data.fights, 10_000),
            (8_000, 3, vec![1_000, 2_000, 5_000])
        );
    }

    #[test]
    fn buff_uptime_closes_at_fight_boundaries_and_skips_enemies() {
        let mut meter = MeterAccumulator::trash(0);
        meter.buff(
            BuffEvent::Applied,
            "Creature-0-BOSS",
            "Boss",
            MOB,
            "Enrage",
            1_000,
        );
        buff(&mut meter, BuffEvent::Applied, "Bloodlust", 1_000);
        // A re-application without a removal closes the stale span first.
        buff(&mut meter, BuffEvent::Applied, "Bloodlust", 2_000);
        // An unaligned boundary: the closing credit's bucket (5_000) rounds
        // past the cut, and the fight must still end late enough to reach it.
        meter.cut(4_700, "Boss".to_owned());
        // A removal without an intervening application belongs to no span.
        buff(&mut meter, BuffEvent::Removed, "Bloodlust", 4_800);
        // A refresh after the boundary re-opens the span this fight.
        buff(&mut meter, BuffEvent::Refreshed, "Bloodlust", 6_000);
        buff(&mut meter, BuffEvent::Removed, "Bloodlust", 8_000);
        let data = meter.drain(10_000, 0, "Trash", &HashMap::new());
        let first = data.fights[0].actors[0]
            .spells
            .iter()
            .find(|entry| entry.metric == MeterMetric::Buffs)
            .expect("buff row");
        assert_eq!(first.amount, 1_000 + 2_700);
        assert_eq!(first.hits, 2);
        // Overall at the credit's bucket: the boundary span is visible; the
        // refreshed span lands in its own later bucket.
        let projection = project_overall(&data.fights, 5_000);
        let projected = projection
            .actors
            .iter()
            .flat_map(|actor| &actor.spells)
            .find(|entry| entry.metric == MeterMetric::Buffs)
            .expect("boundary credit is visible at the playhead");
        assert_eq!(projected.amount, 3_700);
        assert_eq!(projected.times, vec![1_000, 2_000]);
        let second = data.fights[1].actors[0]
            .spells
            .iter()
            .find(|entry| entry.metric == MeterMetric::Buffs)
            .expect("refreshed buff row");
        assert_eq!(second.amount, 2_000);
        assert_eq!(second.hits, 1);
        // The enemy aura was never tracked.
        assert_eq!(data.fights[0].actors.len(), 1);
    }

    #[test]
    fn ally_trash_stays_ambient_until_the_host_engages() {
        let mut meter = MeterAccumulator::trash(0);
        meter.damage(
            "Player-0-ALLY",
            "Qeld",
            ALLY,
            "Trash",
            "Trash",
            MOB,
            0,
            "Thrash",
            245_000,
            1_000,
        );
        host_hit(&mut meter, "Trash", "Strike", 100_000, 31_000);
        host_hit(&mut meter, "Trash", "Strike", 100_000, 35_000);

        let data = meter.drain(40_000, 0, "Dungeon", &HashMap::new());
        assert_eq!(data.fights.len(), 2);
        assert!(data.fights[0].ambient);
        assert!(!data.fights[1].ambient);
        assert_eq!(data.fights[1].first_event_ms, Some(31_000));
        assert!(project_current(&data.fights, 30_000).is_none());
        let current = project_current(&data.fights, 40_000).unwrap();
        assert_eq!(current.elapsed_ms, 4_000);
        assert_eq!(current.actors[0].guid, "Player-0-HOST");
        let overall = project_overall(&data.fights, 40_000);
        assert_eq!(
            overall
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .map(|entry| entry.amount)
                .sum::<u64>(),
            445_000
        );
    }

    #[test]
    fn a_pull_gap_freezes_current_and_keeps_later_ally_damage_overall() {
        let mut meter = MeterAccumulator::trash(0);
        host_hit(&mut meter, "Mob", "Hit", 10, 1_000);
        host_hit(&mut meter, "Mob", "Hit", 20, 3_000);
        ally_hit(&mut meter, "Mob", "Hit", 30, 30_000);

        let data = meter.drain(40_000, 0, "Dungeon", &HashMap::new());
        assert_eq!(data.fights.len(), 3);
        assert!(!data.fights[1].ambient);
        assert!(data.fights[2].ambient);
        let current = project_current(&data.fights, 40_000).unwrap();
        assert_eq!(current.elapsed_ms, 2_000);
        assert_eq!(current.actors[0].guid, "Player-0-HOST");
        assert_eq!(current.actors[0].spells[0].amount, 30);
        let overall = project_overall(&data.fights, 40_000);
        assert_eq!(
            overall
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .map(|entry| entry.amount)
                .sum::<u64>(),
            60
        );
    }

    #[test]
    fn damage_taken_by_the_host_starts_current() {
        let mut meter = MeterAccumulator::trash(0);
        ally_hit(&mut meter, "Mob", "Hit", 10, 1_000);
        meter.damage(
            "Creature-0-MOB",
            "Mob",
            MOB,
            "Host",
            "Host",
            SELF,
            0,
            "Hit",
            50,
            4_000,
        );
        ally_hit(&mut meter, "Mob", "Hit", 20, 5_000);

        let data = meter.drain(10_000, 0, "Dungeon", &HashMap::new());
        assert_eq!(data.fights.len(), 2);
        assert_eq!(data.fights[1].first_event_ms, Some(4_000));
        assert_eq!(
            project_current(&data.fights, 10_000).unwrap().elapsed_ms,
            1_000
        );
    }

    #[test]
    fn death_events_from_earlier_pulls_do_not_bleed_in() {
        let mut meter = MeterAccumulator::trash(0);
        // An ally takes damage during a first pull that ends without a death.
        mob_hit(&mut meter, "Player-0-ALLY", "Ally", "Stale Hit", 10, 1_000);
        // The host opens a new pull beyond the gap; the ally dies to one hit.
        host_hit(&mut meter, "Mob", "Hit", 20, 30_000);
        mob_hit(&mut meter, "Player-0-ALLY", "Ally", "Kill", 500, 31_000);
        meter.death("Player-0-ALLY", "Ally", 31_100);

        let data = meter.drain(40_000, 0, "Dungeon", &HashMap::new());
        let deaths: Vec<&MeterDeath> = data.fights.iter().flat_map(|fight| &fight.deaths).collect();
        assert_eq!(deaths.len(), 1);
        assert_eq!(deaths[0].events.len(), 1);
        assert_eq!(deaths[0].events[0].spell_name, "Kill");
    }

    #[test]
    fn joining_an_ongoing_pull_keeps_the_death_log() {
        let mut meter = MeterAccumulator::trash(0);
        // The group is already fighting; the host has not engaged yet.
        for (at, hp) in [(1_000, 300_000), (2_000, 220_000)] {
            // Spell hits carry the victim's HP after the hit.
            meter.note_hp("Player-0-ALLY", hp, 500_000, at);
            mob_hit(&mut meter, "Player-0-ALLY", "Ally", "Early Hit", 10, at);
        }
        // The host joins the same pull, splitting the fight into Current.
        host_hit(&mut meter, "Mob", "Strike", 20, 3_000);
        // A swing reports the swinger's HP, not the victim's, so the killing
        // blow's remainder is derived from the amount.
        mob_hit(&mut meter, "Player-0-ALLY", "Ally", "Kill", 500, 3_400);
        // The killing blow's own line reports the post-hit HP, so overkill
        // must be measured against the HP standing before it.
        meter.note_hp("Player-0-ALLY", 0, 500_000, 3_450);
        mob_hit(
            &mut meter,
            "Player-0-ALLY",
            "Ally",
            "Killing Blow",
            300_000,
            3_450,
        );
        meter.death("Player-0-ALLY", "Ally", 3_500);

        let data = meter.drain(9_000, 0, "Dungeon", &HashMap::new());
        let deaths: Vec<&MeterDeath> = data.fights.iter().flat_map(|fight| &fight.deaths).collect();
        assert_eq!(deaths.len(), 1);
        assert_eq!(deaths[0].events.len(), 4);
        assert_eq!(deaths[0].max_hp, 500_000);
        assert_eq!(deaths[0].events[1].hp, 220_000);
        assert_eq!(deaths[0].events[2].hp, 219_500);
        assert_eq!(deaths[0].events[2].overkill, 0);
        // The killing blow wasted everything past the remaining 219_500.
        assert_eq!(deaths[0].events[3].hp, 0);
        assert_eq!(deaths[0].events[3].overkill, 80_500);
    }

    #[test]
    fn healer_joined_pull_still_splits_after_damage_silence() {
        let mut meter = MeterAccumulator::trash(0);
        ally_hit(&mut meter, "Mob", "Hit", 10, 100_000);
        ally_hit(&mut meter, "Mob", "Hit", 10, 110_000);
        heal_host(&mut meter, "Ally", 100, 112_000);
        heal_host(&mut meter, "Ally", 100, 129_000);
        ally_hit(&mut meter, "Mob 2", "Hit", 10, 130_000);
        host_hit(&mut meter, "Mob 2", "Hit", 10, 131_000);
        heal_host(&mut meter, "Host", 50, 132_000);

        let data = meter.drain(140_000, 0, "Dungeon", &HashMap::new());
        let current = project_current(&data.fights, 140_000).unwrap();
        assert_eq!(current.elapsed_ms, 0);
        assert_eq!(
            current
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .filter(|entry| entry.metric == MeterMetric::Healing)
                .map(|entry| entry.amount)
                .sum::<u64>(),
            50
        );
    }

    #[test]
    fn host_death_keeps_the_group_fight_current() {
        let mut meter = MeterAccumulator::trash(0);
        host_hit(&mut meter, "Mob", "Hit", 10, 1_000);
        ally_hit(&mut meter, "Mob", "Hit", 20, 3_000);
        meter.host_died();
        // Lingering host effects and surviving allies stay in the same fight.
        host_hit(&mut meter, "Mob", "DoT", 5, 5_000);
        ally_hit(&mut meter, "Mob", "Hit", 30, 7_000);

        let data = meter.drain(10_000, 0, "Dungeon", &HashMap::new());
        assert_eq!(data.fights.len(), 2);
        assert!(!data.fights[1].ambient);
        let current = project_current(&data.fights, 10_000).unwrap();
        assert_eq!(current.elapsed_ms, 6_000);
        assert_eq!(current.actors.len(), 2);
        assert_eq!(
            current
                .actors
                .iter()
                .flat_map(|actor| &actor.spells)
                .map(|entry| entry.amount)
                .sum::<u64>(),
            65
        );
    }

    #[test]
    fn boss_cuts_override_host_state_then_return_to_ambient_trash() {
        let mut meter = MeterAccumulator::trash(0);
        ally_hit(&mut meter, "Mob", "Hit", 10, 1_000);
        meter.cut(10_000, "Boss".to_owned());
        ally_hit(&mut meter, "Boss", "Hit", 20, 11_000);
        ally_hit(&mut meter, "Boss", "Hit", 30, 30_000);
        meter.cut_to_trash(40_000);

        let data = meter.drain(50_000, 0, "Dungeon", &HashMap::new());
        assert_eq!(data.fights.len(), 3);
        assert!(data.fights[0].ambient);
        assert!(!data.fights[1].ambient);
        assert_eq!(data.fights[1].label, "Boss");
        assert_eq!(data.fights[1].active_ms, 19_000);
        assert!(data.fights[2].ambient);
    }

    #[test]
    fn projections_advance_on_half_second_samples() {
        let mut meter = MeterAccumulator::new(0, None);
        hit(&mut meter, "Boss", MOB, "Hit", 10, 100);
        hit(&mut meter, "Boss", MOB, "Hit", 20, 600);
        let data = meter.drain(1_500, 0, "Fight", &HashMap::new());

        assert!(
            project_current(&data.fights, 499)
                .unwrap()
                .actors
                .is_empty()
        );
        assert_eq!(
            project_current(&data.fights, 500).unwrap().actors[0].spells[0].amount,
            10
        );
        assert_eq!(
            project_current(&data.fights, 999).unwrap().actors[0].spells[0].amount,
            10
        );
        assert_eq!(
            project_current(&data.fights, 1_000).unwrap().actors[0].spells[0].amount,
            30
        );
    }

    #[test]
    fn cut_splits_fights_at_segment_boundaries() {
        let mut meter = MeterAccumulator::new(0, Some("Trash".to_owned()));
        hit(&mut meter, "Boss", 0, "Hit", 10, 1_000);
        hit(&mut meter, "Boss", 0, "Hit", 5, 1_500);
        meter.cut(2_000, "Boss One".to_owned());
        hit(&mut meter, "Boss", 0, "Hit", 20, 3_000);
        let data = meter.drain(4_000, 0, "Fight", &HashMap::new());
        assert_eq!(data.fights.len(), 2);
        assert_eq!(data.fights[0].label, "Trash");
        assert_eq!(data.fights[0].start_ms, 0);
        assert_eq!(data.fights[0].end_ms, 2_000);
        assert_eq!(data.fights[0].active_ms, 500);
        assert_eq!(data.fights[0].actors[0].spells[0].amount, 15);
        assert_eq!(
            data.fights[0].actors[0].spells[0]
                .samples
                .iter()
                .map(|sample| (sample.at_ms, sample.amount))
                .collect::<Vec<_>>(),
            vec![(1_000, 10), (1_500, 5)]
        );
        assert_eq!(data.fights[1].label, "Boss One");
        assert_eq!(data.fights[1].start_ms, 2_000);
        assert_eq!(data.fights[1].end_ms, 4_000);
        assert_eq!(data.fights[1].active_ms, 0);
        assert_eq!(data.fights[1].actors[0].spells[0].amount, 20);
        let current = project_current(&data.fights, 1_000).unwrap();
        assert_eq!(current.actors[0].spells[0].amount, 10);
        let current = project_current(&data.fights, 3_000).unwrap();
        assert_eq!(current.actors[0].spells[0].amount, 20);
        let overall = project_overall(&data.fights, 3_000);
        assert_eq!(overall.actors[0].spells[0].amount, 35);
        assert_eq!(overall.elapsed_ms, 500);
    }

    #[test]
    fn unknown_pets_stay_their_own_row_and_uncontrolled_sources_are_excluded() {
        let mut meter = MeterAccumulator::new(0, None);
        // Hostile NPC source: not player-controlled, excluded entirely.
        meter.damage(
            "Creature-0-X",
            "Mob",
            0,
            "Boss",
            "Boss",
            0,
            0,
            "Hit",
            999,
            1_000,
        );
        // Player-controlled pet without an ownership record stays its own row.
        pet_hit(&mut meter, "Boss", "Firebolt", 100, 2_000);
        let data = meter.drain(3_000, 0, "Fight", &HashMap::new());
        let actors = &data.fights[0].actors;
        assert_eq!(actors.len(), 1);
        assert_eq!(actors[0].guid, "Pet-0-1");
        assert_eq!(actors[0].name, "Imp");
    }

    #[test]
    fn damage_taken_uses_destination_without_changing_active_time() {
        let mut meter = MeterAccumulator::new(0, None);
        mob_hit(&mut meter, "Player-0-TANK", "Tank", "Claw", 30, 500);
        meter.damage(
            "Player-0-A",
            "A",
            FRIENDLY_PLAYER,
            "Creature-0-BOSS",
            "Boss",
            MOB,
            0,
            "Strike",
            10,
            1_000,
        );
        meter.damage(
            "Player-0-A",
            "A",
            FRIENDLY_PLAYER,
            "Creature-0-BOSS",
            "Boss",
            MOB,
            0,
            "Strike",
            20,
            3_000,
        );
        mob_hit(&mut meter, "Player-0-TANK", "Tank", "Claw", 40, 4_000);
        let data = meter.drain(4_500, 0, "Fight", &HashMap::new());
        let fight = &data.fights[0];
        assert_eq!(fight.active_ms, 2_000);
        let tank = fight
            .actors
            .iter()
            .find(|actor| actor.guid == "Player-0-TANK")
            .unwrap();
        assert_eq!(
            tank.spells
                .iter()
                .find(|entry| entry.metric == MeterMetric::DamageTaken)
                .unwrap()
                .amount,
            70
        );
    }

    #[test]
    fn death_log_is_scoped_to_the_fight_since_the_cut() {
        let mut meter = MeterAccumulator::new(0, None);
        for index in 1..=11 {
            if index == 6 {
                meter.cut(5_500, "Boss".to_owned());
            }
            mob_hit(
                &mut meter,
                "Player-0-TANK",
                "Tank",
                "Claw",
                index,
                index as i64 * 1_000,
            );
        }
        meter.death("Player-0-TANK", "Tank", 12_000);
        meter.heal(
            "Player-0-HEALER",
            "Healer",
            FRIENDLY_PLAYER,
            "Player-0-TANK",
            "Tank",
            FRIENDLY_PLAYER,
            0,
            "Flash Heal",
            100,
            40,
            12_500,
        );
        meter.death("Player-0-TANK", "Tank", 13_000);
        let data = meter.drain(14_000, 0, "Fight", &HashMap::new());
        let deaths = &data.fights[1].deaths;
        assert_eq!(deaths.len(), 2);
        // Events before the cut describe the previous fight and must not
        // bleed into the boss-fight death.
        assert_eq!(deaths[0].events.len(), 6);
        assert_eq!(deaths[0].events[0].at_ms, 6_000);
        assert_eq!(deaths[0].events[5].at_ms, 11_000);
        assert_eq!(deaths[1].events.len(), 1);
        assert_eq!(deaths[1].events[0].kind, MeterDeathEventKind::Healing);
        assert_eq!(deaths[1].events[0].amount, 60);
        assert!(
            project_current(&data.fights, 11_999)
                .unwrap()
                .deaths
                .is_empty()
        );
        assert_eq!(
            project_current(&data.fights, 12_000).unwrap().deaths.len(),
            1
        );
        assert_eq!(project_overall(&data.fights, 13_000).deaths.len(), 2);
        // Healing Done is the effective amount; overheal is kept apart.
        let heal = data.fights[1]
            .actors
            .iter()
            .find(|actor| actor.guid == "Player-0-HEALER")
            .and_then(|actor| actor.spells.first())
            .unwrap();
        assert_eq!(heal.metric, MeterMetric::Healing);
        assert_eq!((heal.amount, heal.overheal), (60, 40));
    }
}
