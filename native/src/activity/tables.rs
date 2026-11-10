// SPDX-License-Identifier: GPL-3.0-or-later

//! Factual data tables for the activity engine (ported from
//! `src/main/constants.ts`), plus the MD5 behind the activity hash.

use crate::domain::{GameFlavor, RaidDifficulty};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PartyType {
    Party,
    Raid,
    Pvp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MappedDifficulty {
    Lfr,
    Normal,
    Heroic,
    Mythic,
    Pvp,
}

pub(super) struct DifficultyInfo {
    mapped: MappedDifficulty,
    pub(super) short: &'static str,
    pub(super) party: PartyType,
}

impl DifficultyInfo {
    /// Ordered difficulty index; PvP has none and never meets a raid threshold.
    pub(super) fn order(&self) -> Option<u8> {
        match self.mapped {
            MappedDifficulty::Lfr => Some(0),
            MappedDifficulty::Normal => Some(1),
            MappedDifficulty::Heroic => Some(2),
            MappedDifficulty::Mythic => Some(3),
            MappedDifficulty::Pvp => None,
        }
    }
}

pub(super) fn difficulty_order(difficulty: &RaidDifficulty) -> u8 {
    match difficulty {
        RaidDifficulty::Lfr => 0,
        RaidDifficulty::Normal => 1,
        RaidDifficulty::Heroic => 2,
        RaidDifficulty::Mythic => 3,
    }
}

use MappedDifficulty::{Heroic, Lfr, Mythic, Normal, Pvp};
use PartyType::{Party, Pvp as PvpParty, Raid};

#[rustfmt::skip]
static INSTANCE_DIFFICULTY: &[(u32, DifficultyInfo)] = &[
    (1, DifficultyInfo { mapped: Normal, short: "N", party: Party }),
    (2, DifficultyInfo { mapped: Heroic, short: "HC", party: Party }),
    (3, DifficultyInfo { mapped: Normal, short: "10N", party: Raid }),
    (4, DifficultyInfo { mapped: Normal, short: "25N", party: Raid }),
    (5, DifficultyInfo { mapped: Heroic, short: "10HC", party: Raid }),
    (6, DifficultyInfo { mapped: Heroic, short: "25HC", party: Raid }),
    (7, DifficultyInfo { mapped: Lfr, short: "LFR", party: Raid }),
    (8, DifficultyInfo { mapped: Mythic, short: "Mythic Keystone", party: Party }),
    (9, DifficultyInfo { mapped: Normal, short: "40", party: Raid }),
    (14, DifficultyInfo { mapped: Normal, short: "N", party: Raid }),
    (15, DifficultyInfo { mapped: Heroic, short: "HC", party: Raid }),
    (16, DifficultyInfo { mapped: Mythic, short: "M", party: Raid }),
    (17, DifficultyInfo { mapped: Lfr, short: "LFR", party: Raid }),
    (23, DifficultyInfo { mapped: Mythic, short: "M", party: Party }),
    (24, DifficultyInfo { mapped: Normal, short: "T", party: Party }),
    (33, DifficultyInfo { mapped: Normal, short: "T", party: Raid }),
    (34, DifficultyInfo { mapped: Pvp, short: "PvP", party: PvpParty }),
    (150, DifficultyInfo { mapped: Normal, short: "N", party: Party }),
    (151, DifficultyInfo { mapped: Lfr, short: "T", party: Raid }),
    (175, DifficultyInfo { mapped: Normal, short: "10N", party: Raid }),
    (176, DifficultyInfo { mapped: Normal, short: "25N", party: Raid }),
    (185, DifficultyInfo { mapped: Normal, short: "N", party: Raid }),
    (186, DifficultyInfo { mapped: Normal, short: "N", party: Raid }),
    (193, DifficultyInfo { mapped: Heroic, short: "10HC", party: Raid }),
    (194, DifficultyInfo { mapped: Heroic, short: "25HC", party: Raid }),
    (198, DifficultyInfo { mapped: Normal, short: "10N", party: Raid }),
    (215, DifficultyInfo { mapped: Normal, short: "10N", party: Raid }),
    (226, DifficultyInfo { mapped: Normal, short: "N", party: Raid }),
    (233, DifficultyInfo { mapped: Mythic, short: "M", party: Raid }),
];

pub(super) fn difficulty_info(id: u32) -> Option<&'static DifficultyInfo> {
    INSTANCE_DIFFICULTY
        .iter()
        .find(|(entry_id, _)| *entry_id == id)
        .map(|(_, info)| info)
}

pub(super) struct RaidInstance {
    zone_id: u32,
    pub(super) name: &'static str,
    pub(super) short_name: &'static str,
    encounters: &'static [u32],
}

static UNKNOWN_RAID: RaidInstance = RaidInstance {
    zone_id: 0,
    name: "Unknown Raid",
    short_name: "Unknown Raid",
    encounters: &[],
};

