// SPDX-License-Identifier: GPL-3.0-or-later

//! The damage-meter overlay: a compact Details/Skada-style ranking laid over
//! the player video and fed from the sidecar meter's interval aggregates. The
//! player owns one instance on its `video_overlay`; visibility, filters, and
//! drag position are session-only state. Current and Overall totals stop at
//! the latest completed interval.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use gtk4::gdk::Texture;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use warcraft_rustcorder::domain::{
    LibraryEntry, MeterData, MeterDeath, MeterDeathEventKind, MeterFight, MeterMetric,
};
use warcraft_rustcorder::meter::{
    MeterProjection, ProjectedActor, ProjectedEntry, SAMPLE_INTERVAL_MS, fight_index_at,
    has_untimed_totals, is_count_metric, project_current, project_overall,
};
use warcraft_rustcorder::spelldb::SpellDb;

use super::filters::class_css_class;
use super::format_clock;

/// Raid-marker values (`destRaidFlags & 0xff`) in display order.
const MARKERS: [(u8, &str); 8] = [
    (0x01, "Star"),
    (0x02, "Circle"),
    (0x04, "Diamond"),
    (0x08, "Triangle"),
    (0x10, "Moon"),
    (0x20, "Square"),
    (0x40, "Cross"),
    (0x80, "Skull"),
];

/// Max natural height of the ranking/breakdown list; the scroller takes over
/// beyond it.
const MAX_LIST_HEIGHT: i32 = 260;
/// Minimum panel width/height once the user has resized it; a smaller
/// viewport wins.
const MIN_WIDTH: i32 = 240;
const MIN_HEIGHT: i32 = 140;
/// Bounded target rows fold into "Other", which is not a selectable target.
const OTHER_KEY: &str = "Other";
/// A seek into the player, in media-relative milliseconds.
type SeekFn = Box<dyn Fn(u64)>;

/// Seeking from a meter row lands this far before the event, so the moment
/// plays out on screen instead of having already happened.
const SEEK_LEAD_MS: u64 = 3_000;

/// Minimum spacing of playhead-driven refreshes, so dragging the timeline
/// re-projects the meter about ten times a second instead of on every
/// pointer event.
const POSITION_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a bar fill eases from its previous on-screen position toward
/// the new one. Kept under the 500 ms sample cadence so consecutive
/// updates chain into continuous motion instead of jagged jumps.
const FILL_ANIMATE_MS: u32 = 400;
/// Fill changes smaller than this (about a pixel on the default panel) are
/// applied without an animation, so a settled late-fight meter does not keep
/// the frame clock running for invisible motion.
const MIN_ANIMATED_STEP: f64 = 0.004;

/// Pixels between the meter's left edge and the tooltip that opens to its
/// left.
const TOOLTIP_GAP: i32 = 8;
/// The small spell icon shown on each spell row, in pixels.
const SPELL_ICON_SIZE: i32 = 20;
/// Resource paths for the bundled spell database and its icons.
const SPELLS_JSON_RESOURCE: &str = "/io/github/JohanWes/WarcraftRustcorder/spells/spells.json";
const SPELL_ICON_RESOURCE: &str = "/io/github/JohanWes/WarcraftRustcorder/spells/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Metric(MeterMetric),
    Buffs,
    Deaths,
}

fn view_label(view: View) -> &'static str {
    match view {
        View::Metric(MeterMetric::Damage) => "Damage Done",
        View::Metric(MeterMetric::DamageTaken) => "Damage Taken",
        View::Metric(MeterMetric::Healing) => "Healing Done",
        View::Metric(MeterMetric::Interrupts) => "Interrupts",
        View::Metric(MeterMetric::Dispels) => "Dispels",
        View::Metric(MeterMetric::Casts) => "Casts",
        View::Metric(MeterMetric::Buffs) | View::Buffs => "Buffs",
        View::Deaths => "Deaths",
    }
}

fn view_key(view: View) -> &'static str {
    match view {
        View::Metric(MeterMetric::Damage) => "damage",
        View::Metric(MeterMetric::DamageTaken) => "damage_taken",
        View::Metric(MeterMetric::Healing) => "healing",
        View::Metric(MeterMetric::Interrupts) => "interrupts",
        View::Metric(MeterMetric::Dispels) => "dispels",
        View::Metric(MeterMetric::Casts) => "casts",
        View::Metric(MeterMetric::Buffs) | View::Buffs => "buffs",
        View::Deaths => "deaths",
    }
}

fn view_from_key(key: &str) -> Option<View> {
    Some(match key {
        "damage" => View::Metric(MeterMetric::Damage),
        "damage_taken" => View::Metric(MeterMetric::DamageTaken),
        "healing" => View::Metric(MeterMetric::Healing),
        "interrupts" => View::Metric(MeterMetric::Interrupts),
        "dispels" => View::Metric(MeterMetric::Dispels),
        "casts" => View::Metric(MeterMetric::Casts),
        "buffs" => View::Buffs,
        "deaths" => View::Deaths,
        _ => return None,
    })
}

fn view_empty_message(view: View) -> String {
    format!("No {} in this fight.", view_key(view).replace('_', " "))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentKind {
    Overall,
    Current,
}

/// What a claimed drag sequence on the meter does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragMode {
    /// Header drags move the meter.
    Move,
    /// Grip drags resize the meter.
    Resize,
}

fn segment_key(segment: SegmentKind) -> &'static str {
    match segment {
        SegmentKind::Overall => "overall",
        SegmentKind::Current => "current",
    }
}

fn segment_label(segment: SegmentKind) -> &'static str {
    match segment {
        SegmentKind::Overall => "Overall",
        SegmentKind::Current => "Current fight",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TargetSel {
    All,
    Name(String),
    Marker(u8),
}

/// `meter.target` action key: "all", "name:<target>", or "marker:<value>".
fn target_key(target: &TargetSel) -> String {
    match target {
        TargetSel::All => "all".to_owned(),
        TargetSel::Name(name) => format!("name:{name}"),
        TargetSel::Marker(marker) => format!("marker:{marker}"),
    }
}

fn target_from_key(key: &str) -> Option<TargetSel> {
    match key {
        "all" => Some(TargetSel::All),
        _ => match key.split_once(':')? {
            ("name", name) => Some(TargetSel::Name(name.to_owned())),
            ("marker", value) => Some(TargetSel::Marker(value.parse().ok()?)),
            _ => None,
        },
    }
}

/// The title suffix a filter adds, if any: `Damage Done to Skull`.
fn target_label(target: &TargetSel) -> Option<String> {
    match target {
        TargetSel::All => None,
        TargetSel::Name(name) => Some(name.clone()),
        TargetSel::Marker(marker) => marker_name(*marker).map(str::to_owned),
    }
}

fn marker_name(value: u8) -> Option<&'static str> {
    MARKERS
        .iter()
        .find(|(marker, _)| *marker == value)
        .map(|(_, name)| *name)
}

/// Details-style compact amounts: 1234 → "1.23K", 5_000_000 → "5.00M".
fn format_compact(amount: u64) -> String {
    let mut value = amount as f64;
    let mut suffix = "";
    for candidate in ["K", "M", "B"] {
        if value < 1_000.0 {
            break;
        }
        value /= 1_000.0;
        suffix = candidate;
    }
    if suffix.is_empty() {
        return amount.to_string();
    }
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    match decimals {
        0 => format!("{value:.0}{suffix}"),
        1 => format!("{value:.1}{suffix}"),
        _ => format!("{value:.2}{suffix}"),
    }
}

/// Uptime text: sub-minute spans read as seconds, longer ones as m:ss.
fn format_uptime(ms: u64) -> String {
    if ms < 60_000 {
        return format!("{:.1}s", ms as f64 / 1_000.0);
    }
    format_clock(ms)
}

/// The selected entry's meter facts plus the combatant GUID → spec id join
/// for class colors. The meter is loaded by the player; no file I/O here.
struct EntryMeter {
    fights: Vec<MeterFight>,
    spec_by_guid: HashMap<String, u16>,
}

impl EntryMeter {
    fn from_entry(entry: &LibraryEntry, meter: MeterData) -> Self {
        let spec_by_guid = entry
            .combatants
            .iter()
            .filter_map(|combatant| Some((combatant.guid.clone()?, combatant.spec_id?)))
            .collect();
        Self {
            fights: meter.fights,
            spec_by_guid,
        }
    }
}

/// Actor total for a view: spell amounts unfiltered, or the matching target
/// rows when a target or marker is selected. Utility totals count events.
fn actor_total(actor: &ProjectedActor, view: MeterMetric, target: &TargetSel) -> u64 {
    match target {
        TargetSel::All => actor
            .spells
            .iter()
            .filter(|entry| entry.metric == view)
            .map(|entry| entry.amount)
            .sum(),
        _ => actor
            .targets
            .iter()
            .filter(|entry| entry.metric == view && matches_target(entry, target))
            .map(|entry| entry.amount)
            .sum(),
    }
}

