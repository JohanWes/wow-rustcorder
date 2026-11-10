// SPDX-License-Identifier: GPL-3.0-or-later

//! Combat-log parsing into small facts consumed by the activity engine.

use std::borrow::Cow;

use crate::domain::{GameFlavor, MeterMetric};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedEvent {
    pub flavor: GameFlavor,
    pub occurred_at_ms: i64,
    pub event: CombatEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CombatEvent {
    ZoneChanged {
        zone_id: u32,
        /// 0 outside instances and when the line carries none.
        difficulty_id: u32,
    },
    EncounterStarted {
        encounter_id: u32,
        name: String,
        difficulty_id: u32,
    },
    EncounterEnded {
        difficulty_id: u32,
        success: bool,
    },
    ChallengeStarted {
        zone_id: u32,
        map_id: u32,
        level: u32,
        affixes: Vec<u32>,
    },
    ChallengeEnded {
        success: bool,
        duration_ms: u64,
    },
    ArenaStarted {
        zone_id: u32,
        match_type: String,
    },
    ArenaEnded {
        winning_team_id: u32,
    },
    Combatant {
        guid: String,
        team_id: Option<u8>,
        spec_id: Option<u16>,
    },
    PlayerObserved {
        kind: PlayerObservationKind,
        aura_type: Option<AuraType>,
        spell_id: u32,
        guid: String,
        name: String,
        flags: u64,
        target_guid: String,
        target_name: String,
        target_flags: u64,
        spell_name: String,
        /// Source-side `ownerGUID` when the advanced block identifies the
        /// source unit, used for pet-to-owner attribution.
        owner_guid: Option<String>,
    },
    UnitDied {
        guid: String,
        name: String,
        flags: u64,
        unconscious: bool,
    },
    Damage {
        source_guid: String,
        source_name: String,
        source_flags: u64,
        /// Source-side `ownerGUID` from `SWING_DAMAGE` swings.
        source_owner_guid: Option<String>,
        dest_guid: String,
        dest_name: String,
        dest_flags: u64,
        dest_raid_marker: u8,
        /// "Melee" for swings, otherwise the spell name.
        spell_name: String,
        amount: u64,
        /// Destination HP, trusted only when the advanced block's infoGUID
        /// names the destination.
        dest_current_hp: Option<u64>,
        dest_max_hp: Option<u64>,
    },
    Heal {
        source_guid: String,
        source_name: String,
        source_flags: u64,
        dest_guid: String,
        dest_name: String,
        dest_flags: u64,
        dest_raid_marker: u8,
        spell_name: String,
        amount: u64,
        overheal: u64,
        /// Destination HP after the heal, on the same infoGUID rule as damage.
        dest_current_hp: Option<u64>,
        dest_max_hp: Option<u64>,
    },
    /// Damage or effective healing reassigned from the original actor to the
    /// player whose support effect contributed it.
    Support {
        metric: MeterMetric,
        supporter_guid: String,
        source_guid: String,
        dest_name: String,
        dest_raid_marker: u8,
        spell_name: String,
        amount: u64,
        overheal: u64,
    },
    Interrupt {
        source_guid: String,
        source_name: String,
        source_flags: u64,
        dest_name: String,
        dest_raid_marker: u8,
        /// The interrupted spell name.
        spell_name: String,
    },
    Dispel {
        source_guid: String,
        source_name: String,
        source_flags: u64,
        dest_name: String,
        dest_raid_marker: u8,
        /// The dispelled or stolen spell name.
        spell_name: String,
    },
    Summon {
        source_guid: String,
        source_name: String,
        source_flags: u64,
        pet_guid: String,
    },
    /// A `SPELL_CAST_START`.
    BossCast {
        source_name: String,
        spell_name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerObservationKind {
    AuraApplied,
    AuraRefreshed,
    AuraRemoved,
    CastSucceeded,
}

/// The `auraType` suffix parameter of aura events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuraType {
    Buff,
    Debuff,
}

pub fn is_bloodlust_spell(spell_id: u32) -> bool {
    matches!(spell_id, 2825 | 32182 | 80353 | 264667 | 390386)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseTimeContext {
    pub year: i32,
    pub utc_offset_minutes: i32,
    /// Advanced-block arity selected by the log's COMBAT_LOG_VERSION.
    advanced_block_fields: usize,
}

impl ParseTimeContext {
    pub const fn new(year: i32, utc_offset_minutes: i32) -> Self {
        Self {
            year,
            utc_offset_minutes,
            advanced_block_fields: LEGACY_ADVANCED_BLOCK_FIELDS,
        }
    }

    /// Applies a COMBAT_LOG_VERSION value: version 22 and newer carry a
    /// 19-field advanced block, older or unknown versions the legacy 17.
    pub const fn with_combat_log_version(self, version: u32) -> Self {
        Self {
            advanced_block_fields: if version >= 22 {
                V22_ADVANCED_BLOCK_FIELDS
            } else {
                LEGACY_ADVANCED_BLOCK_FIELDS
            },
            ..self
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseFailure {
    MalformedTimestamp,
    MalformedRetainedEvent,
}

pub fn parse_line(
    flavor: GameFlavor,
    context: ParseTimeContext,
    line: &str,
) -> Result<Option<ParsedEvent>, ParseFailure> {
    let Some((timestamp, payload)) = line.split_once("  ") else {
        return Ok(None);
    };
    let raw_event_name = payload.split_once(',').map_or(payload, |(name, _)| name);
    if !is_retained(raw_event_name) {
        return Ok(None);
    }
    let occurred_at_ms = parse_timestamp(timestamp, context)?;
    let fields = split_fields(payload).map_err(|()| ParseFailure::MalformedRetainedEvent)?;
    let Some(event_name) = fields.first().map(|value| &**value) else {
        return Ok(None);
    };

    let event = match event_name {
        "ZONE_CHANGE" => CombatEvent::ZoneChanged {
            zone_id: number(&fields, 1)?,
            difficulty_id: optional_number(&fields, 3)?.unwrap_or(0),
        },
        "ENCOUNTER_START" => CombatEvent::EncounterStarted {
            encounter_id: number(&fields, 1)?,
            name: text(&fields, 2)?.to_owned(),
            difficulty_id: number(&fields, 3)?,
        },
        "ENCOUNTER_END" => CombatEvent::EncounterEnded {
            difficulty_id: number(&fields, 3)?,
            success: number::<u8>(&fields, 5)? != 0,
        },
        "CHALLENGE_MODE_START" => CombatEvent::ChallengeStarted {
            zone_id: number(&fields, 2)?,
            map_id: number(&fields, 3)?,
            level: number(&fields, 4)?,
            affixes: integer_list(text(&fields, 5)?)?,
        },
        "CHALLENGE_MODE_END" => CombatEvent::ChallengeEnded {
            success: number::<u8>(&fields, 2)? != 0,
            duration_ms: number(&fields, 4)?,
        },
        "ARENA_MATCH_START" => CombatEvent::ArenaStarted {
            zone_id: number(&fields, 1)?,
            match_type: text(&fields, 3)?.to_owned(),
        },
        "ARENA_MATCH_END" => CombatEvent::ArenaEnded {
            winning_team_id: number(&fields, 1)?,
        },
        "COMBATANT_INFO" => {
            let (team_id, spec_id) = if matches!(flavor, GameFlavor::Retail) {
                (
                    Some(number(&fields, 2)?),
                    Some(number(&fields, 25)?).filter(|value| *value != 0),
                )
            } else {
                (optional_number(&fields, 2)?, None)
            };
            CombatEvent::Combatant {
                guid: text(&fields, 1)?.to_owned(),
                team_id,
                spec_id,
            }
        }
        "UNIT_DIED" => CombatEvent::UnitDied {
            guid: text(&fields, 5)?.to_owned(),
            name: text(&fields, 6)?.to_owned(),
            flags: hexadecimal(&fields, 7)?,
            unconscious: optional_number::<u8>(&fields, 9)?.is_some_and(|value| value != 0),
        },
        "SPELL_AURA_APPLIED" | "SPELL_AURA_REFRESH" | "SPELL_AURA_REMOVED"
        | "SPELL_CAST_SUCCESS" => {
            let kind = match event_name {
                "SPELL_AURA_APPLIED" => PlayerObservationKind::AuraApplied,
                "SPELL_AURA_REFRESH" => PlayerObservationKind::AuraRefreshed,
                "SPELL_AURA_REMOVED" => PlayerObservationKind::AuraRemoved,
                _ => PlayerObservationKind::CastSucceeded,
            };
            let guid = text(&fields, 1)?.to_owned();
            let owner_guid = fields
                .get(12)
                .filter(|info_guid| info_guid.as_ref() == guid)
                .and_then(|_| fields.get(13))
                .and_then(|owner| guid_or_none(owner).map(str::to_owned));
            CombatEvent::PlayerObserved {
                kind,
                // Aura events carry `auraType` right after the spell block;
                // casts do not, and field 12 there is the advanced block.
                aura_type: (kind != PlayerObservationKind::CastSucceeded)
                    .then(|| aura_type(&fields))
                    .flatten(),
                spell_id: number(&fields, 9)?,
                guid,
                name: text(&fields, 2)?.to_owned(),
                flags: hexadecimal(&fields, 3)?,
                target_guid: text(&fields, 5)?.to_owned(),
                target_name: text(&fields, 6)?.to_owned(),
                target_flags: hexadecimal(&fields, 7)?,
                spell_name: text(&fields, 10)?.to_owned(),
                owner_guid,
            }
        }
        "SWING_DAMAGE"
        | "RANGE_DAMAGE"
        | "SPELL_DAMAGE"
        | "SPELL_PERIODIC_DAMAGE"
        | "DAMAGE_SHIELD" => {
            let Some(event) = parse_damage(event_name, &fields, context)? else {
                return Ok(None);
            };
            event
        }
        "SPELL_HEAL" | "SPELL_PERIODIC_HEAL" => {
            let Some(event) = parse_heal(&fields, context)? else {
                return Ok(None);
            };
            event
        }
        "SPELL_DAMAGE_SUPPORT"
        | "SPELL_PERIODIC_DAMAGE_SUPPORT"
        | "RANGE_DAMAGE_SUPPORT"
        | "SWING_DAMAGE_LANDED_SUPPORT"
        | "SPELL_HEAL_SUPPORT"
        | "SPELL_PERIODIC_HEAL_SUPPORT" => {
            let Some(event) = parse_support(event_name, &fields, context)? else {
                return Ok(None);
            };
            event
        }
        "SPELL_INTERRUPT" | "SPELL_DISPEL" | "SPELL_STOLEN" => {
            let Some(event) = parse_utility(&fields, event_name, context) else {
                return Ok(None);
            };
            event
        }
        "SPELL_SUMMON" => {
            let Some(event) = parse_summon(&fields) else {
                return Ok(None);
            };
            event
        }
        "SPELL_CAST_START" => CombatEvent::BossCast {
            source_name: text(&fields, 2)?.to_owned(),
            spell_name: text(&fields, 10)?.to_owned(),
        },
        _ => return Ok(None),
    };

    Ok(Some(ParsedEvent {
        flavor,
        occurred_at_ms,
        event,
    }))
}

const BASE_UNIT_FIELDS: usize = 8;
/// Advanced-block arity carried by logs older than COMBAT_LOG_VERSION 22.
const LEGACY_ADVANCED_BLOCK_FIELDS: usize = 17;
/// COMBAT_LOG_VERSION 22 (Midnight) widened the advanced block by two fields.
const V22_ADVANCED_BLOCK_FIELDS: usize = 19;
pub(crate) const EMPTY_GUID: &str = "0000000000000000";

/// A located advanced block: where it starts, whether the infoGUID rule
/// found one, and where the event suffix begins.
struct AdvancedBlock {
    start: usize,
    present: bool,
    suffix: usize,
}

/// The meter suffix starts after the eight base unit fields plus the advanced
/// block when present. The block is detected by the GUID shape of its
/// infoGUID field at the event-specific boundary, never by total field count;
/// its arity follows the log's COMBAT_LOG_VERSION.
fn advanced_block(
    event_name: &str,
    fields: &[Cow<'_, str>],
    context: ParseTimeContext,
) -> AdvancedBlock {
    let prefix_len = if event_name == "SWING_DAMAGE" { 0 } else { 3 };
    let start = 1 + BASE_UNIT_FIELDS + prefix_len;
    let present = fields
        .get(start)
        .is_some_and(|value| value == EMPTY_GUID || value.contains('-'));
    let suffix = if present {
        start + context.advanced_block_fields
    } else {
        start
    };
    AdvancedBlock {
        start,
        present,
        suffix,
    }
}

/// A real unit GUID, excluding the empty/nil placeholders.
fn guid_or_none(value: &str) -> Option<&str> {
    (!value.is_empty() && value != EMPTY_GUID && value != "nil" && value != "0").then_some(value)
}

/// Lenient numeric field read: missing or unparseable fields read as `None`
/// instead of a parser diagnostic (advanced logging may be off).
fn lenient_number<T: std::str::FromStr>(fields: &[Cow<'_, str>], index: usize) -> Option<T> {
    fields
        .get(index)
        .filter(|value| !value.is_empty() && value.as_ref() != "nil")
        .and_then(|value| value.parse().ok())
}

fn lenient_hex(fields: &[Cow<'_, str>], index: usize) -> Option<u64> {
    fields
        .get(index)
        .and_then(|value| u64::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16).ok())
}

/// The raid-target marker in the low byte of the destination raid flags.
fn dest_raid_marker(fields: &[Cow<'_, str>]) -> u8 {
    (lenient_hex(fields, 8).unwrap_or(0) & 0xff) as u8
}

/// Destination HP from the advanced block, trusted only when its infoGUID
/// names the destination (for swings the block describes the source).
fn dest_hp(
    fields: &[Cow<'_, str>],
    block: &AdvancedBlock,
    dest_guid: &str,
) -> (Option<u64>, Option<u64>) {
    if block.present
        && fields
            .get(block.start)
            .is_some_and(|info_guid| info_guid == dest_guid)
    {
        (
            lenient_number(fields, block.start + 2),
            lenient_number(fields, block.start + 3),
        )
    } else {
        (None, None)
    }
}

/// The meter amount at the start of the suffix. A detected advanced block
/// promises a readable amount; a missing or unparseable one means the line is
/// truncated, not that advanced logging is off.
fn suffix_amount(
    fields: &[Cow<'_, str>],
    block: &AdvancedBlock,
) -> Result<Option<u64>, ParseFailure> {
    match lenient_number(fields, block.suffix) {
        None if block.present => Err(ParseFailure::MalformedRetainedEvent),
        amount => Ok(amount),
    }
}

fn parse_damage(
    event_name: &str,
    fields: &[Cow<'_, str>],
    context: ParseTimeContext,
) -> Result<Option<CombatEvent>, ParseFailure> {
    let block = advanced_block(event_name, fields, context);
    let Some(amount) = suffix_amount(fields, &block)? else {
        return Ok(None);
    };
    let event = (|| {
        let source_guid = fields.get(1)?.as_ref();
        let dest_guid = fields.get(5)?.as_ref();
        let (dest_current_hp, dest_max_hp) = dest_hp(fields, &block, dest_guid);
        // SWING_DAMAGE's block names the source, so its ownerGUID is the
        // swinging pet's owner.
        let source_owner_guid = if event_name == "SWING_DAMAGE"
            && block.present
            && fields
                .get(block.start)
                .is_some_and(|info_guid| info_guid == source_guid)
        {
            fields
                .get(block.start + 1)
                .and_then(|owner| guid_or_none(owner).map(str::to_owned))
        } else {
            None
        };
        Some(CombatEvent::Damage {
            source_guid: source_guid.to_owned(),
            source_name: fields.get(2)?.as_ref().to_owned(),
            source_flags: lenient_hex(fields, 3)?,
            source_owner_guid,
            dest_guid: dest_guid.to_owned(),
            dest_name: fields.get(6)?.as_ref().to_owned(),
            dest_flags: lenient_hex(fields, 7)?,
            dest_raid_marker: dest_raid_marker(fields),
            spell_name: if event_name == "SWING_DAMAGE" {
                "Melee".to_owned()
            } else {
                fields.get(10)?.as_ref().to_owned()
            },
            amount,
            dest_current_hp,
            dest_max_hp,
        })
    })();
    Ok(event)
}

fn parse_heal(
    fields: &[Cow<'_, str>],
    context: ParseTimeContext,
) -> Result<Option<CombatEvent>, ParseFailure> {
    let block = advanced_block("SPELL_HEAL", fields, context);
    let Some(amount) = suffix_amount(fields, &block)? else {
        return Ok(None);
    };
    let event = (|| {
        // Modern healing suffixes carry baseAmount at index 1, older layouts do
        // not, so the overhealing index follows the suffix arity.
        let overheal_index = block.suffix
            + if fields.len() - block.suffix >= 5 {
                2
            } else {
                1
            };
        let overheal = lenient_number::<u64>(fields, overheal_index).unwrap_or(0);
        let (dest_current_hp, dest_max_hp) = dest_hp(fields, &block, fields.get(5)?.as_ref());
        Some(CombatEvent::Heal {
            source_guid: fields.get(1)?.as_ref().to_owned(),
            source_name: fields.get(2)?.as_ref().to_owned(),
            source_flags: lenient_hex(fields, 3)?,
            dest_guid: fields.get(5)?.as_ref().to_owned(),
            dest_name: fields.get(6)?.as_ref().to_owned(),
            dest_flags: lenient_hex(fields, 7)?,
            dest_raid_marker: dest_raid_marker(fields),
            spell_name: fields.get(10)?.as_ref().to_owned(),
            amount,
            overheal,
            dest_current_hp,
            dest_max_hp,
        })
    })();
    Ok(event)
}

/// Interrupts and dispels carry the interrupted/dispelled spell as the second
/// suffix parameter; no spell IDs are needed anywhere.
fn parse_utility(
    fields: &[Cow<'_, str>],
    event_name: &str,
    context: ParseTimeContext,
) -> Option<CombatEvent> {
    let block = advanced_block(event_name, fields, context);
    let spell_name = fields
        .get(block.suffix + 1)
        .filter(|value| !value.is_empty() && value.as_ref() != "nil")?;
    let spell_name = spell_name.to_string();
    Some(if event_name == "SPELL_INTERRUPT" {
        CombatEvent::Interrupt {
            source_guid: fields.get(1)?.as_ref().to_owned(),
            source_name: fields.get(2)?.as_ref().to_owned(),
            source_flags: lenient_hex(fields, 3)?,
            dest_name: fields.get(6)?.as_ref().to_owned(),
            dest_raid_marker: dest_raid_marker(fields),
            spell_name,
        }
    } else {
        CombatEvent::Dispel {
            source_guid: fields.get(1)?.as_ref().to_owned(),
            source_name: fields.get(2)?.as_ref().to_owned(),
            source_flags: lenient_hex(fields, 3)?,
            dest_name: fields.get(6)?.as_ref().to_owned(),
            dest_raid_marker: dest_raid_marker(fields),
            spell_name,
        }
    })
}

fn parse_support(
    event_name: &str,
    fields: &[Cow<'_, str>],
    context: ParseTimeContext,
) -> Result<Option<CombatEvent>, ParseFailure> {
    // Even SWING_DAMAGE_LANDED_SUPPORT has the three-field spell prefix, so
    // advanced_block's exact SWING_DAMAGE special case must not apply here.
    let block = advanced_block(event_name, fields, context);
    let Some(amount) = suffix_amount(fields, &block)? else {
        return Ok(None);
    };
    let healing = event_name.contains("HEAL");
    let overheal = if healing {
        let index = block.suffix
            + if fields.len() - block.suffix >= 6 {
                2
            } else {
                1
            };
        lenient_number(fields, index).unwrap_or(0)
    } else {
        0
    };
    let event = (|| {
        Some(CombatEvent::Support {
            metric: if healing {
                MeterMetric::Healing
            } else {
                MeterMetric::Damage
            },
            supporter_guid: guid_or_none(fields.last()?)?.to_owned(),
            source_guid: fields.get(1)?.as_ref().to_owned(),
            dest_name: fields.get(6)?.as_ref().to_owned(),
            dest_raid_marker: dest_raid_marker(fields),
            spell_name: fields.get(10)?.as_ref().to_owned(),
            amount,
            overheal,
        })
    })();
    Ok(event)
}

fn parse_summon(fields: &[Cow<'_, str>]) -> Option<CombatEvent> {
    Some(CombatEvent::Summon {
        source_guid: fields.get(1)?.as_ref().to_owned(),
        source_name: fields.get(2)?.as_ref().to_owned(),
        source_flags: lenient_hex(fields, 3)?,
        pet_guid: fields.get(5)?.as_ref().to_owned(),
    })
}

pub(crate) fn event_name(line: &str) -> Option<&str> {
    let payload = line.split_once("  ")?.1;
    Some(payload.split_once(',').map_or(payload, |(name, _)| name))
}

/// The integer `COMBAT_LOG_VERSION` value of a complete header line, with or
/// without the leading timestamp real clients write. Event lines and headers
/// without a readable version return `None`; a header is never a retained
/// event itself.
pub(crate) fn combat_log_version(line: &str) -> Option<u32> {
    let payload = line.split_once("  ").map_or(line, |(_, rest)| rest);
    payload
        .strip_prefix("COMBAT_LOG_VERSION,")?
        .split(',')
        .next()?
        .parse()
        .ok()
}

/// The `auraType` suffix field of aura events, at the fixed index after the
/// spell block.
fn aura_type(fields: &[Cow<'_, str>]) -> Option<AuraType> {
    match fields.get(12).map(|value| &**value) {
        Some("BUFF") => Some(AuraType::Buff),
        Some("DEBUFF") => Some(AuraType::Debuff),
        _ => None,
    }
}

fn is_retained(name: &str) -> bool {
    matches!(
        name,
        "ZONE_CHANGE"
            | "ENCOUNTER_START"
            | "ENCOUNTER_END"
            | "CHALLENGE_MODE_START"
            | "CHALLENGE_MODE_END"
            | "ARENA_MATCH_START"
            | "ARENA_MATCH_END"
            | "COMBATANT_INFO"
            | "UNIT_DIED"
            | "SPELL_AURA_APPLIED"
            | "SPELL_AURA_REFRESH"
            | "SPELL_AURA_REMOVED"
            | "SPELL_CAST_START"
            | "SPELL_CAST_SUCCESS"
            | "SWING_DAMAGE"
            | "RANGE_DAMAGE"
            | "SPELL_DAMAGE"
            | "SPELL_PERIODIC_DAMAGE"
            | "DAMAGE_SHIELD"
            | "SPELL_DAMAGE_SUPPORT"
            | "SPELL_PERIODIC_DAMAGE_SUPPORT"
            | "RANGE_DAMAGE_SUPPORT"
            | "SWING_DAMAGE_LANDED_SUPPORT"
            | "SPELL_HEAL"
            | "SPELL_PERIODIC_HEAL"
            | "SPELL_HEAL_SUPPORT"
            | "SPELL_PERIODIC_HEAL_SUPPORT"
            | "SPELL_INTERRUPT"
            | "SPELL_DISPEL"
            | "SPELL_STOLEN"
            | "SPELL_SUMMON"
    )
}

fn text<'a>(fields: &'a [Cow<'_, str>], index: usize) -> Result<&'a str, ParseFailure> {
    fields
        .get(index)
        .map(|value| &**value)
        .ok_or(ParseFailure::MalformedRetainedEvent)
}

fn number<T: std::str::FromStr>(fields: &[Cow<'_, str>], index: usize) -> Result<T, ParseFailure> {
    text(fields, index)?
        .parse()
        .map_err(|_| ParseFailure::MalformedRetainedEvent)
}

fn optional_number<T: std::str::FromStr>(
    fields: &[Cow<'_, str>],
    index: usize,
) -> Result<Option<T>, ParseFailure> {
    fields
        .get(index)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|_| ParseFailure::MalformedRetainedEvent)
        })
        .transpose()
}

fn hexadecimal(fields: &[Cow<'_, str>], index: usize) -> Result<u64, ParseFailure> {
    let value = text(fields, index)?;
    u64::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16)
        .map_err(|_| ParseFailure::MalformedRetainedEvent)
}

fn integer_list(value: &str) -> Result<Vec<u32>, ParseFailure> {
    let inner = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or(ParseFailure::MalformedRetainedEvent)?;
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|value| {
            value
                .parse()
                .map_err(|_| ParseFailure::MalformedRetainedEvent)
        })
        .collect()
}

/// Splits a payload on top-level commas. Fields borrow the payload and only
/// allocate when a quoted field needs unescaping.
fn split_fields(payload: &str) -> Result<Vec<Cow<'_, str>>, ()> {
    let mut fields = Vec::with_capacity(48);
    let bytes = payload.as_bytes();
    let mut start = 0;
    let mut index = 0;
    let mut quoted = false;
    let mut nesting = 0_u32;

    // Every delimiter is ASCII, so a byte scan never splits a UTF-8 character.
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' if quoted && bytes.get(index + 1) == Some(&b'"') => index += 1,
            b'"' => quoted = !quoted,
            // Skip the escaped byte; a dangling escape leaves `quoted` set.
            b'\\' if quoted => index += 1,
            _ if quoted => {}
            b'[' | b'(' => nesting += 1,
            b']' | b')' => nesting = nesting.checked_sub(1).ok_or(())?,
            b',' if nesting == 0 => {
                fields.push(unquote(&payload[start..index]));
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    if quoted || nesting != 0 {
        return Err(());
    }
    fields.push(unquote(&payload[start..]));
    Ok(fields)
}

/// Removes quoting from one already-delimited field: quotes are dropped,
/// and inside them `\x` yields `x` and `""` yields `"`.
fn unquote(raw: &str) -> Cow<'_, str> {
    if !raw.contains('"') {
        return Cow::Borrowed(raw);
    }
    if let Some(inner) = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        && !inner.contains(['"', '\\'])
    {
        return Cow::Borrowed(inner);
    }
    let mut field = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' => quoted = !quoted,
            '\\' if quoted => field.extend(chars.next()),
            _ => field.push(character),
        }
    }
    Cow::Owned(field)
}

pub(crate) fn parse_timestamp(value: &str, context: ParseTimeContext) -> Result<i64, ParseFailure> {
    let (date, time) = value
        .split_once(' ')
        .ok_or(ParseFailure::MalformedTimestamp)?;
    let mut date_parts = date.split('/');
    let mut date_part = || date_parts.next().map(str::parse::<i32>).transpose();
    let (Ok(Some(month)), Ok(Some(day)), Ok(year), Ok(None)) =
        (date_part(), date_part(), date_part(), date_part())
    else {
        return Err(ParseFailure::MalformedTimestamp);
    };
    let year = year.unwrap_or(context.year);
    if !(1..=9999).contains(&year) {
        return Err(ParseFailure::MalformedTimestamp);
    }
    let (whole_time, raw_fraction) = time
        .split_once('.')
        .ok_or(ParseFailure::MalformedTimestamp)?;
    // Newer combat logs append the writer's UTC offset in whole hours directly
    // onto the three millisecond digits: "038-4" west of UTC, "4481" (448 ms,
    // UTC+1) east of it. The line's own offset is what its local time was
    // written in, so it wins over the caller's: that one is read once at
    // startup and goes stale across a DST change while the app keeps running.
    // Logs without one, or with anything that is not an hour, use the caller's.
    // So does a system set to a zone off the hour (India, central Australia):
    // what the game writes there is unknown, and the caller's was right before.
    let sign_at = raw_fraction.find(['+', '-']);
    let fraction = &raw_fraction[..sign_at.unwrap_or(raw_fraction.len())];
    if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseFailure::MalformedTimestamp);
    }
    let offset = match sign_at {
        Some(index) => &raw_fraction[index..],
        None => fraction.get(3..).unwrap_or_default(),
    };
    let utc_offset_minutes = offset
        .parse::<i32>()
        .ok()
        .filter(|hours| (-12..=14).contains(hours) && context.utc_offset_minutes % 60 == 0)
        .map_or(context.utc_offset_minutes, |hours| hours * 60);
    let mut time_parts = whole_time.split(':');
    let mut time_part = || time_parts.next().map(str::parse::<i64>).transpose();
    let (Ok(Some(hour)), Ok(Some(minute)), Ok(Some(second)), Ok(None)) =
        (time_part(), time_part(), time_part(), time_part())
    else {
        return Err(ParseFailure::MalformedTimestamp);
    };
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return Err(ParseFailure::MalformedTimestamp);
    }
    // Milliseconds are the first three fraction digits, right-padded with zeros.
    let digits = &fraction.as_bytes()[..fraction.len().min(3)];
    let milliseconds = digits
        .iter()
        .fold(0_i64, |value, digit| value * 10 + i64::from(digit - b'0'))
        * 10_i64.pow(3 - digits.len() as u32);
    let days = days_from_civil(year, month, day);
    Ok(
        (days * 86_400 + hour * 3_600 + minute * 60 + second - i64::from(utc_offset_minutes) * 60)
            * 1_000
            + milliseconds,
    )
}

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 31,
    }
}

/// Howard Hinnant's proleptic Gregorian days-from-civil conversion.
pub(crate) fn days_from_civil(year: i32, month: i32, day: i32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT: ParseTimeContext = ParseTimeContext::new(2026, 120);
    const V22_CONTEXT: ParseTimeContext =
        ParseTimeContext::new(2026, 120).with_combat_log_version(22);

    #[test]
    fn parses_every_retained_event_shape_in_order() {
        let fixture = [
            "4/9 19:27:13.200  ZONE_CHANGE,2652,\"The Stonevault\",23",
            "4/9 19:27:14.200  CHALLENGE_MODE_START,\"The Stonevault\",2652,501,10,[158,10,152,9]",
            "4/9 19:27:15.200  ENCOUNTER_START,9999,\"Training Construct\",8,5,2652,1",
            "4/9 19:27:16.200  ENCOUNTER_END,9999,\"Training Construct\",8,5,1",
            "4/9 19:27:17.200  CHALLENGE_MODE_END,2652,1,3,123456,0,0",
            "4/9 19:27:18.200  ARENA_MATCH_START,1134,34,\"Rated, Solo Shuffle\",0",
            "4/9 19:27:19.200  ARENA_MATCH_END,1,8,1600,1700",
            "4/9 19:27:20.200  UNIT_DIED,0,nil,0x0,0x0,Player-0-AAAA,\"Player One\",0x511,0x0,0",
            "4/9 19:27:21.200  COMBATANT_INFO,Player-0-AAAA,1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1234,256,[]",
            "4/9 19:27:22.200  SPELL_AURA_APPLIED,Player-0-AAAA,\"Player One\",0x511,0x0,Player-0-BBBB,\"Player Two\",0x512,0x0,123,\"Aura, Tested\",0x1,BUFF",
            "4/9 19:27:23.200  SPELL_DAMAGE,Player-0-AAAA,\"Player One\",0x511,0x0,Creature-0-BOSS,\"Training Boss\",0x10a48,0x0,123,\"Smite\",0x2,Creature-0-BOSS,0000000000000000,105,152,0,0,189,2084,0,0,0,0,0,0,0,0,0,46,0,2,0,0,0,1,0,0,0,0.000,1,1",
            "4/9 19:27:24.200  SPELL_CAST_START,Creature-0-BOSS,\"Training Boss\",0x10a48,0x0,0,nil,0x0,0x0,456,\"Rebirth\",0x1",
        ];
        let events = fixture
            .iter()
            .map(|line| {
                parse_line(GameFlavor::Retail, CONTEXT, line)
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let golden = vec![
            CombatEvent::ZoneChanged {
                zone_id: 2652,
                difficulty_id: 23,
            },
            CombatEvent::ChallengeStarted {
                zone_id: 2652,
                map_id: 501,
                level: 10,
                affixes: vec![158, 10, 152, 9],
            },
            CombatEvent::EncounterStarted {
                encounter_id: 9999,
                name: "Training Construct".into(),
                difficulty_id: 8,
            },
            CombatEvent::EncounterEnded {
                difficulty_id: 8,
                success: true,
            },
            CombatEvent::ChallengeEnded {
                success: true,
                duration_ms: 123_456,
            },
            CombatEvent::ArenaStarted {
                zone_id: 1134,
                match_type: "Rated, Solo Shuffle".into(),
            },
            CombatEvent::ArenaEnded { winning_team_id: 1 },
            CombatEvent::UnitDied {
                guid: "Player-0-AAAA".into(),
                name: "Player One".into(),
                flags: 0x511,
                unconscious: false,
            },
            CombatEvent::Combatant {
                guid: "Player-0-AAAA".into(),
                team_id: Some(1),
                spec_id: Some(256),
            },
            CombatEvent::PlayerObserved {
                kind: PlayerObservationKind::AuraApplied,
                aura_type: Some(AuraType::Buff),
                spell_id: 123,
                guid: "Player-0-AAAA".into(),
                name: "Player One".into(),
                flags: 0x511,
                target_guid: "Player-0-BBBB".into(),
                target_name: "Player Two".into(),
                target_flags: 0x512,
                spell_name: "Aura, Tested".into(),
                owner_guid: None,
            },
            CombatEvent::Damage {
                source_guid: "Player-0-AAAA".into(),
                source_name: "Player One".into(),
                source_flags: 0x511,
                source_owner_guid: None,
                dest_guid: "Creature-0-BOSS".into(),
                dest_name: "Training Boss".into(),
                dest_flags: 0x10a48,
                dest_raid_marker: 0,
                spell_name: "Smite".into(),
                amount: 46,
                dest_current_hp: Some(105),
                dest_max_hp: Some(152),
            },
            CombatEvent::BossCast {
                source_name: "Training Boss".into(),
                spell_name: "Rebirth".into(),
            },
        ];
        assert_eq!(
            events.iter().map(|event| &event.event).collect::<Vec<_>>(),
            golden.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.occurred_at_ms)
                .collect::<Vec<_>>(),
            (0..fixture.len())
                .map(|index| 1_775_755_633_200 + index as i64 * 1_000)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn classic_shapes_allow_missing_combatant_details_and_short_deaths() {
        let fixture = [
            "10/16 12:44:44.182  ZONE_CHANGE,562,\"Blade's Edge Arena\",0",
            "10/16 12:44:45.182  COMBATANT_INFO,Player-0-CLASSIC",
            "10/16 12:44:46.182  SPELL_CAST_SUCCESS,Player-0-CLASSIC,\"Fighter-One\",0x511,0x0,Player-0-RIVAL,\"Fighter-Two\",0x548,0x0,123,\"Mortal Strike\",0x1",
            "10/16 12:44:47.182  UNIT_DIED,0,nil,0x0,0x0,Player-0-RIVAL,\"Fighter Two\",0x548,0x0",
            "10/16 12:44:48.182  ZONE_CHANGE,571,\"Dalaran\",0",
        ];
        let events = fixture
            .iter()
            .map(|line| {
                parse_line(GameFlavor::Classic, CONTEXT, line)
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let golden = vec![
            CombatEvent::ZoneChanged {
                zone_id: 562,
                difficulty_id: 0,
            },
            CombatEvent::Combatant {
                guid: "Player-0-CLASSIC".into(),
                team_id: None,
                spec_id: None,
            },
            CombatEvent::PlayerObserved {
                kind: PlayerObservationKind::CastSucceeded,
                aura_type: None,
                spell_id: 123,
                guid: "Player-0-CLASSIC".into(),
                name: "Fighter-One".into(),
                flags: 0x511,
                target_guid: "Player-0-RIVAL".into(),
                target_name: "Fighter-Two".into(),
                target_flags: 0x548,
                spell_name: "Mortal Strike".into(),
                owner_guid: None,
            },
            CombatEvent::UnitDied {
                guid: "Player-0-RIVAL".into(),
                name: "Fighter Two".into(),
                flags: 0x548,
                unconscious: false,
            },
            CombatEvent::ZoneChanged {
                zone_id: 571,
                difficulty_id: 0,
            },
        ];
        let golden = golden
            .into_iter()
            .enumerate()
            .map(|(index, event)| ParsedEvent {
                flavor: GameFlavor::Classic,
                occurred_at_ms: 1_792_147_484_182 + index as i64 * 1_000,
                event,
            })
            .collect::<Vec<_>>();
        assert_eq!(events, golden);
    }

    #[test]
    fn era_family_shapes_parse_explicit_year_and_encounter_sequence() {
        let fixture = [
            "3/24/2026 19:54:49.5171  ENCOUNTER_START,2940,\"Clockwork Keeper\",198,10,90,2",
            "3/24/2026 19:54:49.5171  COMBATANT_INFO,Player-0-ERA,0",
            "3/24/2026 19:54:50.5171  SPELL_CAST_SUCCESS,Player-0-ERA,\"Raider-One\",0x511,0x0,Creature-0-BOSS,\"Clockwork Keeper\",0x10a48,0x0,321,\"Storm Strike\",0x1",
            "3/24/2026 19:54:51.5171  UNIT_DIED,0,nil,0x0,0x0,Player-0-ERA,\"Raider One\",0x511,0x0,0",
            "3/24/2026 19:54:52.5171  ENCOUNTER_END,2940,\"Clockwork Keeper\",198,10,1",
        ];
        let era_context = ParseTimeContext::new(1999, -300);
        let events = fixture
            .iter()
            .map(|line| {
                parse_line(GameFlavor::Classic, era_context, line)
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let golden = vec![
            CombatEvent::EncounterStarted {
                encounter_id: 2940,
                name: "Clockwork Keeper".into(),
                difficulty_id: 198,
            },
            CombatEvent::Combatant {
                guid: "Player-0-ERA".into(),
                team_id: Some(0),
                spec_id: None,
            },
            CombatEvent::PlayerObserved {
                kind: PlayerObservationKind::CastSucceeded,
                aura_type: None,
                spell_id: 321,
                guid: "Player-0-ERA".into(),
                name: "Raider-One".into(),
                flags: 0x511,
                target_guid: "Creature-0-BOSS".into(),
                target_name: "Clockwork Keeper".into(),
                target_flags: 0x10a48,
                spell_name: "Storm Strike".into(),
                owner_guid: None,
            },
            CombatEvent::UnitDied {
                guid: "Player-0-ERA".into(),
                name: "Raider One".into(),
                flags: 0x511,
                unconscious: false,
            },
            CombatEvent::EncounterEnded {
                difficulty_id: 198,
                success: true,
            },
        ];
        // The lines carry UTC+1, which wins over the context's UTC-5.
        let timestamps = [
            1_774_378_489_517,
            1_774_378_489_517,
            1_774_378_490_517,
            1_774_378_491_517,
            1_774_378_492_517,
        ];
        let golden = golden
            .into_iter()
            .zip(timestamps)
            .map(|(event, occurred_at_ms)| ParsedEvent {
                flavor: GameFlavor::Classic,
                occurred_at_ms,
                event,
            })
            .collect::<Vec<_>>();
        assert_eq!(events, golden);
    }

    #[test]
    fn required_retail_combatant_fields_and_optional_death_flag_are_strict() {
        let prefix = "4/9 19:27:13.200  ";
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                &format!("{prefix}COMBATANT_INFO,Player-0-A,not-a-team")
            ),
            Err(ParseFailure::MalformedRetainedEvent)
        );
        let mut combatant = vec!["0"; 26];
        combatant[0] = "COMBATANT_INFO";
        combatant[1] = "Player-0-A";
        combatant[2] = "1";
        combatant[25] = "not-a-spec";
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                &format!("{prefix}{}", combatant.join(","))
            ),
            Err(ParseFailure::MalformedRetainedEvent)
        );
        assert!(matches!(
            parse_line(
                GameFlavor::Classic,
                CONTEXT,
                &format!("{prefix}COMBATANT_INFO,Player-0-A")
            )
            .unwrap()
            .unwrap()
            .event,
            CombatEvent::Combatant {
                team_id: None,
                spec_id: None,
                ..
            }
        ));
        assert!(matches!(
            parse_line(
                GameFlavor::Classic,
                CONTEXT,
                &format!("{prefix}UNIT_DIED,0,nil,0x0,0x0,Player-0-A,Name,0x511,0x0")
            )
            .unwrap()
            .unwrap()
            .event,
            CombatEvent::UnitDied {
                unconscious: false,
                ..
            }
        ));
        assert_eq!(
            parse_line(
                GameFlavor::Classic,
                CONTEXT,
                &format!("{prefix}UNIT_DIED,0,nil,0x0,0x0,Player-0-A,Name,0x511,0x0,invalid")
            ),
            Err(ParseFailure::MalformedRetainedEvent)
        );
    }

    #[test]
    fn timestamp_uses_the_utc_offset_the_line_carries() {
        // Every context here is deliberately wrong (a stale year and offset),
        // as it is after a DST change while the app keeps running.
        let at = |context, timestamp: &str| {
            parse_line(
                GameFlavor::Retail,
                context,
                &format!("{timestamp}  ZONE_CHANGE,1,Zone,0"),
            )
            .unwrap()
            .unwrap()
            .occurred_at_ms
        };
        let stale = ParseTimeContext::new(1999, 0);
        // Real lines from either side of the 2026 switch to winter time:
        // UTC+2 before it, UTC+1 after, written without a sign.
        assert_eq!(at(stale, "10/24/2026 13:05:47.7022"), 1_792_839_947_702);
        assert_eq!(at(CONTEXT, "10/25/2026 15:27:12.2731"), 1_792_938_432_273);
        // West of UTC the offset is signed.
        assert_eq!(at(CONTEXT, "8/19/2026 21:00:24.038-4"), 1_787_187_624_038);
        assert_eq!(at(CONTEXT, "8/19/2026 21:00:24.038+4"), 1_787_158_824_038);
        assert_eq!(at(stale, "7/30/2025 23:35:05.5863"), 1_753_907_705_586);
    }

    #[test]
    fn timestamp_without_a_usable_offset_uses_the_context() {
        let at = |timestamp: &str| {
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                &format!("{timestamp}  ZONE_CHANGE,1,Zone,0"),
            )
            .unwrap()
            .unwrap()
            .occurred_at_ms
        };
        // CONTEXT is UTC+2: no suffix, and suffixes that are not an hour.
        for timestamp in [
            "8/19/2026 21:00:24.038",
            "8/19/2026 21:00:24.038456",
            "8/19/2026 21:00:24.038-99",
            "8/19/2026 21:00:24.038+",
        ] {
            assert_eq!(at(timestamp), 1_787_166_024_038, "{timestamp}");
        }
        // A system zone off the hour keeps its own offset whatever the line says.
        let india = parse_timestamp("8/19/2026 21:00:24.0385", ParseTimeContext::new(2026, 330));
        assert_eq!(india.unwrap(), 1_787_153_424_038);
    }

    #[test]
    fn timestamp_rejects_invalid_calendar_dates_and_leap_seconds() {
        for timestamp in [
            "2/29/2025 12:00:00.000",
            "4/31/2026 12:00:00.000",
            "13/1/2026 12:00:00.000",
            "2/28/2026 12:00:60.000",
            "2/28/2026 12:00:00",
            "2/28/2026 12:00:00.",
            "2/28/2026 12:00:00.123garbage",
            "2/28/0 12:00:00.000",
            "2/28/10000 12:00:00.000",
            "2/28/999999999999999999999 12:00:00.000",
        ] {
            assert_eq!(
                parse_line(
                    GameFlavor::Retail,
                    CONTEXT,
                    &format!("{timestamp}  ZONE_CHANGE,1,Zone,0")
                ),
                Err(ParseFailure::MalformedTimestamp)
            );
        }
        assert!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                "2/29/2024 12:00:59.000  ZONE_CHANGE,1,Zone,0"
            )
            .is_ok()
        );
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                ParseTimeContext::new(i32::MAX, 0),
                "2/28 12:00:00.000  ZONE_CHANGE,1,Zone,0"
            ),
            Err(ParseFailure::MalformedTimestamp)
        );
    }

    #[test]
    fn quoted_and_nested_commas_do_not_split_top_level_fields() {
        let fields = split_fields("EVENT,\"one, two\",[1,(2,3)],\"a\\\"b\",\"c\"\"d\"").unwrap();
        assert_eq!(fields, ["EVENT", "one, two", "[1,(2,3)]", "a\"b", "c\"d"]);
    }

    #[test]
    fn unknown_event_is_ignored() {
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                "4/9 19:27:13.200  SPELL_BUILDING_DAMAGE,irrelevant"
            ),
            Ok(None)
        );
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                "not a timestamp  SPELL_BUILDING_DAMAGE,\"unterminated"
            ),
            Ok(None)
        );
        assert_eq!(
            parse_line(GameFlavor::Retail, CONTEXT, "irrelevant"),
            Ok(None)
        );
    }

    #[test]
    fn meter_events_without_a_readable_suffix_are_ignored() {
        // Advanced logging off: the amount field is unreadable, so the line is
        // skipped instead of diagnosed (the app warns about ACL separately).
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                "4/9 19:27:13.200  SPELL_DAMAGE,Player-0-A,\"A\",0x511,0x0,Creature-0-B,\"B\",0x10a48,0x0,123,\"Smite\",0x2"
            ),
            Ok(None)
        );
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                "4/9 19:27:13.200  SPELL_HEAL,irrelevant"
            ),
            Ok(None)
        );
    }

    #[test]
    fn real_acl_spell_damage_carries_amount_and_destination_hp() {
        let line = "5/24 20:26:10.911  SPELL_DAMAGE,Player-1322-07763A7B,\"Xiaohuli\",0x511,0x0,Creature-0-3013-0-11406-74284-0000266503,\"Cutpurse\",0x10a48,0x0,585,\"Smite\",0x2,Creature-0-3013-0-11406-74284-0000266503,0000000000000000,105,152,0,0,189,2084,0,0,0,0,0,0,0,0,0,46,0,2,0,0,0,1,0,0,0,0.000,1,1";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, line)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Damage {
                source_guid: "Player-1322-07763A7B".into(),
                source_name: "Xiaohuli".into(),
                source_flags: 0x511,
                source_owner_guid: None,
                dest_guid: "Creature-0-3013-0-11406-74284-0000266503".into(),
                dest_name: "Cutpurse".into(),
                dest_flags: 0x10a48,
                dest_raid_marker: 0,
                spell_name: "Smite".into(),
                amount: 46,
                dest_current_hp: Some(105),
                dest_max_hp: Some(152),
            }
        );
    }

    #[test]
    fn modern_and_old_heal_suffixes_pick_the_right_overheal_field() {
        let modern = "4/9 19:27:13.200  SPELL_HEAL,Player-0-A,\"Healer\",0x511,0x0,Player-0-B,\"Tank\",0x512,0x0,2061,\"Flash Heal\",0x2,Player-0-B,0000000000000000,500,500,0,0,0,0,0,0,0,0,0,0,0,0,0,1000,600,400,0,1";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, modern)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Heal {
                source_guid: "Player-0-A".into(),
                source_name: "Healer".into(),
                source_flags: 0x511,
                dest_guid: "Player-0-B".into(),
                dest_name: "Tank".into(),
                dest_flags: 0x512,
                dest_raid_marker: 0,
                spell_name: "Flash Heal".into(),
                amount: 1000,
                overheal: 400,
                dest_current_hp: Some(500),
                dest_max_hp: Some(500),
            }
        );
        let old = "4/9 19:27:13.200  SPELL_HEAL,Player-0-A,\"Healer\",0x511,0x0,Player-0-B,\"Tank\",0x512,0x0,2061,\"Flash Heal\",0x2,300,50,2,1";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, old)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Heal {
                source_guid: "Player-0-A".into(),
                source_name: "Healer".into(),
                source_flags: 0x511,
                dest_guid: "Player-0-B".into(),
                dest_name: "Tank".into(),
                dest_flags: 0x512,
                dest_raid_marker: 0,
                spell_name: "Flash Heal".into(),
                amount: 300,
                overheal: 50,
                dest_current_hp: None,
                dest_max_hp: None,
            }
        );
    }

    #[test]
    fn swing_damage_carries_the_source_owner() {
        let line = "4/9 19:27:13.200  SWING_DAMAGE,Pet-0-1,\"Imp\",0x2114,0x0,Creature-0-B,\"Training Boss\",0x10a48,0x0,Pet-0-1,Player-0-OWNER,500,1000,0,0,0,0,0,0,0,0,0,0,0,0,0,120,0,1,0,0,0,0,0,0,1";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, line)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Damage {
                source_guid: "Pet-0-1".into(),
                source_name: "Imp".into(),
                source_flags: 0x2114,
                source_owner_guid: Some("Player-0-OWNER".into()),
                dest_guid: "Creature-0-B".into(),
                dest_name: "Training Boss".into(),
                dest_flags: 0x10a48,
                dest_raid_marker: 0,
                spell_name: "Melee".into(),
                amount: 120,
                dest_current_hp: None,
                dest_max_hp: None,
            }
        );
    }

    #[test]
    fn cast_success_carries_the_source_owner_when_the_block_names_the_source() {
        let line = "4/9 19:27:13.200  SPELL_CAST_SUCCESS,Pet-0-1,\"Imp\",0x2114,0x0,Creature-0-B,\"Boss\",0x10a48,0x0,688,\"Firebolt\",0x4,Pet-0-1,Player-0-OWNER,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, line)
            .unwrap()
            .unwrap();
        assert!(matches!(
            &parsed.event,
            CombatEvent::PlayerObserved {
                kind: PlayerObservationKind::CastSucceeded,
                guid,
                owner_guid: Some(owner),
                ..
            } if guid == "Pet-0-1" && owner == "Player-0-OWNER"
        ));
    }

    #[test]
    fn interrupt_dispel_and_summon_parse_basic_fields() {
        let interrupt = "4/9 19:27:13.200  SPELL_INTERRUPT,Player-0-A,\"Rogue\",0x511,0x0,Creature-0-B,\"Caster\",0x10a48,0x0,1766,\"Kick\",0x1,133,\"Fireball\",0x4";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, interrupt)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Interrupt {
                source_guid: "Player-0-A".into(),
                source_name: "Rogue".into(),
                source_flags: 0x511,
                dest_name: "Caster".into(),
                dest_raid_marker: 0,
                spell_name: "Fireball".into(),
            }
        );

        let dispel = "4/9 19:27:13.200  SPELL_DISPEL,Player-0-A,\"Priest\",0x511,0x0,Player-0-B,\"Victim\",0x548,0x0,528,\"Dispel Magic\",0x1,1243,\"Power Word: Fortitude\",0x2,BUFF";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, dispel)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Dispel {
                source_guid: "Player-0-A".into(),
                source_name: "Priest".into(),
                source_flags: 0x511,
                dest_name: "Victim".into(),
                dest_raid_marker: 0,
                spell_name: "Power Word: Fortitude".into(),
            }
        );

        let summon = "4/9 19:27:13.200  SPELL_SUMMON,Player-0-A,\"Warlock\",0x511,0x0,Pet-0-IMP,\"Korlok\",0x2114,0x0,688,\"Summon Imp\",0x20";
        let parsed = parse_line(GameFlavor::Retail, CONTEXT, summon)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Summon {
                source_guid: "Player-0-A".into(),
                source_name: "Warlock".into(),
                source_flags: 0x511,
                pet_guid: "Pet-0-IMP".into(),
            }
        );
    }
    #[test]
    fn combat_log_version_reads_only_complete_header_values() {
        assert_eq!(
            combat_log_version(
                "8/11/2026 18:28:29.3992  COMBAT_LOG_VERSION,22,ADVANCED_LOG_ENABLED,1,BUILD_VERSION,12.1.0,PROJECT_ID,1"
            ),
            Some(22)
        );
        // Bare headers from older clients or hand-written fixtures also work.
        assert_eq!(
            combat_log_version(
                "COMBAT_LOG_VERSION,22,ADVANCED_LOG_ENABLED,1,BUILD_VERSION,12.0.0,PROJECT_ID,1"
            ),
            Some(22)
        );
        assert_eq!(
            combat_log_version("COMBAT_LOG_VERSION,garbage,ADVANCED_LOG_ENABLED,1"),
            None
        );
        assert_eq!(combat_log_version("COMBAT_LOG_VERSION"), None);
        assert_eq!(
            combat_log_version(
                "4/9 19:27:13.200  SPELL_DAMAGE,Player-0-A,\"A\",0x511,0x0,Creature-0-B,\"B\",0x10a48,0x0,123,\"Smite\",0x2"
            ),
            None
        );
    }

    #[test]
    fn version_22_amounts_live_at_the_wider_suffix() {
        let cases = [
            // (line, amount, overheal, source owner)
            (
                "5/24 20:26:10.911  SPELL_DAMAGE,Player-1322-07763A7B,\"Xiaohuli\",0x511,0x0,Creature-0-3013-0-11406-74284-0000266503,\"Cutpurse\",0x10a48,0x0,585,\"Smite\",0x2,Creature-0-3013-0-11406-74284-0000266503,0000000000000000,105,152,0,0,189,2084,0,0,0,250000,250000,0,0,0,0,0,0,46,0,2,0,0,0,1,0,0,0,0.000,1,1",
                46,
                0,
                None,
            ),
            (
                "4/9 19:27:13.200  SWING_DAMAGE,Pet-0-1,\"Imp\",0x2114,0x0,Creature-0-B,\"Training Boss\",0x10a48,0x0,Pet-0-1,Player-0-OWNER,500,1000,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,120,0,1,0,0,0,0,0,0,1",
                120,
                0,
                Some("Player-0-OWNER"),
            ),
            (
                "4/9 19:27:13.200  SPELL_HEAL,Player-0-A,\"Healer\",0x511,0x0,Player-0-B,\"Tank\",0x512,0x0,2061,\"Flash Heal\",0x2,Player-0-B,0000000000000000,500,500,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1000,600,400,0,1",
                1000,
                400,
                None,
            ),
        ];
        for (line, amount, overheal, owner) in cases {
            let event = parse_line(GameFlavor::Retail, V22_CONTEXT, line)
                .unwrap()
                .unwrap()
                .event;
            let actual = match event {
                CombatEvent::Damage {
                    amount,
                    source_owner_guid,
                    ..
                } => (amount, 0, source_owner_guid),
                CombatEvent::Heal {
                    amount, overheal, ..
                } => (amount, overheal, None),
                other => panic!("unexpected event {other:?}"),
            };
            assert_eq!(
                actual,
                (amount, overheal, owner.map(str::to_owned)),
                "{line}"
            );
        }
    }

    #[test]
    fn version_22_swing_support_keeps_spell_prefix_and_amount() {
        let line = "8/11/2026 18:32:21.3482  SWING_DAMAGE_LANDED_SUPPORT,Player-3682-0B8856AA,\"Rhenin-Ragnaros-EU\",0x512,0x80000000,Creature-0-B,\"Ymirjar Graveblade\",0x10a48,0x80000000,413984,\"Shifting Sands\",0x40,Creature-0-B,0000000000000000,20707592,20832478,0,0,1470,0,0,0,1,0,0,0,502.83,212.23,184,4.6665,91,292,201,-1,1,0,0,0,1,nil,nil,Player-3391-0CB9742F";
        let parsed = parse_line(GameFlavor::Retail, V22_CONTEXT, line)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Support {
                metric: MeterMetric::Damage,
                supporter_guid: "Player-3391-0CB9742F".into(),
                source_guid: "Player-3682-0B8856AA".into(),
                dest_name: "Ymirjar Graveblade".into(),
                dest_raid_marker: 0,
                spell_name: "Shifting Sands".into(),
                amount: 292,
                overheal: 0,
            }
        );
    }

    #[test]
    fn version_22_heal_support_reads_appended_supporter() {
        let line = "8/11/2026 18:32:18.7932  SPELL_HEAL_SUPPORT,Player-3682-0BBA26EE,\"Paulwalkerx-Ragnaros-EU\",0x511,0x80000000,Player-3682-0BBA26EE,\"Paulwalkerx-Ragnaros-EU\",0x511,0x80000000,413786,\"Fate Mirror\",0x40,Player-3682-0BBA26EE,0000000000000000,499731,499731,851,2750,884,300,546,0,17,43,120,0,488.27,218.90,184,0.0327,293,3088,3088,3088,0,nil,Player-3391-0CB9742F";
        let parsed = parse_line(GameFlavor::Retail, V22_CONTEXT, line)
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed.event,
            CombatEvent::Support {
                metric: MeterMetric::Healing,
                supporter_guid: "Player-3391-0CB9742F".into(),
                source_guid: "Player-3682-0BBA26EE".into(),
                dest_name: "Paulwalkerx-Ragnaros-EU".into(),
                dest_raid_marker: 0,
                spell_name: "Fate Mirror".into(),
                amount: 3088,
                overheal: 3088,
            }
        );
    }

    #[test]
    fn a_detected_block_with_an_unreadable_amount_is_malformed() {
        let prefix = "4/9 19:27:13.200  SPELL_DAMAGE,Player-0-A,\"A\",0x511,0x0,Creature-0-B,\"B\",0x10a48,0x0,123,\"Smite\",0x2,Creature-0-B,0000000000000000,100,200,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0";
        // The 19-field block is complete but the amount suffix is missing.
        assert_eq!(
            parse_line(GameFlavor::Retail, V22_CONTEXT, prefix),
            Err(ParseFailure::MalformedRetainedEvent)
        );
        // Same for an unparseable amount; without a block the line is still
        // merely ignored.
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                V22_CONTEXT,
                &format!("{prefix},overkill!")
            ),
            Err(ParseFailure::MalformedRetainedEvent)
        );
        assert_eq!(
            parse_line(
                GameFlavor::Retail,
                CONTEXT,
                "4/9 19:27:13.200  SPELL_DAMAGE,Player-0-A,\"A\",0x511,0x0,Creature-0-B,\"B\",0x10a48,0x0,123,\"Smite\",0x2"
            ),
            Ok(None)
        );
    }
}
