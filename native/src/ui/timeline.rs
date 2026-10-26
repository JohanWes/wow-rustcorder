// SPDX-License-Identifier: GPL-3.0-or-later

//! The combat seek track: one `GtkDrawingArea` drawing activity spans, death
//! markers, encounter/round boundaries, the playhead, and clip handles
//! directly from `TimelineItem`s. Offsets convert to pixels at draw/hit-test
//! time; visibility preferences filter drawn items only.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use warcraft_rustcorder::domain::{
    Category, DeathMarkerVisibility, LibraryEntry, MarkerVisibility, Outcome, TimelineItem,
    TimelineKind, TimelineShape,
};

use super::format_clock;

/// Marker visibility preferences, mirrored from `InterfaceSettings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerPrefs {
    pub deaths: DeathMarkerVisibility,
    pub encounters: MarkerVisibility,
    pub rounds: MarkerVisibility,
}

/// Clip handles in milliseconds, always `0 <= start < end <= duration`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipRangeMs {
    pub start_ms: u64,
    pub end_ms: u64,
}

/// Initial clip range: current position ±15 s, clamped to media.
pub fn initial_clip_range(position_ms: u64, duration_ms: u64) -> ClipRangeMs {
    let start_ms = position_ms.saturating_sub(15_000);
    let end_ms = (position_ms + 15_000).min(duration_ms);
    ClipRangeMs {
        start_ms,
        end_ms: end_ms.max(start_ms + 1).min(duration_ms.max(1)),
    }
}

/// Move one clip handle, preserving `start < end` within the media.
pub fn drag_clip_handle(range: ClipRangeMs, start_handle: bool, to_ms: u64) -> ClipRangeMs {
    if start_handle {
        ClipRangeMs {
            start_ms: to_ms.min(range.end_ms.saturating_sub(1)),
            end_ms: range.end_ms,
        }
    } else {
        ClipRangeMs {
            start_ms: range.start_ms,
            end_ms: to_ms.max(range.start_ms + 1),
        }
    }
}

/// A player name without its `-Realm` suffix.
pub fn plain_name(name: &str) -> &str {
    name.split('-').next().unwrap_or(name)
}