/// Every raid by the encounter ids the game logs, from the client's
/// `DungeonEncounter` table (zone id = the raid's `AreaTable` row). Classic
/// clients log the same ids; the trailing 6xx/7xx ids on the Wrath raids are
/// the ones Wrath Classic logged instead.
#[rustfmt::skip]
static RAID_INSTANCES: &[RaidInstance] = &[
    // Classic
    RaidInstance { zone_id: 2159, name: "Onyxia's Lair", short_name: "Onyxia", encounters: &[1084] },
    RaidInstance { zone_id: 2717, name: "Molten Core", short_name: "Molten Core", encounters: &[663, 664, 665, 666, 667, 668, 669, 670, 671, 672, 3018] },
    RaidInstance { zone_id: 2677, name: "Blackwing Lair", short_name: "Blackwing Lair", encounters: &[610, 611, 612, 613, 614, 615, 616, 617] },
    RaidInstance { zone_id: 1977, name: "Zul'Gurub", short_name: "Zul'Gurub", encounters: &[785, 784, 786, 787, 788, 789, 790, 791, 792, 793] },
    RaidInstance { zone_id: 3429, name: "Ruins of Ahn'Qiraj", short_name: "AQ20", encounters: &[718, 719, 720, 721, 722, 723] },
    RaidInstance { zone_id: 3428, name: "Temple of Ahn'Qiraj", short_name: "AQ40", encounters: &[709, 710, 711, 712, 713, 714, 715, 716, 717] },
    RaidInstance { zone_id: 16236, name: "Scarlet Enclave", short_name: "Scarlet Enclave", encounters: &[3185, 3187, 3196, 3186, 3197, 3188, 3190, 3189] },
    // The Burning Crusade
    RaidInstance { zone_id: 3457, name: "Karazhan", short_name: "Karazhan", encounters: &[652, 653, 654, 655, 656, 657, 658, 659, 660, 661, 662] },
    RaidInstance { zone_id: 3923, name: "Gruul's Lair", short_name: "Gruul", encounters: &[649, 650] },
    RaidInstance { zone_id: 3836, name: "Magtheridon's Lair", short_name: "Magtheridon", encounters: &[651] },
    RaidInstance { zone_id: 3607, name: "Serpentshrine Cavern", short_name: "Serpentshrine", encounters: &[623, 624, 625, 626, 627, 628] },
    RaidInstance { zone_id: 3845, name: "Tempest Keep", short_name: "Tempest Keep", encounters: &[730, 731, 732, 733] },
    RaidInstance { zone_id: 3606, name: "Hyjal Summit", short_name: "Hyjal", encounters: &[618, 619, 620, 621, 622] },
    RaidInstance { zone_id: 3959, name: "Black Temple", short_name: "Black Temple", encounters: &[601, 602, 603, 604, 605, 606, 607, 608, 609] },
    RaidInstance { zone_id: 3805, name: "Zul'Aman", short_name: "Zul'Aman", encounters: &[1189, 1190, 1191, 1192, 1193, 1194] },
    RaidInstance { zone_id: 4075, name: "Sunwell Plateau", short_name: "Sunwell", encounters: &[724, 725, 726, 727, 728, 729] },
    // Wrath of the Lich King
    RaidInstance { zone_id: 3456, name: "Naxxramas", short_name: "Naxxramas", encounters: &[1107, 1110, 1116, 1117, 1112, 1115, 1113, 1109, 1121, 1118, 1111, 1108, 1120, 1119, 1114] },
    RaidInstance { zone_id: 4500, name: "Eye of Eternity", short_name: "EoE", encounters: &[1094, 734] },
    RaidInstance { zone_id: 4493, name: "Obsidian Sanctum", short_name: "OS", encounters: &[1093, 1092, 1091, 1090, 742] },
    RaidInstance { zone_id: 4603, name: "Vault of Archavon", short_name: "VoA", encounters: &[1126, 1127, 1128, 1129, 772] },
    RaidInstance { zone_id: 4273, name: "Ulduar", short_name: "Ulduar", encounters: &[1132, 1136, 1139, 1142, 1140, 1137, 1131, 1135, 1141, 1164, 1165, 1166, 1133, 1138, 1134, 1143, 1130, 744, 745, 746, 747, 748, 749, 750, 751, 752, 753, 754, 755, 756, 757] },
    RaidInstance { zone_id: 4722, name: "Trial of the Crusader", short_name: "ToC", encounters: &[1088, 1087, 1086, 1089, 1085, 629, 633, 637, 641, 645] },
    RaidInstance { zone_id: 4812, name: "Icecrown Citadel", short_name: "ICC", encounters: &[1101, 1100, 1099, 1096, 1104, 1097, 1102, 1095, 1103, 1098, 1105, 1106] },
    RaidInstance { zone_id: 4987, name: "Ruby Sanctum", short_name: "RS", encounters: &[1147, 1149, 1148, 1150] },
    // Cataclysm
    RaidInstance { zone_id: 5600, name: "Baradin Hold", short_name: "Baradin Hold", encounters: &[1033, 1250, 1332] },
    RaidInstance { zone_id: 5094, name: "Blackwing Descent", short_name: "Blackwing Descent", encounters: &[1027, 1024, 1022, 1023, 1025, 1026] },
    RaidInstance { zone_id: 5334, name: "The Bastion of Twilight", short_name: "Bastion of Twilight", encounters: &[1030, 1032, 1028, 1029, 1082, 1083] },
    RaidInstance { zone_id: 5638, name: "Throne of the Four Winds", short_name: "Four Winds", encounters: &[1035, 1034] },
    RaidInstance { zone_id: 5723, name: "Firelands", short_name: "Firelands", encounters: &[1197, 1204, 1205, 1206, 1200, 1185, 1203] },
    RaidInstance { zone_id: 5892, name: "Dragon Soul", short_name: "Dragon Soul", encounters: &[1292, 1294, 1295, 1296, 1297, 1298, 1291, 1299] },
    // Mists of Pandaria
    RaidInstance { zone_id: 6125, name: "Mogu'shan Vaults", short_name: "Mogu'shan Vaults", encounters: &[1395, 1390, 1434, 1436, 1500, 1407] },
    RaidInstance { zone_id: 6297, name: "Heart of Fear", short_name: "Heart of Fear", encounters: &[1507, 1504, 1463, 1498, 1499, 1501] },
    RaidInstance { zone_id: 6067, name: "Terrace of Endless Spring", short_name: "Terrace", encounters: &[1409, 1505, 1506, 1431] },
    RaidInstance { zone_id: 6622, name: "Throne of Thunder", short_name: "Throne of Thunder", encounters: &[1577, 1575, 1570, 1565, 1578, 1573, 1572, 1574, 1576, 1559, 1560, 1579, 1580, 1581] },
    RaidInstance { zone_id: 6738, name: "Siege of Orgrimmar", short_name: "Siege of Orgrimmar", encounters: &[1602, 1598, 1624, 1604, 1622, 1600, 1606, 1603, 1595, 1594, 1599, 1601, 1593, 1623, 1605] },
    // Warlords of Draenor
    RaidInstance { zone_id: 6996, name: "Highmaul", short_name: "Highmaul", encounters: &[1721, 1706, 1720, 1722, 1719, 1723, 1705] },
    RaidInstance { zone_id: 6967, name: "Blackrock Foundry", short_name: "Foundry", encounters: &[1696, 1691, 1693, 1694, 1689, 1692, 1690, 1713, 1695, 1704] },
    RaidInstance { zone_id: 7545, name: "Hellfire Citadel", short_name: "Hellfire Citadel", encounters: &[1778, 1785, 1787, 1798, 1786, 1783, 1788, 1794, 1777, 1800, 1784, 1795, 1799] },
    // Legion
    RaidInstance { zone_id: 8026, name: "The Emerald Nightmare", short_name: "Emerald Nightmare", encounters: &[1853, 1841, 1873, 1854, 1876, 1877, 1864] },
    RaidInstance { zone_id: 8440, name: "Trial of Valor", short_name: "Trial of Valor", encounters: &[1958, 1962, 2008] },
    RaidInstance { zone_id: 8025, name: "The Nighthold", short_name: "Nighthold", encounters: &[1849, 1865, 1867, 1871, 1862, 1886, 1842, 1863, 1872, 1866] },
    RaidInstance { zone_id: 8524, name: "Tomb of Sargeras", short_name: "Tomb of Sargeras", encounters: &[2032, 2048, 2036, 2050, 2037, 2054, 2052, 2038, 2051] },
    RaidInstance { zone_id: 8638, name: "Antorus, the Burning Throne", short_name: "Antorus", encounters: &[2076, 2074, 2064, 2070, 2075, 2082, 2088, 2069, 2073, 2063, 2092] },
    // Battle for Azeroth
    RaidInstance { zone_id: 9389, name: "Uldir", short_name: "Uldir", encounters: &[2144, 2141, 2136, 2128, 2134, 2145, 2135, 2122] },
    RaidInstance { zone_id: 10076, name: "Battle of Dazar'alor", short_name: "Dazar'alor", encounters: &[2265, 2263, 2266, 2285, 2284, 2271, 2268, 2272, 2276, 2280, 2281] },
    RaidInstance { zone_id: 10057, name: "Crucible of Storms", short_name: "Crucible", encounters: &[2269, 2273] },
    RaidInstance { zone_id: 10425, name: "The Eternal Palace", short_name: "Eternal Palace", encounters: &[2298, 2305, 2289, 2304, 2303, 2311, 2293, 2299] },
    RaidInstance { zone_id: 10522, name: "Ny'alotha, the Waking City", short_name: "Ny'alotha", encounters: &[2329, 2334, 2327, 2328, 2336, 2333, 2331, 2335, 2343, 2345, 2337, 2344] },
    // Shadowlands
    RaidInstance { zone_id: 13224, name: "Castle Nathria", short_name: "Nathria", encounters: &[2398, 2418, 2402, 2383, 2405, 2406, 2412, 2399, 2417, 2407] },
    RaidInstance { zone_id: 13561, name: "Sanctum of Domination", short_name: "Sanctum", encounters: &[2423, 2433, 2429, 2434, 2432, 2430, 2436, 2431, 2422, 2435] },
    RaidInstance { zone_id: 13742, name: "Sepulcher of the First Ones", short_name: "Sepulcher", encounters: &[2512, 2540, 2553, 2544, 2542, 2529, 2539, 2546, 2543, 2549, 2537] },
    // Dragonflight
    RaidInstance { zone_id: 14030, name: "Vault of the Incarnates", short_name: "Vault", encounters: &[2587, 2590, 2635, 2639, 2592, 2605, 2614, 2607] },
    RaidInstance { zone_id: 14663, name: "Aberrus, the Shadowed Crucible", short_name: "Aberrus", encounters: &[2688, 2687, 2693, 2682, 2680, 2689, 2683, 2684, 2685] },
    RaidInstance { zone_id: 14643, name: "Amirdrassil, the Dream's Hope", short_name: "Amirdrassil", encounters: &[2820, 2709, 2737, 2731, 2728, 2708, 2824, 2786, 2677] },
    // The War Within
    RaidInstance { zone_id: 14980, name: "Nerub-ar Palace", short_name: "Nerub-ar Palace", encounters: &[2902, 2917, 2898, 2918, 2919, 2920, 2921, 2922] },
    RaidInstance { zone_id: 15913, name: "Blackrock Depths", short_name: "Blackrock Depths", encounters: &[3042, 3044, 3043, 3046, 3048, 3045, 3047, 3049] },
    RaidInstance { zone_id: 15522, name: "Liberation of Undermine", short_name: "Undermine", encounters: &[3009, 3010, 3011, 3012, 3013, 3014, 3015, 3016] },
    RaidInstance { zone_id: 16178, name: "Manaforge Omega", short_name: "Manaforge", encounters: &[3129, 3131, 3130, 3132, 3122, 3133, 3134, 3135] },
    // Midnight
    RaidInstance { zone_id: 16279, name: "Sporefall", short_name: "Sporefall", encounters: &[3159] },
    RaidInstance { zone_id: 16340, name: "The Voidspire", short_name: "Voidspire", encounters: &[3176, 3177, 3179, 3178, 3180, 3181] },
    RaidInstance { zone_id: 16531, name: "The Dreamrift", short_name: "Dreamrift", encounters: &[3306] },
    RaidInstance { zone_id: 16342, name: "March on Quel'Danas", short_name: "Quel'Danas", encounters: &[3182, 3183] },
    RaidInstance { zone_id: 16915, name: "The Venomous Abyss", short_name: "Venomous Abyss", encounters: &[3470, 3445, 3497, 3455, 3420, 3421, 3429, 3492] },
    RaidInstance { zone_id: 16671, name: "The Tidebound Grotto", short_name: "Tidebound Grotto", encounters: &[3379] },
    RaidInstance { zone_id: 17059, name: "The Unbinding of Kith'ix", short_name: "Kith'ix", encounters: &[3513] },
];