/// Spell rows for one metric, ranked by their share of the actor's total.
/// Since every row uses the same total as its denominator, descending amount
/// is exactly descending percentage; the name makes equal shares stable.
fn ranked_spells(actor: &ProjectedActor, view: MeterMetric) -> Vec<&ProjectedEntry> {
    let mut spells: Vec<_> = actor
        .spells
        .iter()
        .filter(|entry| entry.metric == view)
        .collect();
    spells.sort_by(|a, b| b.amount.cmp(&a.amount).then_with(|| a.key.cmp(&b.key)));
    spells
}

/// Where activating a keyed row navigates.
#[derive(Clone)]
enum Open {
    /// The actor breakdown for this GUID.
    Actor(String),
    /// The spell detail for this spell key.
    Spell(String),
}

/// One line of keyed meter content. Refreshes describe the list as data;
/// [`Inner::set_lines`] applies it onto the row widgets kept by key.
enum Line {
    Heading(&'static str),
    Bar(Bar),
}

/// A keyed fill row. `key` is unique within the list and stable across
/// refreshes, and within one view, segment, and target it always means the
/// same row, so `class` and `open` are only read when the row is created.
struct Bar {
    key: String,
    class: Option<&'static str>,
    left: String,
    right: String,
    fraction: f64,
    /// Whether `left` is a spell name that gets an icon and tooltip.
    spell_icon: bool,
    open: Option<Open>,
}

impl Line {
    fn key(&self) -> String {
        match self {
            Line::Heading(text) => format!("h:{text}"),
            Line::Bar(bar) => bar.key.clone(),
        }
    }
}

/// The widgets kept for one keyed line.
enum RowWidgets {
    Heading(gtk4::Label),
    Bar(BarRow),
}

impl RowWidgets {
    fn widget(&self) -> gtk4::Widget {
        match self {
            RowWidgets::Heading(label) => label.clone().upcast(),
            RowWidgets::Bar(row) => row.root.clone(),
        }
    }
}

/// A kept fill row: the fill bar and labels are updated in place.
struct BarRow {
    /// The row's widget in the content box: a button for clickable rows.
    root: gtk4::Widget,
    fill: gtk4::ProgressBar,
    line: gtk4::Box,
    left: gtk4::Label,
    right: gtk4::Label,
    has_icon: Cell<bool>,
    /// The fraction the fill is heading to.
    target: Cell<f64>,
    /// Eases the fill toward `target`. libadwaita skips straight to the end
    /// when animations are disabled or the row is unmapped.
    animation: adw::TimedAnimation,
}

impl BarRow {
    fn new(root: gtk4::Widget, widgets: BarWidgets) -> Self {
        let fill = widgets.fill.clone();
        let animation = adw::TimedAnimation::new(
            &widgets.fill,
            0.0,
            0.0,
            FILL_ANIMATE_MS,
            adw::CallbackAnimationTarget::new(move |value| fill.set_fraction(value)),
        );
        // Quick off the old position, settling on the new one.
        animation.set_easing(adw::Easing::EaseOutCubic);
        Self {
            root,
            fill: widgets.fill,
            line: widgets.line,
            left: widgets.left,
            right: widgets.right,
            has_icon: Cell::new(false),
            target: Cell::new(0.0),
            animation,
        }
    }

    fn update(&self, bar: &Bar) {
        set_text_if_changed(&self.left, &bar.left);
        set_text_if_changed(&self.right, &bar.right);
        self.set_fraction(bar.fraction.clamp(0.0, 1.0));
    }

    /// Ease the fill from its on-screen position toward `target`, so
    /// consecutive sample updates chain into continuous motion. A sub-pixel
    /// step from rest is applied directly instead of animating.
    fn set_fraction(&self, target: f64) {
        if self.target.replace(target) == target {
            return;
        }
        let from = self.fill.fraction();
        if self.animation.state() != adw::AnimationState::Playing
            && (target - from).abs() < MIN_ANIMATED_STEP
        {
            self.fill.set_fraction(target);
            return;
        }
        self.animation.set_value_from(from);
        self.animation.set_value_to(target);
        self.animation.play();
    }
}

fn set_text_if_changed(label: &gtk4::Label, text: &str) {
    if label.label().as_str() != text {
        label.set_label(text);
    }
}

/// The active target filter, applied to target rows only: by name across all
/// markers, or by marker across all names.
fn matches_target(entry: &ProjectedEntry, target: &TargetSel) -> bool {
    match target {
        TargetSel::All => true,
        TargetSel::Name(name) => &entry.key == name,
        TargetSel::Marker(marker) => entry.marker == *marker,
    }
}

struct Inner {
    root: gtk4::Box,
    header: gtk4::Box,
    title: gtk4::Label,
    context_menu: gtk4::PopoverMenu,
    content: gtk4::Box,
    scroller: gtk4::ScrolledWindow,
    grip: gtk4::Label,
    empty_label: gtk4::Label,
    actions: gtk4::gio::SimpleActionGroup,

    entry: RefCell<Option<EntryMeter>>,
    view: Cell<View>,
    segment: Cell<SegmentKind>,
    /// The open actor breakdown: the actor's GUID.
    target: RefCell<TargetSel>,
    breakdown: RefCell<Option<String>>,
    /// The open spell detail inside that breakdown: the spell key.
    spell: RefCell<Option<String>>,
    /// Fight index the Current segment last rendered from.
    current_fight: Cell<Option<usize>>,
    /// Drag mode chosen at drag begin, if the sequence was claimed.
    drag_mode: Cell<Option<DragMode>>,
    /// Geometry captured at drag begin: `(margin_end, margin_bottom, width,
    /// height)`.
    drag_geometry: Cell<(i32, i32, i32, i32)>,
    /// The user's chosen panel size once resized: an explicit size request
    /// instead of the natural size, so it survives a temporary viewport
    /// shrink.
    desired_size: Cell<Option<(i32, i32)>>,
    /// Last playhead position, kept so a segment switch can pick the Current
    /// fight even while another segment was rendered.
    position_ms: Cell<u64>,
    /// Whether the meter was last asked to show itself, so a refresh arriving
    /// while hidden defers instead of building widgets nobody can see.
    visible: Cell<bool>,
    /// Set when a refresh was deferred while hidden; the next reveal renders
    /// it.
    dirty: Cell<bool>,
    /// Whether a playhead refresh ran within the last
    /// [`POSITION_REFRESH_INTERVAL`], and whether another move arrived since.
    position_throttled: Cell<bool>,
    position_pending: Cell<bool>,
    /// The virtualized history list on screen and its data key. Occurrence
    /// times and deaths are frozen between playhead ticks, so a matching key
    /// keeps the widget — scroll position and bound rows included — instead
    /// of rebuilding it every tick.
    list_cache: RefCell<Option<(String, gtk4::ListView)>>,
    /// Seek request into the player, installed by it at construction.
    seek: RefCell<Option<SeekFn>>,
    /// The keyed rows in `content`, updated in place by `set_lines`. Cleared
    /// when the view, segment, target, or entry changes, so a key only ever
    /// names one row meaning and fresh bars grow in from empty.
    rows: RefCell<HashMap<String, RowWidgets>>,
    /// The bundled spell database, parsed off the GTK thread the first time
    /// the meter is shown; `None` until it arrives.
    spell_db: RefCell<Option<SpellDb>>,
    spell_db_requested: Cell<bool>,
    /// Decoded spell-icon textures keyed by basename, so new rows reuse
    /// them instead of re-decoding.
    icons: RefCell<HashMap<String, Texture>>,
    /// The video overlay the meter sits on; the tooltip lives here, outside
    /// the meter, so it is never clipped by the scroller.
    overlay: RefCell<Option<gtk4::Overlay>>,
    /// The shared spell tooltip, shown directly left of the meter.
    tooltip: gtk4::Box,
    tooltip_icon: gtk4::Picture,
    tooltip_name: gtk4::Label,
    tooltip_desc: gtk4::Label,
}

pub struct DamageMeter {
    pub widget: gtk4::Box,
    inner: Rc<Inner>,
}

impl DamageMeter {
    pub fn new() -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.add_css_class("wr-meter");
        root.set_size_request(300, -1);
        root.set_halign(gtk4::Align::End);
        root.set_valign(gtk4::Align::End);
        root.set_margin_end(16);
        root.set_margin_bottom(16);
        root.set_visible(false);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        header.add_css_class("wr-meter-header");
        let title = gtk4::Label::new(Some("Damage Done"));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        // Cap the title's natural width: its long text is the header's main
        // width contributor, so the explicit root widths from resizing can
        // govern.
        title.set_max_width_chars(1);
        let context_menu = gtk4::PopoverMenu::from_model(None::<&gtk4::gio::Menu>);
        context_menu.set_parent(&title);
        context_menu.set_position(gtk4::PositionType::Bottom);
        context_menu.set_has_arrow(false);
        context_menu.update_property(&[gtk4::accessible::Property::Label("Meter options")]);
        let close = gtk4::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_tooltip_text(Some("Hide meter"));
        close.update_property(&[gtk4::accessible::Property::Label("Hide meter")]);
        header.append(&title);
        header.append(&close);

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
        scroller.set_propagate_natural_height(true);
        scroller.set_vexpand(true);
        scroller.set_max_content_height(MAX_LIST_HEIGHT);
        scroller.set_child(Some(&content));
        // The resize grip: a dim, bottom-right corner handle the drag
        // gesture picks on.
        let grip = gtk4::Label::new(Some("◢"));
        grip.add_css_class("dim-label");
        grip.set_halign(gtk4::Align::End);
        grip.set_size_request(16, 16);
        grip.set_cursor_from_name(Some("se-resize"));
        grip.set_tooltip_text(Some("Resize meter"));
        grip.update_property(&[gtk4::accessible::Property::Label("Resize meter")]);
        let empty_label = gtk4::Label::new(None);
        empty_label.add_css_class("dim-label");
        empty_label.set_halign(gtk4::Align::Center);
        // Wrapped, so a long empty-state text never imposes a width floor
        // on the resizable panel.
        empty_label.set_wrap(true);
        empty_label.set_margin_top(16);
        empty_label.set_margin_bottom(16);