/// The timeline items the preferences allow for this entry. Clips draw no
/// markers: their metadata is lifted from the parent and meaningless here.
pub fn visible_items(entry: &LibraryEntry, prefs: MarkerPrefs) -> Vec<&TimelineItem> {
    if entry.category == Category::Clip {
        return Vec::new();
    }
    let own = entry
        .player
        .as_ref()
        .map(|player| plain_name(&player.name).to_owned());
    entry
        .timeline
        .iter()
        .filter(|item| match item.kind() {
            TimelineKind::Death => match prefs.deaths {
                DeathMarkerVisibility::None => false,
                DeathMarkerVisibility::All => true,
                DeathMarkerVisibility::Own => match (&own, item.label()) {
                    (Some(own), Some(label)) => plain_name(label) == own,
                    _ => false,
                },
            },
            TimelineKind::Bloodlust => true,
            TimelineKind::Encounter | TimelineKind::Trash => {
                prefs.encounters == MarkerVisibility::Visible
            }
            TimelineKind::Round => prefs.rounds == MarkerVisibility::Visible,
            TimelineKind::Activity | TimelineKind::Unknown(_) => true,
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerDirection {
    Previous,
    Next,
}

/// Start of the nearest visible marker strictly before or after the playhead.
pub fn marker_target(
    entry: &LibraryEntry,
    prefs: MarkerPrefs,
    position_ms: u64,
    direction: MarkerDirection,
) -> Option<u64> {
    visible_items(entry, prefs)
        .into_iter()
        .map(TimelineItem::start_ms)
        .filter(|start_ms| match direction {
            MarkerDirection::Previous => *start_ms < position_ms,
            MarkerDirection::Next => *start_ms > position_ms,
        })
        .reduce(|best, start_ms| match direction {
            MarkerDirection::Previous => best.max(start_ms),
            MarkerDirection::Next => best.min(start_ms),
        })
}

pub fn ms_to_x(ms: u64, duration_ms: u64, width: f64) -> f64 {
    if duration_ms == 0 {
        return 0.0;
    }
    (ms as f64 / duration_ms as f64) * width
}

pub fn x_to_ms(x: f64, duration_ms: u64, width: f64) -> u64 {
    if width <= 0.0 {
        return 0;
    }
    ((x.clamp(0.0, width) / width) * duration_ms as f64).round() as u64
}

/// `(start_ms, end_ms, label)` of one visible item, for tooltip hit-testing.
pub type LabelledItem = (u64, Option<u64>, Option<String>);

/// The hover/focus label: the nearest visible item within `radius_px` of `x`
/// (a span counts as zero distance from anywhere inside it), with its
/// timestamp.
pub fn hover_label(
    items: &[LabelledItem],
    x: f64,
    duration_ms: u64,
    width: f64,
    radius_px: f64,
) -> Option<String> {
    let mut best: Option<(f64, &LabelledItem)> = None;
    for item in items {
        let start_x = ms_to_x(item.0, duration_ms, width);
        let distance = match item.1 {
            Some(end_ms) => {
                let end_x = ms_to_x(end_ms, duration_ms, width);
                if x >= start_x && x <= end_x {
                    0.0
                } else {
                    (x - start_x).abs().min((x - end_x).abs())
                }
            }
            None => (x - start_x).abs(),
        };
        if distance <= radius_px && best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, item));
        }
    }
    best.map(|(_, (start_ms, _, label))| {
        let time = format_clock(*start_ms);
        match label {
            Some(label) => format!("{label} - {time}"),
            None => time,
        }
    })
}

// --- Widget ---

const TRACK_HEIGHT: i32 = 34;
const HANDLE_RADIUS: f64 = 5.0;

/// Which handle a pointer press grabbed in clip mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grab {
    Start,
    End,
    Seek,
}

pub struct Timeline {
    pub widget: gtk4::DrawingArea,
    state: Rc<State>,
}

/// A position to seek to, and whether it may be served from the nearest
/// keyframe instead of decoded exactly.
type SeekRequest = (u64, bool);

/// The seek requests one pointer grab is allowed to produce.
///
/// A press lands exactly where it was aimed, motion during the drag may snap
/// to a keyframe so the picture keeps up, and the release settles precisely,
/// but only if the drag actually moved. Emitting a second seek for a single
/// click makes the decoder present a second frame, which reads as a flicker.
#[derive(Clone, Copy, Default)]
struct SeekGesture {
    /// Last position emitted, so a pointer that has not reached a new
    /// millisecond cannot seek again.
    last_ms: Option<u64>,
    /// Newest position a preview asked for, replayed precisely on release.
    previewed_ms: Option<u64>,
}

impl SeekGesture {
    fn press(&mut self, ms: u64) -> SeekRequest {
        self.last_ms = Some(ms);
        self.previewed_ms = None;
        (ms, false)
    }

    fn motion(&mut self, ms: u64) -> Option<SeekRequest> {
        if self.last_ms == Some(ms) {
            return None;
        }
        self.last_ms = Some(ms);
        self.previewed_ms = Some(ms);
        Some((ms, true))
    }

    fn release(&mut self) -> Option<SeekRequest> {
        self.last_ms = None;
        self.previewed_ms.take().map(|ms| (ms, false))
    }
}