fn known_raid(encounter_id: u32) -> Option<&'static RaidInstance> {
    RAID_INSTANCES
        .iter()
        .find(|raid| raid.encounters.contains(&encounter_id))
}

pub(super) fn raid_lookup(encounter_id: u32) -> &'static RaidInstance {
    known_raid(encounter_id).unwrap_or(&UNKNOWN_RAID)
}

pub(super) fn raid_zone_id(encounter_id: u32) -> u32 {
    raid_lookup(encounter_id).zone_id
}

/// The raid an encounter belongs to, as the library shows it.
pub(crate) fn raid_short_name(encounter_id: u32) -> Option<&'static str> {
    known_raid(encounter_id).map(|raid| raid.short_name)
}

pub(super) static CURRENT_RETAIL_ENCOUNTERS: &[u32] = &[
    3176, 3177, 3178, 3179, 3180, 3181, 3182, 3183, 3306, 3159, 3470, 3445, 3455, 3497, 3420, 3421,
    3429, 3492, 3379, 3513, 9999,
];

#[rustfmt::skip]
static DUNGEON_ENCOUNTERS: &[(u32, &str)] = &[
    (1715, "Rocketspark and Borka"), (1732, "Nitrogg Thundertower"), (1736, "Skylord Tovra"),
    (1748, "Grimrail Enforcers"), (1749, "Fleshrender Nok'gar"), (1750, "Oshir"), (1754, "Skulloc, Son of Gruul"),
    (1954, "Maiden of Virtue"), (1957, "Opera Hall"), (1960, "Attumen the Huntsman"), (1961, "Moroes"),
    (1964, "The Curator"), (1959, "Mana Devourer"), (1965, "Shade of Medivh"), (2017, "Viz'aduum the Watcher"),
    (2257, "Tussle Tonks"), (2258, "K.U.-J.0."), (2259, "Machinist's Garden"), (2260, "King Mechagon"),
    (2290, "King Gobbamak"), (2291, "HK-8 Aerial Oppression Unit"), (2292, "Gunker"), (2312, "Trixie & Naeno"),
    (2356, "Ventunax"), (2357, "Kin-Tara"), (2358, "Oryphrion"), (2359, "Devos, Paragon of Loyalty"),
    (2360, "Kryxis the Voracious"), (2361, "Executor Tarvold"), (2362, "Grand Proctor Beryllia"), (2363, "General Kaal"),
    (2364, "Kul'tharok"), (2365, "Gorechop"), (2366, "Xav the Unfallen"), (2391, "An Affront of Challengers"), (2404, "Mordretha"),
    (2380, "Echelon"), (2381, "Lord Chamberlain"), (2401, "Halkias, the Sin-Stained Goliath"), (2403, "High Adjudicator Aleez"),
    (2382, "Globgrog"), (2384, "Doctor Ickus"), (2385, "Domina Venomblade"), (2386, "Stradama Margrave"),
    (2387, "Blightbone"), (2388, "Amarth, The Harvester"), (2389, "Surgeon Stitchflesh"), (2390, "Nalthor the Rimebinder"),
    (2394, "The Manastorms"), (2395, "Hakkar, the Soulflayer"), (2396, "Mueh'zala"), (2400, "Dealer Xy'exa"),
    (2397, "Ingra Maloch"), (2392, "Mistcaller"), (2393, "Tred'ova"),
    (2419, "Timecap'n Hooktail"), (2426, "Hylbrande"), (2442, "So'leah"),
    (2424, "Mailroom Mayhem"), (2425, "Zo'phex the Sentinel"), (2441, "The Grand Menagerie"), (2437, "So'azmi"), (2440, "Myza's Oasis"),
    (2609, "Melidrussa Chillworn"), (2606, "Kokia Blazehoof"), (2623, "Kyrakka and Erhkard Stormvein"),
    (2637, "Granyth"), (2636, "The Raging Tempest"), (2581, "Teera and Maruuk"), (2580, "Balakar Khan"),
    (2582, "Leymor"), (2585, "Azureblade"), (2583, "Telash Greywing"), (2584, "Umbrelskul"),
    (2562, "Vexamus"), (2563, "Overgrown Ancient"), (2564, "Crawth"), (2565, "Echo of Doragosa"),
    (1805, "Hymdall"), (1806, "Hyrja"), (1807, "Fenryr"), (1808, "God-King Skovald"), (1809, "Odyn"),
    (1868, "Patrol Captain Gerdo"), (1869, "Talixae Flamewreath"), (1870, "Advisor Melandrus"),
    (1677, "Sadana Bloodfury"), (1688, "Nhallish"), (1679, "Bonemaw"), (1682, "Ner'zhul"),
    (1418, "Wise Mari"), (1417, "Lorewalker Stonestep"), (1416, "Liu Flameheart"), (1439, "Sha of Doubt"),
    (2570, "Hackclaw's War-Band"), (2567, "Gutshot"), (2568, "Treemouth"), (2569, "Decatriarch Wratheye"),
    (2615, "Watcher Irideus"), (2616, "Gulping Goliath"), (2617, "Khajin the Unyielding"), (2618, "Primal Tsunami"),
    (2555, "The Lost Dwarves"), (2556, "Bromach"), (2557, "Sentinel Talondras"), (2558, "Emberon"), (2559, "Chrono-Lord Deios"),
    (2610, "Magmatusk"), (2611, "Warlord Sargha"), (2612, "Forgemaster Gorek"), (2613, "Chargath, Bane of Scales"),
    (2093, "Skycap'n Kragg"), (2094, "Council o' Captains"), (2095, "Ring of Booty"), (2096, "Harlan Sweete"),
    (2111, "Elder Leaxa"), (2118, "Cragmaw the Infested"), (2112, "Sporecaller Zancha"), (2123, "Unbound Abomination"),
    (1790, "Rokmora"), (1791, "Ularogg Cragshaper"), (1792, "Naraxas"), (1793, "Dargrul the Underking"),
    (1041, "Altairus"), (1042, "Asaad, Caliph of Zephyrs"), (1043, "Grand Vizier Ertan"),
    (2113, "Heartsbane Triad"), (2114, "Soulbound Goliath"), (2115, "Raal the Gluttonous"), (2116, "Lord and Lady Waycrest"), (2117, "Gorak Tul"),
    (1832, "The Amalgam of Souls"), (1833, "Illysanna Ravencrest"), (1834, "Smashspite the Hateful"), (1835, "Lord Kur'talos Ravencrest"),
    (1045, "Lady Naz'jar"), (1044, "Commander Ulthok, the Festering Prince"), (1046, "Mindbender Ghur'sha"), (1047, "Ozumat"),
    (1746, "Witherbark"), (1757, "Ancient Protectors"), (1751, "Archmage Sol"), (1756, "Yalnu"),
    (2084, "Priestess Alun'za"), (2086, "Rezan"), (2085, "Vol'kaal"), (2087, "Yazma"),
    (2666, "Chronikar"), (2667, "Manifested Timeways"), (2668, "Blight of Galakrond"), (2669, "Iridikron the Stonescaled"),
    (2670, "Tyr, the Infinite Keeper"), (2671, "Morchie"), (2672, "Time-Lost Battlefield"), (2673, "Chrono-Lord Deios"),
    (1836, "Archdruid Glaidalis"), (1837, "Oakheart"), (1838, "Dresaron"), (1839, "Shade of Xavius"),
    (2854, "E.D.N.A"), (2880, "Skarmorak"), (2888, "Master Machinists"), (2883, "Void Speaker Eirich"),
    (2837, "Speaker Shadowcrown"), (2838, "Anub'ikkaj"), (2839, "Rasha'nan"),
    (2907, "Orator Krix'vizk"), (2908, "Fangs of the Queen"), (2905, "The Coaglamation"), (2909, "Izo, the Grand Splicer"),
    (2926, "Avanoxx"), (2906, "Anub'zekt"), (2901, "Ki'katal the Harvester"),
    (2098, "Chopper Redhook"), (2109, "Dread Captain Lockwood"), (2099, "Hadal Darkfathom"), (2100, "Viq'Goth"),
    (1051, "General Umbriss"), (1050, "Forgemaster Throngus"), (1048, "Drahga Shadowburner"), (1049, "Erudax, the Duke of Below"),
    (2847, "Captain Dailcry"), (2835, "Baron Braunpyke"), (2848, "Prioress Murrpray"),
    (2829, "Ol' Waxbeard"), (2826, "Blazikon"), (2787, "The Candle King"), (2788, "The Darkness"),
    (2816, "Kyrioss"), (2861, "Stormguard Gorren"), (2836, "Voidstone Monstrosity"),
    (2900, "Brew Master Aldryr"), (2929, "I'pa"), (2931, "Benk Buzzbee"), (2930, "Goldie Baronbottom"),
    (3020, "Big M.O.M.M.A."), (3054, "Geezle Gigazap"), (3053, "Swampface"), (3019, "Demolition Duo"),
    (2105, "Coin-Operated Crowd Pummeler"), (2106, "Azerokk"), (2107, "Rixxa Fluxflame"), (2108, "Mogul Razdunk"),
    (3107, "Azhiccar"), (3108, "Taah'bat and A'wazj"), (3109, "Soul-Scribe"),
    (3071, "Arcanotron Custos"), (3073, "Gemellus"), (3072, "Seranel Sunlash"), (3074, "Degentrius"),
    (3212, "Muro'jin and Nekraxx"), (3214, "Rak'tul, Vessel of Souls"), (3213, "Vordaza"),
    (3328, "Chief Corewright Kasreth"), (3332, "Corewarden Nysarra"), (3333, "Lothraxion"),
    (3058, "Commander Kroluk"), (3057, "Derelict Duo"), (3056, "Emberdawn"), (3059, "The Restless Heart"),
    (1999, "Forgemaster Garfrost"), (2001, "Ick and Krick"), (2000, "Scourgelord Tyrannus"),
    (2068, "L'ura"), (2066, "Saprish"), (2067, "Viceroy Nezhar"), (2065, "Zuraal the Ascended"),
    (1699, "Araknath"), (1701, "High Sage Viryx"), (1698, "Ranjit"), (1700, "Rukhran"),
    (3101, "Kystia Manaheart"), (3102, "Zaen Bladesorrow"), (3103, "Xathuux the Annihilator"), (3105, "Lithiel Cinderfury"),
    (3207, "The Hoardmonger"), (3208, "Sentinel of Winter"), (3209, "Nalorakk"),
    (3199, "Lightblossom Trinity"), (3200, "Ikuzz the Light Hunter"), (3201, "Lightwarden Ruia"), (3202, "Ziekket"),
    (3285, "Taz'Rah"), (3286, "Atroxus"), (3287, "Charonus"),
    (3456, "Rav'i"), (3457, "The Writhing Coil"), (3458, "Zul'jan"),
    (2124, "Adderis and Aspix"), (2125, "Merektha"), (2126, "Galvazzt"), (2127, "Avatar of Sethraliss"),
    (2139, "The Golden Serpent"), (2142, "Mchimba the Embalmer"), (2140, "The Council of Tribes"), (2143, "Dazar, The First King"),
];