        let actions = gtk4::gio::SimpleActionGroup::new();
        root.insert_action_group("meter", Some(&actions));

        // The spell tooltip: a compact panel shown directly left of the
        // meter, parented to the video overlay in `attach_drag` so it is
        // never clipped by the meter scroller. Its position is anchored to
        // the meter, not the hovered row, so it stays still while rows
        // reorder.
        let tooltip = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        tooltip.add_css_class("wr-tooltip");
        tooltip.set_visible(false);
        let tooltip_head = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let tooltip_icon = gtk4::Picture::new();
        tooltip_icon.add_css_class("wr-tooltip-icon");
        let tooltip_name = gtk4::Label::new(None);
        tooltip_name.add_css_class("wr-tooltip-name");
        tooltip_name.set_xalign(0.0);
        tooltip_name.set_halign(gtk4::Align::Start);
        let tooltip_desc = gtk4::Label::new(None);
        tooltip_desc.add_css_class("wr-tooltip-desc");
        tooltip_desc.set_xalign(0.0);
        tooltip_desc.set_halign(gtk4::Align::Start);
        tooltip_desc.set_wrap(true);
        tooltip_desc.set_max_width_chars(48);
        tooltip_head.append(&tooltip_icon);
        tooltip_head.append(&tooltip_name);
        tooltip.append(&tooltip_head);
        tooltip.append(&tooltip_desc);

        root.append(&header);
        root.append(&scroller);
        root.append(&grip);

        let inner = Rc::new(Inner {
            root,
            header,
            title,
            context_menu,
            content,
            scroller,
            grip,
            empty_label,
            actions,
            entry: RefCell::new(None),
            view: Cell::new(View::Metric(MeterMetric::Damage)),
            segment: Cell::new(SegmentKind::Current),
            target: RefCell::new(TargetSel::All),
            position_ms: Cell::new(0),
            visible: Cell::new(false),
            dirty: Cell::new(false),
            position_throttled: Cell::new(false),
            position_pending: Cell::new(false),
            list_cache: RefCell::new(None),
            breakdown: RefCell::new(None),
            spell: RefCell::new(None),
            current_fight: Cell::new(None),
            drag_mode: Cell::new(None),
            drag_geometry: Cell::new((16, 16, 0, 0)),
            desired_size: Cell::new(None),
            seek: RefCell::new(None),
            rows: RefCell::new(HashMap::new()),
            spell_db: RefCell::new(None),
            spell_db_requested: Cell::new(false),
            icons: RefCell::new(HashMap::new()),
            overlay: RefCell::new(None),
            tooltip,
            tooltip_icon,
            tooltip_name,
            tooltip_desc,
        });

        inner.connect_actions();
        inner.connect_clicks();
        {
            let inner = Rc::clone(&inner);
            close.connect_clicked(move |_| inner.set_visible(false));
        }
        inner.refresh();

        Self {
            widget: inner.root.clone(),
            inner,
        }
    }

    /// Feed the selected entry with its loaded meter; `None` clears. No file
    /// I/O here. A target filter is reset: names and markers of a previous
    /// recording need not occur here.
    pub fn set_entry(&self, entry: Option<(&LibraryEntry, MeterData)>) {
        let inner = &self.inner;
        inner
            .entry
            .replace(entry.map(|(entry, meter)| EntryMeter::from_entry(entry, meter)));
        inner.position_ms.set(0);
        inner.current_fight.set(None);
        inner.breakdown.replace(None);
        inner.spell.replace(None);
        inner.target.replace(TargetSel::All);
        inner.clear_rows();
        inner.refresh();
    }
    /// The playhead moved. Both segment modes are cumulative through the
    /// latest completed sample, so refresh on each interval or fight boundary.
    pub fn set_position(&self, position_ms: u64) {
        let inner = &self.inner;
        let previous_interval = inner.position_ms.replace(position_ms) / SAMPLE_INTERVAL_MS;
        let previous_fight = inner.current_fight.get();
        inner.sync_current_fight();
        if position_ms / SAMPLE_INTERVAL_MS != previous_interval
            || inner.current_fight.get() != previous_fight
        {
            inner.refresh_for_position();
        }
    }

    pub fn set_visible(&self, visible: bool) {
        self.inner.set_visible(visible);
    }

    pub fn toggle(&self) {
        self.inner.set_visible(!self.inner.visible.get());
    }

    /// Re-clamp the drag margins to the current overlay allocation; once
    /// resized, the desired size is reapplied, capped to the viewport so a
    /// temporary shrink does not lose it.
    pub fn clamp_position(&self) {
        let inner = &self.inner;
        if let Some((width, height)) = inner.desired_size.get()
            && let Some((viewport_width, viewport_height)) = inner.viewport_size()
            && viewport_width > 0
            && viewport_height > 0
        {
            inner
                .root
                .set_size_request(width.min(viewport_width), height.min(viewport_height));
        }
        inner.move_to(
            f64::from(inner.root.margin_end()),
            f64::from(inner.root.margin_bottom()),
        );
    }

    /// Route seeks from clickable meter rows (death log events, occurrence
    /// times) back into the player. Media-relative milliseconds.
    pub fn connect_seek(&self, seek: impl Fn(u64) + 'static) {
        self.inner.seek.replace(Some(Box::new(seek)));
    }

    /// Install the meter's drag gesture on the overlay it was added to.
    pub fn attach_drag(&self, overlay: &gtk4::Overlay) {
        self.inner.connect_drag(overlay);
    }
}

impl Inner {
    fn set_visible(self: &Rc<Self>, visible: bool) {
        // The tooltip lives on the video overlay, outside the meter: hiding
        // the meter must hide it too or it would float over the video alone.
        if visible {
            self.request_spell_db();
        } else {
            self.hide_tooltip();
        }
        self.visible.set(visible);
        self.root.set_visible(visible);
        if visible && self.dirty.take() {
            self.refresh();
        }
    }

    fn connect_actions(self: &Rc<Self>) {
        let view = stateful_action("view", view_key(self.view.get()));
        self.actions.add_action(&view);
        let this = Rc::clone(self);
        view.connect_change_state(move |action, state| {
            // With a change-state handler connected, GLib leaves the state
            // update to it; the default handler is suppressed.
            let Some(state) = state else {
                return;
            };
            action.set_state(state);
            if let Some(key) = state.str()
                && let Some(view) = view_from_key(key)
            {
                this.set_view(view);
            }
        });

        let segment = stateful_action("segment", segment_key(self.segment.get()));
        self.actions.add_action(&segment);
        let this = Rc::clone(self);
        segment.connect_change_state(move |action, state| {
            let Some(state) = state else {
                return;
            };
            action.set_state(state);
            if let Some(key) = state.str() {
                let segment = match key {
                    "overall" => SegmentKind::Overall,
                    "current" => SegmentKind::Current,
                    _ => return,
                };
                this.set_segment(segment);
            }
        });

        let target = stateful_action("target", "all");
        self.actions.add_action(&target);
        let this = Rc::clone(self);
        target.connect_change_state(move |action, state| {
            let Some(state) = state else {
                return;
            };
            action.set_state(state);
            if let Some(key) = state.str()
                && let Some(target) = target_from_key(key)
            {
                this.set_target(target);
            }
        });
    }