struct State {
    duration_ms: Cell<u64>,
    position_ms: Cell<u64>,
    /// The position the last paint drew, so playback only repaints once the
    /// playhead has moved a visible distance.
    painted_position_ms: Cell<u64>,
    items: RefCell<Vec<OwnedItem>>,
    clip: Cell<Option<ClipRangeMs>>,
    grab: Cell<Option<Grab>>,
    #[allow(clippy::type_complexity)]
    /// `(position_ms, scrubbing)`. A scrubbing seek is a preview that the
    /// caller may serve from the nearest keyframe; the release is not.
    on_seek: RefCell<Option<Box<dyn Fn(u64, bool)>>>,
    seek_gesture: Cell<SeekGesture>,
    labels: RefCell<Vec<LabelledItem>>,
}

/// A draw-ready copy of one visible item, so redraws never re-filter.
struct OwnedItem {
    span: bool,
    death: bool,
    bloodlust: bool,
    start_ms: u64,
    end_ms: u64,
    outcome: Option<Outcome>,
}

impl Timeline {
    pub fn new() -> Self {
        let widget = gtk4::DrawingArea::new();
        widget.set_content_height(TRACK_HEIGHT);
        widget.set_hexpand(true);
        widget.set_focusable(true);
        widget.set_has_tooltip(true);
        let state = Rc::new(State {
            duration_ms: Cell::new(0),
            position_ms: Cell::new(0),
            painted_position_ms: Cell::new(0),
            items: RefCell::new(Vec::new()),
            clip: Cell::new(None),
            grab: Cell::new(None),
            on_seek: RefCell::new(None),
            seek_gesture: Cell::new(SeekGesture::default()),
            labels: RefCell::new(Vec::new()),
        });
        let timeline = Self { widget, state };
        timeline.connect_draw();
        timeline.connect_pointer();
        timeline.connect_tooltip();
        timeline
    }

    pub fn connect_seek(&self, on_seek: impl Fn(u64, bool) + 'static) {
        *self.state.on_seek.borrow_mut() = Some(Box::new(on_seek));
    }

    /// Replace the drawn markers from the entry and preferences.
    pub fn set_entry(&self, entry: Option<&LibraryEntry>, prefs: MarkerPrefs) {
        let mut items = self.state.items.borrow_mut();
        let mut labels = self.state.labels.borrow_mut();
        items.clear();
        labels.clear();
        if let Some(entry) = entry {
            self.state.duration_ms.set(entry.duration_ms.max(1));
            for item in visible_items(entry, prefs) {
                items.push(OwnedItem {
                    span: item.shape() == TimelineShape::Span,
                    death: item.kind() == &TimelineKind::Death,
                    bloodlust: item.kind() == &TimelineKind::Bloodlust,
                    start_ms: item.start_ms(),
                    end_ms: item.end_ms().unwrap_or(item.start_ms()),
                    outcome: item.outcome(),
                });
                labels.push((
                    item.start_ms(),
                    item.end_ms(),
                    item.label().map(str::to_owned),
                ));
            }
        } else {
            self.state.duration_ms.set(0);
        }
        drop(items);
        drop(labels);
        self.widget.queue_draw();
    }

    /// Ignored while the pointer owns the playhead: a drag's preview seeks
    /// report back the keyframe they landed on, and letting that through would
    /// pull the playhead backwards out from under the pointer. On a long
    /// recording one pixel spans seconds, so this only repaints once the
    /// playhead is half a pixel from where it was last painted.
    pub fn set_position(&self, position_ms: u64) {
        if self.state.grab.get() == Some(Grab::Seek) {
            return;
        }
        self.state.position_ms.set(position_ms);
        let duration = self.state.duration_ms.get();
        let width = f64::from(self.widget.width());
        let painted = ms_to_x(self.state.painted_position_ms.get(), duration, width);
        if (ms_to_x(position_ms, duration, width) - painted).abs() >= 0.5 {
            self.widget.queue_draw();
        }
    }

    pub fn set_clip(&self, clip: Option<ClipRangeMs>) {
        self.state.clip.set(clip);
        self.widget.queue_draw();
    }

    pub fn clip(&self) -> Option<ClipRangeMs> {
        self.state.clip.get()
    }