pub(super) fn dungeon_encounter_name(encounter_id: u32) -> Option<&'static str> {
    DUNGEON_ENCOUNTERS
        .iter()
        .find(|(id, _)| *id == encounter_id)
        .map(|(_, name)| *name)
}

/// Retail keystone timers in seconds `[one, two, three] chest`.
#[rustfmt::skip]
static DUNGEON_TIMERS: &[(u32, [f64; 3])] = &[
    (377, [2580.0, 2065.0, 1549.0]),
    (378, [1920.0, 1536.0, 1152.0]),
    (375, [1800.0, 1440.0, 1080.0]),
    (379, [2280.0, 1824.0, 1358.0]),
    (380, [2460.0, 1968.0, 1476.0]),
    (381, [2340.0, 1872.0, 1404.0]),
    (376, [1950.0, 1578.0, 1206.0]),
    (382, [2040.0, 1632.0, 1224.0]),
    (227, [2520.0, 2016.0, 1512.0]),
    (234, [2100.0, 1680.0, 1260.0]),
    (369, [2280.0, 1824.0, 1358.0]),
    (370, [1920.0, 1536.0, 1152.0]),
    (391, [2100.0, 1680.0, 1260.0]),
    (392, [1800.0, 1440.0, 1080.0]),
    (169, [1800.0, 1440.0, 1080.0]),
    (166, [1800.0, 1440.0, 1080.0]),
    (399, [1680.0, 1344.0, 1008.0]),
    (400, [2400.0, 1920.0, 1440.0]),
    (401, [2250.0, 1800.0, 1350.0]),
    (200, [2280.0, 1824.0, 1428.0]),
    (210, [1800.0, 1440.0, 1080.0]),
    (165, [1980.0, 1584.0, 1188.0]),
    (2, [1800.0, 1440.0, 1080.0]),
    (405, [2100.0, 1680.0, 1260.0]),
    (406, [2100.0, 1680.0, 1260.0]),
    (403, [2100.0, 1680.0, 1260.0]),
    (404, [1980.0, 1584.0, 1188.0]),
    (245, [1800.0, 1440.0, 1080.0]),
    (251, [1800.0, 1440.0, 1080.0]),
    (206, [1980.0, 1584.0, 1188.0]),
    (438, [1800.0, 1440.0, 1080.0]),
    (463, [2040.0, 1632.0, 1224.0]),
    (464, [2160.0, 1680.0, 1260.0]),
    (248, [2200.0, 1760.0, 1320.0]),
    (198, [1800.0, 1440.0, 1080.0]),
    (199, [2160.0, 1728.0, 1404.0]),
    (244, [1800.0, 1440.0, 1080.0]),
    (168, [1980.0, 1584.0, 1188.0]),
    (456, [2040.0, 1632.0, 1224.0]),
    (501, [1980.0, 1584.0, 1188.0]),
    (503, [1800.0, 1440.0, 1080.0]),
    (353, [1980.0, 1584.0, 1188.0]),
    (502, [2100.0, 1680.0, 1260.0]),
    (505, [1860.0, 1488.0, 1116.0]),
    (507, [2040.0, 1632.0, 1224.0]),
    (506, [1980.0, 1584.0, 1188.0]),
    (504, [1860.0, 1464.0, 1128.0]),
    (500, [1740.0, 1404.0, 1068.0]),
    (499, [1950.0, 1560.0, 1170.0]),
    (525, [1980.0, 1584.0, 1188.0]),
    (247, [1980.0, 1584.0, 1188.0]),
    (542, [1860.0, 1488.0, 1116.0]),
    (558, [2040.0, 1632.0, 1224.0]),
    (560, [1980.0, 1584.0, 1188.0]),
    (559, [1800.0, 1440.0, 1080.0]),
    (557, [2010.0, 1608.0, 1206.0]),
    (402, [1860.0, 1488.0, 1116.0]),
    (556, [1800.0, 1440.0, 1080.0]),
    (239, [2040.0, 1632.0, 1224.0]),
    (161, [1680.0, 1356.0, 1002.0]),
    (587, [2040.0, 1632.0, 1224.0]),
    (586, [1920.0, 1536.0, 1152.0]),
    (584, [1800.0, 1440.0, 1080.0]),
    (585, [1800.0, 1440.0, 1080.0]),
    (588, [1800.0, 1440.0, 1080.0]),
    (250, [1920.0, 1536.0, 1152.0]),
    (249, [1980.0, 1584.0, 1188.0]),
];