    /// Title-and-grip drag: the title moves the meter by pixel margins, the
    /// bottom-right grip resizes it, both clamped so the panel stays inside
    /// the overlay allocation. Other header controls are denied so their
    /// clicks survive. The controller lives on the stationary overlay because
    /// overlay-relative drag coordinates stay valid while the meter moves.
    fn connect_drag(self: &Rc<Self>, overlay: &gtk4::Overlay) {
        // Keep the overlay for tooltip positioning, and park the tooltip on
        // it so it floats over the video, left of the meter, instead of
        // being clipped by the meter scroller. It must not take pointer
        // events, or it would eat row clicks and break the drag pick below.
        self.overlay.replace(Some(overlay.clone()));
        overlay.add_overlay(&self.tooltip);
        self.tooltip.set_can_target(false);

        let drag = gtk4::GestureDrag::new();
        {
            let overlay = (*overlay).clone();
            let this = Rc::clone(self);
            drag.connect_drag_begin(move |gesture, start_x, start_y| {
                // The mode comes from the overlay-relative pick: the grip
                // resizes, the header moves, anything else is denied.
                this.drag_mode.set(None);
                let Some(picked) = overlay.pick(start_x, start_y, gtk4::PickFlags::DEFAULT) else {
                    gesture.set_state(gtk4::EventSequenceState::Denied);
                    return;
                };
                let mode = if picked == this.grip || picked.is_ancestor(&this.grip) {
                    DragMode::Resize
                } else if picked == this.header
                    || picked == this.title
                    || picked.is_ancestor(&this.title)
                {
                    DragMode::Move
                } else {
                    gesture.set_state(gtk4::EventSequenceState::Denied);
                    return;
                };
                let width = this.root.width();
                let height = this.root.height();
                this.drag_geometry.set((
                    this.root.margin_end(),
                    this.root.margin_bottom(),
                    width,
                    height,
                ));
                if mode == DragMode::Resize {
                    // Freeze the current allocation as the explicit size
                    // request; the natural height would otherwise override
                    // it, so the scroller must stop propagating it. Both
                    // changes only queue layout, so nothing jumps.
                    this.root.set_size_request(width, height);
                    this.scroller.set_propagate_natural_height(false);
                    this.desired_size.set(Some((width, height)));
                }
                this.drag_mode.set(Some(mode));
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            });
        }
        {
            let this = Rc::clone(self);
            drag.connect_drag_update(move |_, offset_x, offset_y| {
                let (end, bottom, width, height) = this.drag_geometry.get();
                match this.drag_mode.get() {
                    Some(DragMode::Move) => {
                        this.move_to(f64::from(end) - offset_x, f64::from(bottom) - offset_y);
                    }
                    Some(DragMode::Resize) => {
                        this.resize_to(end, bottom, width, height, offset_x, offset_y);
                    }
                    None => {}
                }
            });
        }
        overlay.add_controller(drag);
    }

    /// Secondary click on the title opens meter options; elsewhere on the
    /// panel it returns an open actor breakdown to the ranking.
    fn connect_clicks(self: &Rc<Self>) {
        let menu_click = gtk4::GestureClick::new();
        menu_click.set_button(gtk4::gdk::BUTTON_SECONDARY);
        let this = Rc::clone(self);
        menu_click.connect_pressed(move |gesture, _, x, y| {
            this.context_menu.set_menu_model(Some(&this.menu_model()));
            this.context_menu
                .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            this.context_menu.popup();
            gesture.set_state(gtk4::EventSequenceState::Claimed);
        });
        self.title.add_controller(menu_click);

        let back_click = gtk4::GestureClick::new();
        back_click.set_button(gtk4::gdk::BUTTON_SECONDARY);
        let this = Rc::clone(self);
        back_click.connect_pressed(move |gesture, _, _, _| {
            // One level per click: spell detail, then actor breakdown.
            if this.spell.borrow().is_some() {
                this.spell.replace(None);
            } else if this.breakdown.borrow().is_some() {
                this.breakdown.replace(None);
            } else {
                return;
            }
            this.refresh();
            gesture.set_state(gtk4::EventSequenceState::Claimed);
        });
        self.root.add_controller(back_click);
    }

    fn set_view(self: &Rc<Self>, view: View) {
        if self.view.replace(view) != view {
            self.target.replace(TargetSel::All);
            self.breakdown.replace(None);
            self.spell.replace(None);
            self.clear_rows();
            self.refresh();
        }
    }

    fn set_segment(self: &Rc<Self>, segment: SegmentKind) {
        if self.segment.replace(segment) != segment {
            // Pick the Current fight from the last known playhead before the
            // re-render; positions arrived while Overall was shown too.
            self.sync_current_fight();
            self.clear_rows();
            self.refresh();
        }
    }

    /// Track the Current fight from the last known playhead position, even
    /// while another segment is rendered, so a segment switch never shows a
    /// stale fight.
    fn sync_current_fight(&self) {
        let index = self
            .entry
            .borrow()
            .as_ref()
            .and_then(|entry| fight_index_at(&entry.fights, self.position_ms.get()));
        self.current_fight.replace(index);
    }

    fn set_target(self: &Rc<Self>, target: TargetSel) {
        if *self.target.borrow() != target {
            self.target.replace(target);
            self.clear_rows();
            self.refresh();
        }
    }

    /// One full re-render: header and content derive from the same session
    /// state and selected entry. Menu models are built only when opened, so a
    /// playback tick never replaces an open popover.
    fn refresh(self: &Rc<Self>) {
        // A tick's projection work and widget rebuilds are unobservable while
        // hidden; defer them to the reveal instead.
        if !self.visible.get() {
            self.dirty.set(true);
            return;
        }
        self.dirty.set(false);
        self.sync_action_states();
        let fight = self.selected_fight();
        self.rebuild_title(fight.as_ref());
        self.rebuild_content(fight);
    }

    /// A playhead refresh, throttled: the first move renders at once and
    /// later moves inside [`POSITION_REFRESH_INTERVAL`] coalesce into one
    /// trailing refresh, so a timeline drag always ends on its final
    /// position.
    fn refresh_for_position(self: &Rc<Self>) {
        if self.position_throttled.get() {
            self.position_pending.set(true);
            return;
        }
        self.refresh();
        self.position_throttled.set(true);
        let this = Rc::clone(self);
        gtk4::glib::timeout_add_local_once(POSITION_REFRESH_INTERVAL, move || {
            this.position_throttled.set(false);
            if this.position_pending.take() {
                this.refresh_for_position();
            }
        });
    }

    /// Mirror the cells into the stateful actions so rebuilt menus mark the
    /// active items. `set_state` updates the property directly.
    fn sync_action_states(&self) {
        let target = target_key(&self.target.borrow());
        for (name, state) in [
            ("view", view_key(self.view.get())),
            ("segment", segment_key(self.segment.get())),
            ("target", target.as_str()),
        ] {
            if let Some(action) = self
                .actions
                .lookup_action(name)
                .and_downcast::<gtk4::gio::SimpleAction>()
            {
                action.set_state(&state.to_variant());
            }
        }
    }

    /// The selected Current or Overall projection at the playhead.
    fn selected_fight(&self) -> Option<MeterProjection> {
        let entry = self.entry.borrow();
        let entry = entry.as_ref()?;
        if entry.fights.is_empty() {
            return None;
        }
        match self.segment.get() {
            SegmentKind::Overall => Some(project_overall(&entry.fights, self.position_ms.get())),
            SegmentKind::Current => project_current(&entry.fights, self.position_ms.get()),
        }
    }

    /// Header text: `{mm:ss} {view}`, with the target appended when filtered.
    fn rebuild_title(&self, fight: Option<&MeterProjection>) {
        let view = self.view.get();
        let label = view_label(view);
        let title = match (view, target_label(&self.target.borrow())) {
            (View::Metric(MeterMetric::DamageTaken), Some(target)) => {
                format!("{label} from {target}")
            }
            (View::Metric(_), Some(target)) => format!("{label} to {target}"),
            (_, _) => label.to_owned(),
        };
        let title = match self.spell.borrow().as_deref() {
            Some(spell) => format!("{title} — {spell}"),
            None => title,
        };
        match fight {
            Some(fight) => self
                .title
                .set_text(&format!("{} {title}", format_clock(fight.elapsed_ms))),
            None => self.title.set_text(&title),
        }
    }

    fn menu_model(self: &Rc<Self>) -> gtk4::gio::Menu {
        let menu = gtk4::gio::Menu::new();

        let view_section = gtk4::gio::Menu::new();
        for view in [
            View::Metric(MeterMetric::Damage),
            View::Metric(MeterMetric::DamageTaken),
            View::Metric(MeterMetric::Healing),
            View::Metric(MeterMetric::Interrupts),
            View::Metric(MeterMetric::Dispels),
            View::Metric(MeterMetric::Casts),
            View::Buffs,
            View::Deaths,
        ] {
            let item = gtk4::gio::MenuItem::new(Some(view_label(view)), None);
            item.set_action_and_target_value(
                Some("meter.view"),
                Some(&view_key(view).to_variant()),
            );
            view_section.append_item(&item);
        }
        menu.append_section(None, &view_section);

        let segment_section = gtk4::gio::Menu::new();
        for segment in [SegmentKind::Overall, SegmentKind::Current] {
            let item = gtk4::gio::MenuItem::new(Some(segment_label(segment)), None);
            item.set_action_and_target_value(
                Some("meter.segment"),
                Some(&segment_key(segment).to_variant()),
            );
            segment_section.append_item(&item);
        }

        let (names, markers) = self.target_choices();
        if matches!(self.view.get(), View::Metric(_)) && !(names.is_empty() && markers.is_empty()) {
            // Only names and markers present in the selected segment; a dead
            // entry would be a filter with no rows.
            let targets = gtk4::gio::Menu::new();
            let all = gtk4::gio::MenuItem::new(Some("All targets"), None);
            all.set_action_and_target_value(Some("meter.target"), Some(&"all".to_variant()));
            targets.append_item(&all);
            for name in names {
                let item = gtk4::gio::MenuItem::new(Some(&name), None);
                item.set_action_and_target_value(
                    Some("meter.target"),
                    Some(&format!("name:{name}").to_variant()),
                );
                targets.append_item(&item);
            }
            for marker in markers {
                let label = marker_name(marker).unwrap_or("Other");
                let item = gtk4::gio::MenuItem::new(Some(label), None);
                item.set_action_and_target_value(
                    Some("meter.target"),
                    Some(&format!("marker:{marker}").to_variant()),
                );
                targets.append_item(&item);
            }
            // Not a section of its own: GTK 4.22 measures a trailing
            // submenu-only section one separator short, clipping the row.
            segment_section.append_submenu(Some("Target"), &targets);
        }
        menu.append_section(None, &segment_section);

        menu
    }