    fn connect_draw(&self) {
        let state = Rc::clone(&self.state);
        self.widget.set_draw_func(move |_, cr, width, height| {
            let width = f64::from(width);
            let height = f64::from(height);
            let duration = state.duration_ms.get();
            let mid = height / 2.0;
            state.painted_position_ms.set(state.position_ms.get());

            // Neutral rail keeps red available for actual failure outcomes.
            cr.set_source_rgba(0.18, 0.20, 0.23, 1.0);
            rounded_bar(cr, 0.0, mid - 3.0, width, 6.0);
            let _ = cr.fill();
            if duration == 0 {
                return;
            }

            // Spans (encounter/trash/round/activity) in stable outcome colors.
            for item in state.items.borrow().iter().filter(|item| item.span) {
                let start = ms_to_x(item.start_ms, duration, width);
                let end = ms_to_x(item.end_ms, duration, width).max(start + 2.0);
                if item.bloodlust {
                    cr.set_source_rgba(0.45, 0.16, 0.68, 0.9);
                } else {
                    match item.outcome {
                        Some(Outcome::Win | Outcome::Complete) => {
                            cr.set_source_rgba(0.12, 1.0, 0.0, 0.55);
                        }
                        Some(Outcome::Loss | Outcome::Abandoned) => {
                            cr.set_source_rgba(1.0, 0.0, 0.0, 0.55);
                        }
                        Some(Outcome::Unknown) | None => {
                            cr.set_source_rgba(0.30, 0.38, 0.46, 0.9);
                        }
                    }
                }
                rounded_bar(cr, start, mid - 3.0, end - start, 6.0);
                let _ = cr.fill();
            }

            // Elapsed track up to the playhead (accent #bb4220).
            let played = ms_to_x(state.position_ms.get(), duration, width);
            cr.set_source_rgba(0.73, 0.26, 0.13, 0.9);
            rounded_bar(cr, 0.0, mid - 1.5, played, 3.0);
            let _ = cr.fill();

            // Point markers: deaths as gravestones, other points as ticks.
            for item in state.items.borrow().iter().filter(|item| !item.span) {
                let x = marker_x(ms_to_x(item.start_ms, duration, width), width);
                if item.death {
                    draw_death_marker(cr, x, mid);
                } else {
                    match item.outcome {
                        Some(Outcome::Win | Outcome::Complete) => {
                            cr.set_source_rgba(0.12, 1.0, 0.0, 1.0);
                        }
                        Some(Outcome::Loss | Outcome::Abandoned) => {
                            cr.set_source_rgba(1.0, 0.0, 0.0, 1.0);
                        }
                        Some(Outcome::Unknown) | None => {
                            cr.set_source_rgba(0.65, 0.72, 0.80, 1.0);
                        }
                    }
                    cr.rectangle(x - 1.0, mid - 7.0, 2.0, 14.0);
                    let _ = cr.fill();
                }
            }

            // Clip range and handles.
            if let Some(clip) = state.clip.get() {
                let start = ms_to_x(clip.start_ms, duration, width);
                let end = ms_to_x(clip.end_ms, duration, width);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
                cr.rectangle(start, 1.0, end - start, height - 2.0);
                let _ = cr.fill();
                cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                for x in [start, end] {
                    cr.arc(x, mid, HANDLE_RADIUS, 0.0, std::f64::consts::TAU);
                    let _ = cr.fill();
                }
            }

            // Playhead.
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            cr.arc(
                marker_x(played, width),
                mid,
                5.0,
                0.0,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
        });
    }

    fn connect_pointer(&self) {
        let drag = gtk4::GestureDrag::new();
        let state = Rc::clone(&self.state);
        let widget = self.widget.clone();
        drag.connect_drag_begin(move |_, x, _| {
            let duration = state.duration_ms.get();
            if duration == 0 {
                return;
            }
            let width = f64::from(widget.width());
            let grab = match state.clip.get() {
                Some(clip) => {
                    let start_x = ms_to_x(clip.start_ms, duration, width);
                    let end_x = ms_to_x(clip.end_ms, duration, width);
                    if (x - start_x).abs() <= HANDLE_RADIUS * 2.0 {
                        Grab::Start
                    } else if (x - end_x).abs() <= HANDLE_RADIUS * 2.0 {
                        Grab::End
                    } else {
                        Grab::Seek
                    }
                }
                None => Grab::Seek,
            };
            state.grab.set(Some(grab));
            // A press lands exactly where it was aimed; only the motion that
            // follows it is a preview.
            apply_pointer(&state, &widget, x, false);
        });
        let state = Rc::clone(&self.state);
        let widget = self.widget.clone();
        drag.connect_drag_update(move |gesture, offset_x, _| {
            if let Some((start_x, _)) = gesture.start_point() {
                apply_pointer(&state, &widget, start_x + offset_x, true);
            }
        });
        let state = Rc::clone(&self.state);
        drag.connect_drag_end(move |_, _, _| {
            let seeking = state.grab.get() == Some(Grab::Seek);
            state.grab.set(None);
            let mut gesture = state.seek_gesture.get();
            let request = gesture.release();
            state.seek_gesture.set(gesture);
            if let (true, Some((ms, scrubbing))) = (seeking, request)
                && let Some(on_seek) = state.on_seek.borrow().as_ref()
            {
                on_seek(ms, scrubbing);
            }
        });
        self.widget.add_controller(drag);
    }

    fn connect_tooltip(&self) {
        // Hover/focus label: nearest marker within 8 px, else the timestamp.
        let state = Rc::clone(&self.state);
        self.widget
            .connect_query_tooltip(move |widget, x, _, keyboard, tooltip| {
                let duration_ms = state.duration_ms.get();
                if duration_ms == 0 {
                    return false;
                }
                let width = f64::from(widget.width());
                let x = if keyboard {
                    ms_to_x(state.position_ms.get(), duration_ms, width)
                } else {
                    f64::from(x)
                };
                let text = hover_label(&state.labels.borrow(), x, duration_ms, width, 8.0)
                    .unwrap_or_else(|| format_clock(x_to_ms(x, duration_ms, width)));
                tooltip.set_text(Some(&text));
                true
            });
    }
}

fn draw_death_marker(cr: &gtk4::cairo::Context, x: f64, mid: f64) {
    // A white event tick above the rail, a muted grey-purple gravestone below.
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.rectangle(x - 1.0, mid - 4.0, 2.0, 8.0);
    let _ = cr.fill();

    let stone_mid = mid + 10.0;
    cr.set_source_rgba(0.40, 0.35, 0.45, 1.0);
    cr.move_to(x - 4.0, stone_mid + 6.0);
    cr.line_to(x - 4.0, stone_mid - 1.0);
    cr.curve_to(
        x - 4.0,
        stone_mid - 6.0,
        x + 4.0,
        stone_mid - 6.0,
        x + 4.0,
        stone_mid - 1.0,
    );
    cr.line_to(x + 4.0, stone_mid + 6.0);
    cr.close_path();
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.15, 0.08, 0.08, 1.0);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    cr.move_to(x - 2.0, stone_mid - 1.0);
    cr.line_to(x + 2.0, stone_mid - 1.0);
    cr.move_to(x, stone_mid - 3.0);
    cr.line_to(x, stone_mid + 2.0);
    let _ = cr.stroke();
}