pub(super) fn dungeon_timers(map_id: u32) -> Option<&'static [f64]> {
    DUNGEON_TIMERS
        .iter()
        .find(|(id, _)| *id == map_id)
        .map(|(_, timers)| &timers[..])
}

/// Dungeon zone names for Mythic+ metadata (`dungeonsByZoneId`).
#[rustfmt::skip]
static DUNGEONS_BY_ZONE_ID: &[(u32, &str)] = &[
    (1651, "Return to Karazhan"), (1208, "Grimrail Depot"), (1195, "Iron Docks"),
    (2097, "Operation: Mechagon"), (2291, "De Other Side"), (2287, "Halls of Atonement"),
    (2290, "Mists of Tirna Scithe"), (2289, "Plaguefall"), (2284, "Sanguine Depths"),
    (2285, "Spires of Ascension"), (2286, "The Necrotic Wake"), (2293, "Theater of Pain"),
    (2441, "Tazavesh the Veiled Market"),
    (2521, "Ruby Life Pools"), (2516, "The Nokhud Offensive"), (2515, "The Azure Vault"),
    (2526, "Algeth'ar Academy"), (1477, "Halls of Valor"), (1571, "Court of Stars"),
    (1176, "Shadowmoon Burial Grounds"), (960, "Temple of the Jade Serpent"),
    (2520, "Brackenhide Hollow"), (2527, "Halls of Infusion"), (2451, "Uldaman: Legacy of Tyr"),
    (2519, "Neltharus"), (1754, "Freehold"), (1841, "The Underrot"), (1458, "Neltharion's Lair"),
    (657, "The Vortex Pinnacle"),
    (2579, "Dawn of the Infinite"), (1862, "Waycrest Manor"), (1466, "Darkheart Thicket"),
    (1501, "Black Rook Hold"), (1763, "Atal'Dazar"), (1279, "The Everbloom"), (643, "Throne of the Tides"),
    (670, "Grim Batol"), (1822, "Siege of Boralus"), (2652, "The Stonevault"),
    (2660, "Ara-Kara, City of Echoes"), (2662, "The Dawnbreaker"), (2669, "City of Threads"),
    (2649, "Priory of the Sacred Flame"), (2651, "Darkflame Cleft"), (2648, "The Rookery"),
    (2661, "Cinderbrew Meadery"), (2773, "Operation: Floodgate"), (1594, "THE MOTHERLODE!!"),
    (2830, "Eco-Dome Al'Dani"),
    (2805, "Windrunner Spire"), (2811, "Magisters' Terrace"), (2874, "Maisara Caverns"),
    (2915, "Nexus-Point Xenas"), (658, "Pit of Saron"), (1209, "Skyreach"),
    (1753, "Seat of the Triumvirate"),
    (2813, "Murder Row"), (2825, "Den of Nalorakk"), (2859, "The Blinding Vale"),
    (2923, "Voidscar Arena"), (2993, "Altar of Fangs"), (1877, "Temple of Sethraliss"),
    (1762, "Kings' Rest"),
];