    /// Target names (sorted) and marker values (canonical order) present in
    /// the selected segment for the active view. `Other` is never a
    /// selectable target.
    fn target_choices(&self) -> (Vec<String>, Vec<u8>) {
        let View::Metric(metric) = self.view.get() else {
            return (Vec::new(), Vec::new());
        };
        let Some(fight) = self.selected_fight() else {
            return (Vec::new(), Vec::new());
        };
        let mut names = BTreeSet::new();
        let mut markers = Vec::new();
        for actor in &fight.actors {
            for entry in actor.targets.iter().filter(|entry| entry.metric == metric) {
                if entry.key != OTHER_KEY {
                    names.insert(entry.key.clone());
                }
                if entry.marker != 0 && !markers.contains(&entry.marker) {
                    markers.push(entry.marker);
                }
            }
        }
        markers.sort_by_key(|marker| {
            MARKERS
                .iter()
                .position(|(value, _)| value == marker)
                .unwrap_or(MARKERS.len())
        });
        (names.into_iter().collect(), markers)
    }

    fn rebuild_content(self: &Rc<Self>, fight: Option<MeterProjection>) {
        let Some(fight) = fight else {
            let has_fights = self
                .entry
                .borrow()
                .as_ref()
                .is_some_and(|entry| !entry.fights.is_empty());
            self.show_empty(if has_fights {
                "No fight yet at this point."
            } else {
                "No combat data for this recording."
            });
            return;
        };
        let view = self.view.get();
        let breakdown = self.breakdown.borrow().clone();
        if view == View::Deaths {
            self.rebuild_deaths(&fight, breakdown.as_deref());
            return;
        }
        let actor = breakdown
            .as_ref()
            .and_then(|guid| fight.actors.iter().find(|actor| &actor.guid == guid));
        // A segment switch may have left the breakdown without its actor.
        if breakdown.is_some() && actor.is_none() {
            self.breakdown.replace(None);
            self.spell.replace(None);
        }
        let View::Metric(metric) = view else {
            match actor {
                Some(actor) => self.rebuild_buffs(actor, &fight),
                None => self.rebuild_buff_ranking(&fight),
            }
            return;
        };
        let target = self.target.borrow().clone();
        if let Some(actor) = actor {
            self.rebuild_breakdown(actor, metric, &target);
            return;
        }
        let mut ranked: Vec<(&ProjectedActor, u64)> = fight
            .actors
            .iter()
            .filter_map(|actor| {
                let total = actor_total(actor, metric, &target);
                (total > 0).then_some((actor, total))
            })
            .collect();
        if ranked.is_empty() {
            let untimed = self
                .entry
                .borrow()
                .as_ref()
                .is_some_and(|entry| has_untimed_totals(&entry.fights));
            if untimed {
                self.show_empty("This recording has no time-resolved meter data.");
            } else {
                self.show_empty(&view_empty_message(view));
            }
            return;
        }
        ranked.sort_by_key(|(_, total)| std::cmp::Reverse(*total));
        self.rebuild_ranking(&fight, &ranked, metric);
    }

    /// Dense ranked buttons: class-colored fill behind white labels, compact
    /// total and rate on the right. Activating a row opens its breakdown.
    fn rebuild_ranking(
        self: &Rc<Self>,
        fight: &MeterProjection,
        ranked: &[(&ProjectedActor, u64)],
        view: MeterMetric,
    ) {
        let top = ranked.first().map_or(1, |(_, total)| *total);
        let counted = is_count_metric(view);
        let mut lines = Vec::with_capacity(ranked.len());
        for (rank, (actor, total)) in ranked.iter().enumerate() {
            let right = if fight.elapsed_ms == 0 {
                format_compact(*total)
            } else if view == MeterMetric::Casts {
                // Casts per minute: a per-second rate on a handful of casts
                // rounds to nothing.
                let cpm = *total as f64 * 60_000.0 / fight.elapsed_ms as f64;
                format!("{total} ({cpm:.1} CPM)")
            } else if counted {
                format_compact(*total)
            } else {
                let rate = u128::from(*total) * 1_000 / u128::from(fight.elapsed_ms);
                format!(
                    "{} ({})",
                    format_compact(*total),
                    format_compact(rate as u64)
                )
            };
            lines.push(self.actor_line(
                rank,
                &actor.guid,
                &actor.name,
                right,
                *total as f64 / top as f64,
            ));
        }
        self.set_lines(0, lines);
    }

    /// A ranking row for one actor; activating it opens the breakdown.
    fn actor_line(
        &self,
        rank: usize,
        guid: &str,
        name: &str,
        right: String,
        fraction: f64,
    ) -> Line {
        Line::Bar(Bar {
            key: format!("r:{guid}"),
            class: self.class_for(guid),
            left: format!("{}. {name}", rank + 1),
            right,
            fraction,
            spell_icon: false,
            open: Some(Open::Actor(guid.to_owned())),
        })
    }

    fn rebuild_deaths(self: &Rc<Self>, fight: &MeterProjection, selected_guid: Option<&str>) {
        if let Some(guid) = selected_guid {
            let deaths: Vec<&MeterDeath> = fight
                .deaths
                .iter()
                .filter(|death| death.guid == guid)
                .collect();
            if !deaths.is_empty() {
                self.rebuild_death_breakdown(guid, &deaths);
                return;
            }
            self.breakdown.replace(None);
        }
        let mut ranked: Vec<(String, String, u64)> = Vec::new();
        for death in &fight.deaths {
            if let Some((_, _, count)) = ranked.iter_mut().find(|(guid, _, _)| guid == &death.guid)
            {
                *count += 1;
            } else {
                ranked.push((death.guid.clone(), death.name.clone(), 1));
            }
        }
        if ranked.is_empty() {
            self.show_empty(&view_empty_message(View::Deaths));
            return;
        }
        ranked.sort_by_key(|(_, _, count)| std::cmp::Reverse(*count));
        let top = ranked[0].2;
        let lines = ranked
            .iter()
            .enumerate()
            .map(|(rank, (guid, name, count))| {
                self.actor_line(
                    rank,
                    guid,
                    name,
                    count.to_string(),
                    *count as f64 / top as f64,
                )
            })
            .collect();
        self.set_lines(0, lines);
    }

    fn rebuild_death_breakdown(self: &Rc<Self>, guid: &str, deaths: &[&MeterDeath]) {
        // Deaths are frozen for the fight like occurrence times; keying on
        // the filter and the list bounds keeps the list across half-second
        // ticks that add nothing.
        let key = death_list_key(guid, deaths);
        if self.list_cached(&key) {
            return;
        }
        let this = Rc::clone(self);
        let items: Vec<(usize, MeterDeath)> = deaths
            .iter()
            .enumerate()
            .map(|(index, death)| (index, (*death).clone()))
            .collect();
        let list = history_list(items, move |(index, death)| {
            let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            content.append(&heading(&format!(
                "Death {} — {}",
                index + 1,
                format_clock(death.at_ms)
            )));
            // Each bar is the health the unit was left on after the event, so
            // the list reads as a health bar draining towards the death.
            for event in &death.events {
                let before_ms = death.at_ms.saturating_sub(event.at_ms);
                let (class, sign) = match event.kind {
                    MeterDeathEventKind::Damage => ("wr-death-damage", "-"),
                    MeterDeathEventKind::Healing => ("wr-death-healing", "+"),
                };
                // Sidecars written before HP was recorded draw a full bar
                // rather than a misleading one.
                let remaining = if death.max_hp > 0 {
                    event.hp as f64 / death.max_hp as f64
                } else {
                    1.0
                };
                let row = fill_line(
                    Some(class),
                    &format!(
                        "-{:.1}s {} ({})",
                        before_ms as f64 / 1_000.0,
                        event.spell_name,
                        event.source_name
                    ),
                    &if event.overkill > 0 {
                        format!(
                            "{sign}{} ({} overkill)",
                            format_compact(event.amount),
                            format_compact(event.overkill)
                        )
                    } else {
                        format!("{sign}{}", format_compact(event.amount))
                    },
                    remaining,
                );
                let at_ms = event.at_ms;
                this.attach_spell_icon(&row.line, &event.spell_name);
                content.append(&this.row_button(&row.overlay, move |this| this.seek_to(at_ms)));
            }
            let row = fill_line(Some("wr-death-damage"), "0.0s Death", "", 0.0);
            let at_ms = death.at_ms;
            content.append(&this.row_button(&row.overlay, move |this| this.seek_to(at_ms)));
            content.upcast()
        });
        self.set_list_content(&key, &list);
    }