fn marker_x(x: f64, width: f64) -> f64 {
    x.clamp(5.0, (width - 5.0).max(5.0))
}

/// Route a pointer x according to the active grab: seek or move a handle.
/// `scrubbing` marks motion during a drag, which may be served from the
/// nearest keyframe; the press and the release are exact.
fn apply_pointer(state: &Rc<State>, widget: &gtk4::DrawingArea, x: f64, scrubbing: bool) {
    let duration = state.duration_ms.get();
    if duration == 0 {
        return;
    }
    let ms = x_to_ms(x, duration, f64::from(widget.width()));
    match state.grab.get() {
        Some(Grab::Start) | Some(Grab::End) => {
            if let Some(clip) = state.clip.get() {
                let start = state.grab.get() == Some(Grab::Start);
                state.clip.set(Some(drag_clip_handle(clip, start, ms)));
                widget.queue_draw();
            }
        }
        _ => {
            let mut gesture = state.seek_gesture.get();
            let request = if scrubbing {
                gesture.motion(ms)
            } else {
                Some(gesture.press(ms))
            };
            state.seek_gesture.set(gesture);
            let Some((ms, scrubbing)) = request else {
                return;
            };
            // The pointer owns the playhead for the length of the grab.
            if state.position_ms.replace(ms) != ms {
                widget.queue_draw();
            }
            if let Some(on_seek) = state.on_seek.borrow().as_ref() {
                on_seek(ms, scrubbing);
            }
        }
    }
}