#[rustfmt::skip]
static RETAIL_BATTLEGROUNDS: &[(u32, &str)] = &[
    (30, "Alterac Valley"), (2107, "Arathi Basin"), (1681, "Arathi Basin"),
    (1105, "Deepwind Gorge"), (2245, "Deepwind Gorge"), (566, "Eye of the Storm"),
    (968, "Eye of the Storm"), (628, "Isle of Conquest"), (1803, "Seething Shore"),
    (727, "Silvershard Mines"), (998, "Temple of Kotmogu"), (761, "The Battle for Gilneas"),
    (726, "Twin Peaks"), (489, "Warsong Gulch"), (2106, "Warsong Gulch"),
    (2656, "Deephaul Ravine"), (2188, "Wintergrasp"), (2118, "Wintergrasp"),
    (1191, "Ashran"), (2799, "Slayer's Rise"),
];

#[rustfmt::skip]
static CLASSIC_BATTLEGROUNDS: &[(u32, &str)] = &[
    (30, "Alterac Valley"), (529, "Arathi Basin"), (566, "Eye of the Storm"),
    (607, "Strand of the Ancients"), (489, "Warsong Gulch"), (628, "Isle of Conquest"),
    (726, "Twin Peaks"), (727, "Silvershard Mines"), (761, "The Battle for Gilneas"),
    (968, "Eye of the Storm"), (998, "Temple of Kotmogu"), (1105, "Deepwind Gorge"),
];