    /// Buff ranking: players by total BUFF uptime; the right label is the
    /// accumulated uptime with the application count.
    fn rebuild_buff_ranking(self: &Rc<Self>, fight: &MeterProjection) {
        let mut ranked: Vec<(&ProjectedActor, u64, u32)> = fight
            .actors
            .iter()
            .filter_map(|actor| {
                let (uptime, apps) = actor
                    .spells
                    .iter()
                    .filter(|entry| entry.metric == MeterMetric::Buffs && entry.key != OTHER_KEY)
                    .fold((0_u64, 0_u32), |(uptime, apps), entry| {
                        (uptime + entry.amount, apps + entry.hits)
                    });
                (uptime > 0 || apps > 0).then_some((actor, uptime, apps))
            })
            .collect();
        if ranked.is_empty() {
            self.show_empty(&view_empty_message(View::Buffs));
            return;
        }
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
        let top = ranked.first().map_or(1, |(_, uptime, _)| *uptime).max(1);
        let lines = ranked
            .iter()
            .enumerate()
            .map(|(rank, (actor, uptime, apps))| {
                self.actor_line(
                    rank,
                    &actor.guid,
                    &actor.name,
                    format!("{} ({apps})", format_uptime(*uptime)),
                    *uptime as f64 / top as f64,
                )
            })
            .collect();
        self.set_lines(0, lines);
    }

    /// The player's buffs: name on the left, applications and their share of
    /// the fight's timed window on the right. Activating a buff lists when it
    /// was applied; the folded "Other" row is filtered out because it is not
    /// a real buff.
    fn rebuild_buffs(self: &Rc<Self>, actor: &ProjectedActor, fight: &MeterProjection) {
        // Bound to a `let` first, mirroring `rebuild_breakdown`: a buff that
        // left the projection below must clear `spell`.
        let spell_key = self.spell.borrow().clone();
        if let Some(key) = spell_key {
            if let Some(entry) = actor
                .spells
                .iter()
                .find(|entry| entry.metric == MeterMetric::Buffs && entry.key == key)
            {
                self.rebuild_spell_breakdown(actor, entry, MeterMetric::Buffs);
                return;
            }
            self.spell.replace(None);
        }
        let mut buffs: Vec<&ProjectedEntry> = actor
            .spells
            .iter()
            .filter(|entry| entry.metric == MeterMetric::Buffs && entry.key != OTHER_KEY)
            .collect();
        if buffs.is_empty() {
            self.show_empty(&view_empty_message(View::Buffs));
            return;
        }
        buffs.sort_by(|a, b| b.amount.cmp(&a.amount).then_with(|| a.key.cmp(&b.key)));
        let class = self.class_for(&actor.guid);
        let lines = buffs
            .into_iter()
            .map(|entry| {
                // The share is relative to the fight's timed window; a buff
                // that outlived it reads as capped rather than over 100%.
                let share = if fight.elapsed_ms == 0 {
                    0.0
                } else {
                    entry.amount as f64 / fight.elapsed_ms as f64
                };
                let right = if fight.elapsed_ms == 0 {
                    entry.hits.to_string()
                } else {
                    format!("{} · {:.0}%", entry.hits, share.min(1.0) * 100.0)
                };
                Line::Bar(Bar {
                    key: format!("b:{}", entry.key),
                    class,
                    left: entry.key.clone(),
                    right,
                    fraction: share,
                    spell_icon: true,
                    open: Some(Open::Spell(entry.key.clone())),
                })
            })
            .collect();
        self.set_lines(4, lines);
    }

    /// The actor drilldown in the same scroller: the Spells and Targets lists
    /// with the active filters intact. Secondary click on the panel returns to
    /// the ranking.
    fn rebuild_breakdown(
        self: &Rc<Self>,
        actor: &ProjectedActor,
        view: MeterMetric,
        target: &TargetSel,
    ) {
        // Bound to a `let` first: an `if let` scrutinee guard lives through
        // the body, and dropping a spell that left the projection below must
        // mutate `spell`.
        let spell_key = self.spell.borrow().clone();
        if let Some(key) = spell_key {
            if let Some(entry) = actor
                .spells
                .iter()
                .find(|entry| entry.metric == view && entry.key == key)
            {
                self.rebuild_spell_breakdown(actor, entry, view);
                return;
            }
            self.spell.replace(None);
        }
        let class = self.class_for(&actor.guid);
        let mut lines = vec![Line::Heading("Spells")];
        let spell_total = actor_total(actor, view, &TargetSel::All);
        for entry in ranked_spells(actor, view) {
            let mut bar =
                breakdown_bar(format!("s:{}", entry.key), class, entry, spell_total, true);
            bar.open = Some(Open::Spell(entry.key.clone()));
            lines.push(Line::Bar(bar));
        }

        // Casts keep no target rows, so the heading would stand alone.
        let targets: Vec<&ProjectedEntry> = actor
            .targets
            .iter()
            .filter(|entry| entry.metric == view && matches_target(entry, target))
            .collect();
        if !targets.is_empty() {
            lines.push(Line::Heading(if view == MeterMetric::DamageTaken {
                "Sources"
            } else {
                "Targets"
            }));
            let target_total: u64 = targets.iter().map(|entry| entry.amount).sum();
            for entry in targets {
                // One name can occur under several markers.
                let key = format!("t:{}:{}", entry.marker, entry.key);
                lines.push(Line::Bar(breakdown_bar(
                    key,
                    class,
                    entry,
                    target_total,
                    false,
                )));
            }
        }
        self.set_lines(4, lines);
    }

    /// One spell's per-hit statistics and its own target split. The spell name
    /// is in the panel title, so no row is spent on it.
    fn rebuild_spell_breakdown(
        self: &Rc<Self>,
        actor: &ProjectedActor,
        entry: &ProjectedEntry,
        view: MeterMetric,
    ) {
        let class = self.class_for(&actor.guid);
        // Count metrics have nothing per-hit to report: every event is worth
        // one. Their detail is when each happened, and each row seeks there.
        // The history can hold hundreds of entries, so it is virtualized and
        // kept across half-second ticks that change nothing — scrubbing
        // neither rebuilds nor scrolls it.
        if is_count_metric(view) {
            let key = occurrence_key(view, &actor.guid, entry);
            if self.list_cached(&key) {
                return;
            }
            let this = Rc::clone(self);
            let items: Vec<(u64, String)> = entry
                .times
                .iter()
                .map(|at_ms| {
                    let target = entry
                        .targets
                        .iter()
                        .find(|target| target.times.contains(at_ms))
                        .map_or("", |target| target.key.as_str())
                        .to_owned();
                    (*at_ms, target)
                })
                .collect();
            let list = history_list(items, move |(at_ms, target)| {
                let at_ms = *at_ms;
                let row = fill_line(class, &format_clock(at_ms), target, 1.0);
                this.row_button(&row.overlay, move |this| this.seek_to(at_ms))
                    .upcast()
            });
            self.set_list_content(&key, &list);
            return;
        }
        let stat = |label: &str, right: String, fraction: f64| {
            Line::Bar(Bar {
                key: format!("stat:{label}"),
                class,
                left: label.to_owned(),
                right,
                fraction,
                spell_icon: false,
                open: None,
            })
        };
        let mut lines = Vec::new();
        let average = if entry.hits == 0 {
            0
        } else {
            entry.amount / u64::from(entry.hits)
        };
        for (label, value) in [
            ("Average", average),
            ("Maximum", entry.max),
            ("Minimum", entry.min),
        ] {
            // A sidecar written before per-hit statistics existed has no
            // extremes; showing 0 would read as a real measurement.
            let right = if entry.max == 0 {
                "—".to_owned()
            } else {
                format_compact(value)
            };
            let fraction = if entry.max == 0 {
                0.0
            } else {
                value as f64 / entry.max as f64
            };
            lines.push(stat(label, right, fraction));
        }
        lines.push(stat("Hits", entry.hits.to_string(), 1.0));
        if view == MeterMetric::Healing {
            let raw = entry.amount + entry.overheal;
            let share = if raw == 0 {
                0.0
            } else {
                entry.overheal as f64 / raw as f64
            };
            lines.push(stat(
                "Overheal",
                format!("{} {:.1}%", format_compact(entry.overheal), share * 100.0),
                share,
            ));
        }

        // A sidecar without the per-spell split would otherwise leave a
        // heading with nothing under it.
        if !entry.targets.is_empty() {
            lines.push(Line::Heading(if view == MeterMetric::DamageTaken {
                "Sources"
            } else {
                "Targets"
            }));
            for target in &entry.targets {
                let key = format!("st:{}:{}", target.marker, target.key);
                lines.push(Line::Bar(breakdown_bar(
                    key,
                    class,
                    target,
                    entry.amount,
                    false,
                )));
            }
        }
        self.set_lines(4, lines);
    }