fn rounded_bar(cr: &gtk4::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let radius = (height / 2.0).min(width / 2.0).max(0.0);
    if width <= 0.0 {
        return;
    }
    cr.new_sub_path();
    cr.arc(
        x + width - radius,
        y + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;
    use warcraft_rustcorder::domain::PlayerSummary;

    fn entry_with(category: Category, timeline: Vec<TimelineItem>) -> LibraryEntry {
        let mut entry = crate::ui::window::tests::entry(category, "T", 0);
        entry.timeline = timeline;
        entry.player = Some(PlayerSummary {
            name: "Alice-Realm".to_owned(),
            realm: None,
            guid: None,
            spec_id: None,
        });
        entry
    }

    fn death(name: &str, at: u64) -> TimelineItem {
        TimelineItem::point(
            TimelineKind::Death,
            at,
            Some(name.to_owned()),
            Some(Outcome::Loss),
            None,
        )
    }

    #[test]
    fn visibility_filters_deaths_rounds_and_encounters_without_mutating_data() {
        let entry = entry_with(
            Category::MythicPlus,
            vec![
                death("Alice", 1_000),
                death("Bob", 2_000),
                TimelineItem::span(TimelineKind::Encounter, 0, 10_000, None, None, None).unwrap(),
                TimelineItem::span(TimelineKind::Round, 0, 5_000, None, None, None).unwrap(),
                TimelineItem::span(
                    TimelineKind::Bloodlust,
                    3_000,
                    43_000,
                    Some("Fury of the Aspects".to_owned()),
                    None,
                    None,
                )
                .unwrap(),
            ],
        );
        let all = MarkerPrefs {
            deaths: DeathMarkerVisibility::All,
            encounters: MarkerVisibility::Visible,
            rounds: MarkerVisibility::Visible,
        };
        assert_eq!(visible_items(&entry, all).len(), 5);

        let own_only = MarkerPrefs {
            deaths: DeathMarkerVisibility::Own,
            encounters: MarkerVisibility::Hidden,
            rounds: MarkerVisibility::Hidden,
        };
        let visible = visible_items(&entry, own_only);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].label(), Some("Alice"));
        assert_eq!(visible[1].kind(), &TimelineKind::Bloodlust);
        // Filtering is presentation only.
        assert_eq!(entry.timeline.len(), 5);

        // Clips never draw markers.
        let clip = entry_with(Category::Clip, vec![death("Alice", 1_000)]);
        assert!(visible_items(&clip, all).is_empty());
    }

    #[test]
    fn marker_target_uses_nearest_visible_marker_without_wrapping() {
        let all = MarkerPrefs {
            deaths: DeathMarkerVisibility::All,
            encounters: MarkerVisibility::Visible,
            rounds: MarkerVisibility::Visible,
        };
        let entry = entry_with(
            Category::MythicPlus,
            vec![
                death("Alice", 5_000),
                TimelineItem::span(TimelineKind::Encounter, 10_000, 40_000, None, None, None)
                    .unwrap(),
                death("Bob", 20_000),
            ],
        );

        assert_eq!(
            marker_target(&entry, all, 10_000, MarkerDirection::Previous),
            Some(5_000)
        );
        assert_eq!(
            marker_target(&entry, all, 10_000, MarkerDirection::Next),
            Some(20_000)
        );
        assert_eq!(
            marker_target(&entry, all, 0, MarkerDirection::Previous),
            None
        );
        assert_eq!(
            marker_target(&entry, all, 20_000, MarkerDirection::Next),
            None
        );

        let hidden = MarkerPrefs {
            deaths: DeathMarkerVisibility::None,
            encounters: MarkerVisibility::Hidden,
            rounds: MarkerVisibility::Hidden,
        };
        assert_eq!(
            marker_target(&entry, hidden, 15_000, MarkerDirection::Previous),
            None
        );
    }

    #[test]
    fn hover_prefers_the_nearest_item() {
        let items = vec![
            (30_000, None, Some("Alice".to_owned())),
            (60_000, None, Some("Bob".to_owned())),
        ];
        let x = ms_to_x(31_000, 120_000, 600.0);
        assert_eq!(
            hover_label(&items, x, 120_000, 600.0, 8.0).as_deref(),
            Some("Alice - 0:30")
        );
        // A span reports zero distance from anywhere inside it.
        let items = vec![(0, Some(120_000), Some("Round 1".to_owned()))];
        assert_eq!(
            hover_label(&items, 300.0, 120_000, 600.0, 8.0).as_deref(),
            Some("Round 1 - 0:00")
        );
    }

    #[test]
    fn clip_bounds_stay_ordered_inside_the_media() {
        let initial = initial_clip_range(5_000, 120_000);
        assert_eq!(initial.start_ms, 0);
        assert_eq!(initial.end_ms, 20_000);
        let tail = initial_clip_range(115_000, 120_000);
        assert_eq!(tail.end_ms, 120_000);

        let range = ClipRangeMs {
            start_ms: 10_000,
            end_ms: 20_000,
        };
        // The start handle cannot cross the end handle, and vice versa.
        assert_eq!(drag_clip_handle(range, true, 25_000).start_ms, 19_999);
        assert_eq!(drag_clip_handle(range, false, 5_000).end_ms, 10_001);
        assert_eq!(drag_clip_handle(range, true, 0).start_ms, 0);
    }

    #[test]
    fn one_pointer_action_produces_one_frame_worth_of_seeks() {
        // A click is a press and a release with nothing between: exactly one
        // exact seek, so the decoder presents exactly one frame.
        let mut click = SeekGesture::default();
        assert_eq!(click.press(4_000), (4_000, false));
        assert_eq!(click.release(), None);

        // A press that jitters without leaving its millisecond is still a
        // click; a repeated position must not become a keyframe preview.
        let mut jitter = SeekGesture::default();
        assert_eq!(jitter.press(4_000), (4_000, false));
        assert_eq!(jitter.motion(4_000), None);
        assert_eq!(jitter.release(), None);

        // A real drag previews while it moves and settles precisely on the
        // position it was released at.
        let mut drag = SeekGesture::default();
        assert_eq!(drag.press(4_000), (4_000, false));
        assert_eq!(drag.motion(4_500), Some((4_500, true)));
        assert_eq!(drag.motion(4_500), None);
        assert_eq!(drag.motion(9_000), Some((9_000, true)));
        assert_eq!(drag.release(), Some((9_000, false)));
        // The gesture is spent: it cannot settle twice.
        assert_eq!(drag.release(), None);
    }
}