#[rustfmt::skip]
static RETAIL_ARENAS: &[(u32, &str)] = &[
    (1672, "Blade's Edge"), (617, "Dalaran Sewers"), (1505, "Nagrand"),
    (572, "Ruins of Lordaeron"), (2167, "Robodrome"), (1134, "Tiger's Peak"),
    (980, "Tol'viron"), (1504, "Black Rook"), (2373, "Empyrean Domain"),
    (1552, "Ashamane's Fall"), (1911, "Mugambala"), (1825, "Hook Point"),
    (2509, "Maldraxxus"), (2547, "Enigma Crucible"), (2563, "Nokhudon"),
    (2759, "Cage of Carnage"), (2923, "Voidscar Arena"),
];

#[rustfmt::skip]
static CLASSIC_ARENAS: &[(u32, &str)] = &[
    (572, "Ruins of Lordaeron"), (559, "Nagrand"), (617, "Dalaran"),
    (562, "Blade's Edge"), (1134, "Tiger's Peak"), (980, "Tol'viron"),
    (618, "Ring of Valor"),
];

fn lookup(table: &[(u32, &'static str)], id: u32) -> Option<&'static str> {
    table
        .iter()
        .find(|(key, _)| *key == id)
        .map(|entry| entry.1)
}

pub(super) fn retail_battleground_name(zone_id: u32) -> Option<&'static str> {
    lookup(RETAIL_BATTLEGROUNDS, zone_id)
}

pub(super) fn classic_battleground_name(zone_id: u32) -> Option<&'static str> {
    lookup(CLASSIC_BATTLEGROUNDS, zone_id)
}

pub(super) fn battleground_name(zone_id: u32) -> &'static str {
    retail_battleground_name(zone_id)
        .or_else(|| classic_battleground_name(zone_id))
        .unwrap_or("Unknown Battleground")
}

pub(super) fn classic_arena_name(zone_id: u32) -> Option<&'static str> {
    lookup(CLASSIC_ARENAS, zone_id)
}

pub(super) fn arena_zone_name(flavor: &GameFlavor, zone_id: u32) -> Option<String> {
    let name = match flavor {
        GameFlavor::Retail => lookup(RETAIL_ARENAS, zone_id),
        _ => lookup(CLASSIC_ARENAS, zone_id),
    };
    name.map(str::to_string)
}

/// Instance names by zone id (battlegrounds, arenas, dungeons) with the classic
/// MoP challenge-mode fallback, without a default. Resolves the display name
/// for new Mythic+ recordings, whose logs carry only zone and map ids.
pub(crate) fn instance_name(flavor: &GameFlavor, zone_id: u32, map_id: u32) -> Option<String> {
    let merged = retail_battleground_name(zone_id)
        .or_else(|| classic_battleground_name(zone_id))
        .or_else(|| lookup(RETAIL_ARENAS, zone_id))
        .or_else(|| lookup(CLASSIC_ARENAS, zone_id))
        .or_else(|| lookup(DUNGEONS_BY_ZONE_ID, zone_id));
    if let Some(name) = merged {
        return Some(name.to_string());
    }
    if *flavor == GameFlavor::Classic {
        return mop_challenge_mode_name(map_id).map(str::to_string);
    }
    None
}

/// `instance_name` with the `Unknown Dungeon` default.
pub(super) fn dungeon_name(flavor: &GameFlavor, zone_id: u32, map_id: u32) -> String {
    instance_name(flavor, zone_id, map_id).unwrap_or_else(|| "Unknown Dungeon".to_string())
}

pub(super) fn mop_challenge_mode_name(map_id: u32) -> Option<&'static str> {
    lookup(MOP_CHALLENGE_MODES, map_id)
}