    /// A clickable row: the fill visual in a flat button that runs
    /// `activate`. Rows persist across refreshes, so a plain `clicked` is
    /// reliable.
    fn row_button(
        self: &Rc<Self>,
        overlay: &gtk4::Overlay,
        activate: impl Fn(&Rc<Self>) + 'static,
    ) -> gtk4::Button {
        let button = gtk4::Button::new();
        button.add_css_class("flat");
        button.add_css_class("wr-meter-row");
        button.set_child(Some(overlay));
        let this = Rc::clone(self);
        button.connect_clicked(move |_| activate(&this));
        button
    }

    /// Navigate into the breakdown or spell detail a keyed row opens.
    fn open_row(self: &Rc<Self>, open: &Open) {
        match open {
            Open::Actor(guid) => self.breakdown.replace(Some(guid.clone())),
            Open::Spell(key) => self.spell.replace(Some(key.clone())),
        };
        self.refresh();
    }
    /// Apply `lines` onto the content box: rows whose key is still present
    /// are updated in place and reordered, new keys get fresh rows, and rows
    /// whose key vanished are removed.
    fn set_lines(self: &Rc<Self>, spacing: i32, lines: Vec<Line>) {
        self.restore_content();
        self.content.set_spacing(spacing);
        let lines: Vec<(String, Line)> = lines.into_iter().map(|line| (line.key(), line)).collect();
        let keys: HashSet<&str> = lines.iter().map(|(key, _)| key.as_str()).collect();
        let mut rows = self.rows.borrow_mut();
        rows.retain(|key, row| {
            let keep = keys.contains(key.as_str());
            if !keep {
                self.remove_row(row);
            }
            keep
        });
        let mut previous: Option<gtk4::Widget> = None;
        for (key, line) in &lines {
            let row = rows.entry(key.clone()).or_insert_with(|| {
                let row = self.new_row(line);
                self.content.append(&row.widget());
                row
            });
            let widget = row.widget();
            if widget.prev_sibling() != previous {
                self.content.reorder_child_after(&widget, previous.as_ref());
            }
            if let (RowWidgets::Bar(row), Line::Bar(bar)) = (&*row, line) {
                row.update(bar);
                if bar.spell_icon && !row.has_icon.get() {
                    row.has_icon
                        .set(self.attach_spell_icon(&row.line, &bar.left));
                }
            }
            previous = Some(widget);
        }
    }

    /// Fresh widgets for a keyed line; `set_lines` fills in the values.
    fn new_row(self: &Rc<Self>, line: &Line) -> RowWidgets {
        let bar = match line {
            Line::Heading(text) => return RowWidgets::Heading(heading(text)),
            Line::Bar(bar) => bar,
        };
        let widgets = bar_widgets(bar.class);
        widgets.overlay.add_css_class("wr-meter-row");
        let root = match &bar.open {
            Some(open) => {
                let open = open.clone();
                self.row_button(&widgets.overlay, move |this| this.open_row(&open))
                    .upcast()
            }
            None => widgets.overlay.clone().upcast(),
        };
        RowWidgets::Bar(BarRow::new(root, widgets))
    }

    /// Drop every keyed row; the next `set_lines` builds them afresh.
    fn clear_rows(&self) {
        for (_, row) in self.rows.borrow_mut().drain() {
            self.remove_row(&row);
        }
    }

    /// Remove a keyed row's widget. A hovered icon goes with it, and the
    /// tooltip must not outlive the row it describes.
    fn remove_row(&self, row: &RowWidgets) {
        if matches!(row, RowWidgets::Bar(bar) if bar.has_icon.get()) {
            self.hide_tooltip();
        }
        self.content.remove(&row.widget());
    }

    /// Seek the player a beat before `at_ms`, so the event plays rather than
    /// having just happened. Inert until the player installs the callback.
    fn seek_to(&self, at_ms: u64) {
        if let Some(seek) = self.seek.borrow().as_ref() {
            seek(at_ms.saturating_sub(SEEK_LEAD_MS));
        }
    }

    fn class_for(&self, guid: &str) -> Option<&'static str> {
        let entry = self.entry.borrow();
        entry
            .as_ref()?
            .spec_by_guid
            .get(guid)
            .and_then(|spec| class_css_class(*spec))
    }

    /// Parse the bundled spell database on GIO's blocking pool the first time
    /// the meter is shown. Rows render without icons and tooltips until it
    /// lands; one refresh then adds them.
    fn request_spell_db(self: &Rc<Self>) {
        if self.spell_db_requested.replace(true) {
            return;
        }
        let this = Rc::clone(self);
        gtk4::glib::spawn_future_local(async move {
            let parsed = gtk4::gio::spawn_blocking(|| {
                let bytes = gtk4::gio::resources_lookup_data(
                    SPELLS_JSON_RESOURCE,
                    gtk4::gio::ResourceLookupFlags::NONE,
                )
                .ok()?;
                // The bundle stays registered for the process lifetime, so
                // leaking this handle keeps nothing extra alive and lets the
                // index borrow the mapped text instead of copying it.
                let bytes: &'static [u8] = Box::leak(Box::new(bytes));
                SpellDb::parse(std::str::from_utf8(bytes).ok()?).ok()
            })
            .await;
            let Ok(Some(db)) = parsed else {
                tracing::warn!("spell database unavailable; meter spell icons disabled");
                return;
            };
            this.spell_db.replace(Some(db));
            // A cached history list was built without icons.
            this.list_cache.borrow_mut().take();
            this.refresh();
        });
    }

    /// The row's icon basename if the database knows this spell.
    fn spell_icon_basename(&self, name: &str) -> Option<&'static str> {
        let db = self.spell_db.borrow();
        db.as_ref()?.lookup(name).map(|info| info.icon)
    }

    /// A cached icon texture for `basename`, decoded once from the resource.
    fn icon_texture(&self, basename: &str) -> Option<Texture> {
        if let Some(texture) = self.icons.borrow().get(basename) {
            return Some(texture.clone());
        }
        let resource = format!("{SPELL_ICON_RESOURCE}{basename}.png");
        let bytes =
            gtk4::gio::resources_lookup_data(&resource, gtk4::gio::ResourceLookupFlags::NONE)
                .ok()?;
        let texture = Texture::from_bytes(&bytes).ok()?;
        self.icons
            .borrow_mut()
            .insert(basename.to_owned(), texture.clone());
        Some(texture)
    }

    /// Prepend a small spell icon to a row's label line and arm its hover
    /// tooltip. False when the spell has no bundled icon.
    fn attach_spell_icon(self: &Rc<Self>, line: &gtk4::Box, spell: &str) -> bool {
        let Some(basename) = self.spell_icon_basename(spell) else {
            return false;
        };
        let Some(texture) = self.icon_texture(basename) else {
            return false;
        };
        let icon = gtk4::Picture::for_paintable(&texture);
        icon.add_css_class("wr-spell-icon");
        icon.set_size_request(SPELL_ICON_SIZE, SPELL_ICON_SIZE);
        line.prepend(&icon);
        // Rows persist across refreshes, so the icon's own crossing events
        // are the whole hover state; GTK also sends `leave` when a hovered
        // row is removed.
        let motion = gtk4::EventControllerMotion::new();
        {
            let this = Rc::clone(self);
            let spell = spell.to_owned();
            motion.connect_enter(move |_, _, _| this.show_tooltip(&spell));
        }
        {
            let this = Rc::clone(self);
            motion.connect_leave(move |_| this.hide_tooltip());
        }
        icon.add_controller(motion);
        true
    }

    /// Populate the shared tooltip and place it directly left of the meter,
    /// bottom-aligned with it inside the video overlay.
    /// Anchoring to the meter rather than the hovered row keeps the panel
    /// still while rows reorder around it.
    fn show_tooltip(&self, spell: &str) {
        let Some(info) = self
            .spell_db
            .borrow()
            .as_ref()
            .and_then(|db| db.lookup(spell).cloned())
        else {
            return;
        };
        if let Some(texture) = self.icon_texture(info.icon) {
            self.tooltip_icon.set_paintable(Some(&texture));
            self.tooltip_icon.set_visible(true);
        } else {
            self.tooltip_icon.set_visible(false);
        }
        self.tooltip_name.set_text(spell);
        self.tooltip_desc.set_text(&info.description);
        self.tooltip_desc.set_visible(!info.description.is_empty());
        let overlay_borrow = self.overlay.borrow();
        let Some(overlay) = overlay_borrow.as_ref() else {
            return;
        };
        let overlay_width = overlay.width();
        if overlay_width <= 0 {
            return;
        }
        // Both widgets are end/bottom aligned overlay children. Reuse the
        // meter's stable margins and allocated width instead of measuring its
        // height, which follows the row count. Their lower edges then
        // remain level and the tooltip cannot fall into the playback bar.
        let margin_end =
            (self.root.margin_end() + self.root.width() + TOOLTIP_GAP).clamp(0, overlay_width);
        self.tooltip.set_halign(gtk4::Align::End);
        self.tooltip.set_valign(gtk4::Align::End);
        self.tooltip.set_margin_top(0);
        self.tooltip.set_margin_end(margin_end);
        self.tooltip.set_margin_bottom(self.root.margin_bottom());
        self.tooltip.set_visible(true);
    }

    fn hide_tooltip(&self) {
        self.tooltip.set_visible(false);
    }

    fn show_empty(&self, message: &str) {
        self.clear_content();
        self.empty_label.set_text(message);
        self.content.append(&self.empty_label);
    }

    fn clear_content(&self) {
        // A history list row may have carried the hovered icon.
        self.hide_tooltip();
        self.restore_content();
        self.clear_rows();
    }

    /// Drop any history list and empty message so the content box is the
    /// scroller's child again.
    fn restore_content(&self) {
        self.list_cache.borrow_mut().take();
        // A history list may have replaced the content box as the scroller's
        // child; normal content always lives in the box again.
        if self.scroller.child().as_ref() != Some(self.content.upcast_ref()) {
            self.scroller.set_child(Some(&self.content));
        }
        if self.empty_label.parent().is_some() {
            self.content.remove(&self.empty_label);
        }
    }

    /// Whether the scroller currently shows the history list built for `key`.
    fn list_cached(&self, key: &str) -> bool {
        self.list_cache
            .borrow()
            .as_ref()
            .is_some_and(|(cached_key, cached)| {
                cached_key == key && self.scroller.child().as_ref() == Some(cached.upcast_ref())
            })
    }

    /// Show a virtualized history list; callers only reach here on a cache
    /// miss, so the previous content is dropped and the list cached.
    fn set_list_content(&self, key: &str, list: &gtk4::ListView) {
        self.clear_content();
        self.list_cache
            .borrow_mut()
            .replace((key.to_owned(), list.clone()));
        self.scroller.set_child(Some(list));
    }

    /// Grip drag: resize the root from the captured geometry, clamped to the
    /// current viewport. The margins preserve the top-left until the grip
    /// reaches the viewport edge, after which growth continues left/up.
    /// Sets the request and margins directly: `move_to` clamps against the
    /// stale allocation, which only the relayout this request queues
    /// updates.
    fn resize_to(
        &self,
        margin_end: i32,
        margin_bottom: i32,
        width: i32,
        height: i32,
        offset_x: f64,
        offset_y: f64,
    ) {
        let Some((viewport_width, viewport_height)) = self.viewport_size() else {
            return;
        };
        if viewport_width <= 0 || viewport_height <= 0 {
            return;
        }
        let target_width = (width as f64 + offset_x)
            .clamp(
                f64::from(MIN_WIDTH.min(viewport_width)),
                f64::from(viewport_width),
            )
            .round() as i32;
        let target_height = (height as f64 + offset_y)
            .clamp(
                f64::from(MIN_HEIGHT.min(viewport_height)),
                f64::from(viewport_height),
            )
            .round() as i32;
        let end =
            (margin_end - (target_width - width)).clamp(0, (viewport_width - target_width).max(0));
        let bottom = (margin_bottom - (target_height - height))
            .clamp(0, (viewport_height - target_height).max(0));
        self.root.set_size_request(target_width, target_height);
        self.root.set_margin_end(end);
        self.root.set_margin_bottom(bottom);
        self.desired_size.set(Some((target_width, target_height)));
    }

    /// Pixel margins from a drag, clamped so the meter stays inside the
    /// overlay allocation.
    fn move_to(&self, margin_end: f64, margin_bottom: f64) {
        let Some((width, height)) = self.viewport_size() else {
            return;
        };
        let max_end = (width - self.root.width()).max(0) as f64;
        let max_bottom = (height - self.root.height()).max(0) as f64;
        self.root
            .set_margin_end(margin_end.clamp(0.0, max_end) as i32);
        self.root
            .set_margin_bottom(margin_bottom.clamp(0.0, max_bottom) as i32);
    }

    /// The overlay allocation the meter is positioned in: the video overlay
    /// it was added to.
    fn viewport_size(&self) -> Option<(i32, i32)> {
        let overlay = self.root.parent().and_downcast::<gtk4::Overlay>()?;
        Some((overlay.width(), overlay.height()))
    }
}

/// The widgets of one dense meter row: class-colored fill behind
/// always-white labels, left label expanding, right label aligned end.
struct BarWidgets {
    overlay: gtk4::Overlay,
    fill: gtk4::ProgressBar,
    line: gtk4::Box,
    left: gtk4::Label,
    right: gtk4::Label,
}

fn bar_widgets(class: Option<&str>) -> BarWidgets {
    let fill = gtk4::ProgressBar::new();
    fill.set_show_text(false);
    fill.add_css_class("wr-meter-fill");
    if let Some(class) = class {
        fill.add_css_class(class);
    }
    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    line.set_margin_start(6);
    line.set_margin_end(6);
    let left = gtk4::Label::new(None);
    left.set_xalign(0.0);
    left.set_hexpand(true);
    left.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let right = gtk4::Label::new(None);
    right.set_xalign(1.0);
    right.add_css_class("numeric");
    line.append(&left);
    line.append(&right);
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&fill));
    overlay.add_overlay(&line);
    BarWidgets {
        overlay,
        fill,
        line,
        left,
        right,
    }
}

/// A static meter row for the virtualized histories: the bar simply shows
/// `fraction`.
fn fill_line(class: Option<&str>, left: &str, right: &str, fraction: f64) -> BarWidgets {
    let widgets = bar_widgets(class);
    widgets.left.set_label(left);
    widgets.right.set_label(right);
    widgets.fill.set_fraction(fraction.clamp(0.0, 1.0));
    widgets
}

/// One breakdown line, sharing the ranking row visual with the fill
/// proportional to the share. Spell rows keep hit counts in their detail
/// view; target/source rows show them inline.
fn breakdown_bar(
    key: String,
    class: Option<&'static str>,
    entry: &ProjectedEntry,
    total: u64,
    spell: bool,
) -> Bar {
    let share = if total == 0 {
        0.0
    } else {
        entry.amount as f64 / total as f64 * 100.0
    };
    let right = if spell {
        format!("{} {:.1}%", format_compact(entry.amount), share)
    } else {
        format!(
            "{} {:.1}% {}",
            format_compact(entry.amount),
            share,
            entry.hits
        )
    };
    Bar {
        key,
        class,
        left: entry.key.clone(),
        right,
        fraction: share / 100.0,
        spell_icon: spell,
        open: None,
    }
}
fn heading(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.add_css_class("caption-heading");
    label.set_xalign(0.0);
    label
}

/// Cache key for a virtualized occurrence history: the list is frozen for the
/// fight, so len plus first/last times identify it between playhead ticks.
fn occurrence_key(view: MeterMetric, guid: &str, entry: &ProjectedEntry) -> String {
    format!(
        "occ:{}:{}:{}:{}:{:?}:{:?}",
        view_key(View::Metric(view)),
        guid,
        entry.key,
        entry.times.len(),
        entry.times.first(),
        entry.times.last(),
    )
}

/// Cache key for a virtualized death history: the actor filter plus the list
/// bounds.
fn death_list_key(guid: &str, deaths: &[&MeterDeath]) -> String {
    format!(
        "death:{}:{}:{:?}:{:?}",
        guid,
        deaths.len(),
        deaths.first().map(|death| death.at_ms),
        deaths.last().map(|death| death.at_ms),
    )
}

/// A virtualized ListView over owned row data: GtkListBase only builds the
/// rows scrolled into view, so a history with hundreds of entries costs the
/// same as a screenful. Rows are produced by `build` on each bind; clicks
/// live on the rows themselves.
fn history_list<T: 'static>(
    items: Vec<T>,
    build: impl Fn(&T) -> gtk4::Widget + 'static,
) -> gtk4::ListView {
    let model = gtk4::gio::ListStore::new::<gtk4::glib::BoxedAnyObject>();
    for item in items {
        model.append(&gtk4::glib::BoxedAnyObject::new(item));
    }
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_bind(move |_, factory_item| {
        let Some(list_item) = factory_item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let Some(item) = list_item.item() else {
            return;
        };
        let Some(boxed) = item.downcast_ref::<gtk4::glib::BoxedAnyObject>() else {
            return;
        };
        let data = boxed.borrow::<T>();
        list_item.set_child(Some(&build(&data)));
    });
    gtk4::ListView::new(Some(gtk4::NoSelection::new(Some(model))), Some(factory))
}

/// A stateful string action for the meter action group.
fn stateful_action(name: &str, state: &str) -> gtk4::gio::SimpleAction {
    gtk4::gio::SimpleAction::new_stateful(
        name,
        Some(gtk4::glib::VariantTy::STRING),
        &state.to_variant(),
    )
}