pub(super) fn mop_challenge_mode_timers(map_id: u32) -> Option<&'static [f64]> {
    MOP_CHALLENGE_MODE_TIMERS
        .iter()
        .find(|(id, _)| *id == map_id)
        .map(|(_, timers)| &timers[..])
}

#[rustfmt::skip]
static MOP_CHALLENGE_MODES: &[(u32, &str)] = &[
    (2, "Temple of the Jade Serpent"), (56, "Stormstout Brewery"),
    (57, "Gate of the Setting Sun"), (58, "Shado-Pan Monastery"),
    (59, "Siege of Niuzao Temple"), (60, "Mogu'shan Palace"),
    (76, "Scholomance"), (77, "Scarlet Halls"), (78, "Scarlet Monastery"),
];

/// Gold/silver/bronze MoP challenge timers, in minutes.
#[rustfmt::skip]
static MOP_CHALLENGE_MODE_TIMERS: &[(u32, [f64; 3])] = &[
    (2, [45.0, 25.0, 15.0]),
    (56, [45.0, 21.0, 12.0]),
    (57, [45.0, 22.0, 13.0]),
    (58, [60.0, 35.0, 21.0]),
    (59, [50.0, 30.0, 17.5]),
    (60, [45.0, 21.0, 12.0]),
    (76, [55.0, 33.0, 19.0]),
    (77, [45.0, 25.0, 13.0]),
    (78, [45.0, 22.0, 13.0]),
];

#[rustfmt::skip]
static RETAIL_UNIQUE_SPEC_SPELLS: &[(&str, u16)] = &[
    ("Heart Strike", 250), ("Frost Strike", 251), ("Festering Strike", 252),
    ("Eye Beam", 577), ("Fel Devastation", 581), ("Starfall", 102),
    ("Tiger's Fury", 103), ("Maul", 104), ("Lifebloom", 105), ("Pyre", 1467),
    ("Echo", 1468), ("Ebon Might", 1473), ("Cobra Shot", 253), ("Aimed Shot", 254),
    ("Raptor Strike", 255), ("Arcane Barrage", 62), ("Pyroblast", 63),
    ("Ice Lance", 64), ("Keg Smash", 268), ("Fists of Fury", 269),
    ("Enveloping Mist", 270), ("Holy Shock", 65), ("Avenger's Shield", 66),
    ("Blade of Justice", 70), ("Penance", 256), ("Holy Word: Serenity", 257),
    ("Devouring Plague", 258), ("Mutilate", 259), ("Sinister Strike", 260),
    ("Shadow Dance", 261), ("Earth Shock", 262), ("Stormstrike", 263),
    ("Riptide", 264), ("Malefic Rapture", 265), ("Call Dreadstalkers", 266),
    ("Chaos Bolt", 267), ("Mortal Strike", 71), ("Bloodthirst", 72),
    ("Ignore Pain", 73),
];

#[rustfmt::skip]
static CLASSIC_UNIQUE_SPEC_SPELLS: &[(&str, u16)] = &[
    ("Heart Strike", 250), ("Howling Blast", 251), ("Summon Gargoyle", 252),
    ("Starfall", 102), ("Mangle", 103), ("Swiftmend", 105), ("Nourish", 105),
    ("Lifebloom", 105), ("Bestial Wrath", 253), ("Chimera Shot", 254),
    ("Explosive Shot", 255), ("Arcane Barrage", 62), ("Dragon's Breath", 63),
    ("Combustion", 63), ("Ice Barrier", 64), ("Deep Freeze", 64),
    ("Holy Shock", 65), ("Avenger's Shield", 66), ("Crusader Strike", 70),
    ("Penance", 256), ("Guardian Spirit", 257), ("Vampiric Touch", 258),
    ("Mutilate", 259), ("Killing Spree", 260), ("Shadowstep", 261),
    ("Lava Burst", 262), ("Thunderstorm", 262), ("Feral Spirit", 263),
    ("Riptide", 264), ("Haunt", 265), ("Metamorphosis", 266),
    ("Chaos Bolt", 267), ("Mortal Strike", 71), ("Bloodthirst", 72),
    ("Shockwave", 73),
];

#[rustfmt::skip]
static CLASSIC_UNIQUE_SPEC_AURAS: &[(&str, u16)] = &[
    ("Borrowed Time", 256), ("Unstable Affliction", 265), ("The Art of War", 70),
];

fn spell_lookup(table: &[(&'static str, u16)], name: &str) -> Option<u16> {
    table
        .iter()
        .find(|(spell, _)| *spell == name)
        .map(|(_, spec)| *spec)
}

pub(super) fn retail_unique_spec(spell: &str) -> Option<u16> {
    spell_lookup(RETAIL_UNIQUE_SPEC_SPELLS, spell)
}

pub(super) fn classic_unique_spec(spell: &str) -> Option<u16> {
    spell_lookup(CLASSIC_UNIQUE_SPEC_SPELLS, spell)
}

pub(super) fn classic_unique_aura(spell: &str) -> Option<u16> {
    spell_lookup(CLASSIC_UNIQUE_SPEC_AURAS, spell)
}

// --- MD5 (RFC 1321) for the activity hash ---
//
// The hash is persisted in every sidecar and drives multi-POV correlation, so
// the digest must stay byte-stable across releases.

pub(super) fn md5_hex(input: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee,
        0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
        0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa,
        0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
        0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039,
        0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
        0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    let mut message = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in message.as_chunks::<64>().0 {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (mut f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(words[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut digest = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            digest.push_str(&format!("{byte:02x}"));
        }
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_raid_encounter_names_one_raid() {
        let mut seen = std::collections::HashSet::new();
        for raid in RAID_INSTANCES {
            for encounter in raid.encounters {
                assert!(seen.insert(*encounter), "{encounter} is in two raids");
            }
        }
        assert_eq!(raid_short_name(3492), Some("Venomous Abyss"));
        assert_eq!(raid_short_name(1), None);
    }
}
