// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use clapper_gtk::prelude::AvExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gtk4::glib;
use gtk4::glib::prelude::Cast;

/// Per-stream byte limit for playbin3's demuxer queue. The default is
/// unlimited: the queue is sized by time, and GStreamer's AV1 parser only
/// timestamps the first frame of these recordings, so the queue never counts
/// itself full and reads the whole file into memory (5 GB for a long key).
///
/// Kept small because it is paid several times over: the limit lands on both
/// the source's and the decoder's queue, each fills completely within a second
/// of loading, and glibc keeps the freed buffers of the previous recording in
/// whichever arena the next one does not reuse. A local file needs no more
/// than a few frames queued. A high-bitrate recording gets more, see
/// `queue_bytes_for`.
const DEMUX_QUEUE_BYTES: u32 = 8 * 1024 * 1024;

/// The byte limit for the recording being opened; read when its queues are
/// created.
static QUEUE_BYTES: AtomicU32 = AtomicU32::new(DEMUX_QUEUE_BYTES);

/// Enough bytes for the interleave floor below at the recording's average
/// bitrate, with headroom for keyframes: below that the byte limit fills
/// first and pause/play wedge again (about 33 Mbps at 8 MiB).
pub fn queue_bytes_for(file_bytes: u64, duration_ms: u64) -> u32 {
    if duration_ms == 0 {
        return DEMUX_QUEUE_BYTES;
    }
    let floor_ms = DEMUX_MIN_INTERLEAVE_NS / 1_000_000;
    let needed = file_bytes.saturating_mul(floor_ms) / duration_ms * 3 / 2;
    needed.clamp(u64::from(DEMUX_QUEUE_BYTES), 128 * 1024 * 1024) as u32
}

/// Least time each multiqueue may hold ahead of the other streams. The
/// defaults (100 ms in decodebin3, 250 ms in urisourcebin) are less than the
/// audio sink buffers ahead. Pausing makes the audio sink wait for one more
/// buffer, but the demuxer can only reach it by pushing video into a queue
/// that is already full. Pause then never completes and play/pause stop
/// working, which a seek just before it triggered reliably on Windows with
/// 60 fps HEVC.
const DEMUX_MIN_INTERLEAVE_NS: u64 = 2_000_000_000;

/// How precisely a seek has to land, which decides how much decoding GStreamer
/// does before it can present a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekMode {
    /// Nearest keyframe. No decode from the previous keyframe, which is what
    /// lets the picture keep up with a dragged playhead.
    Preview,
    /// Clapper's default approximation: where the playhead comes to rest.
    Settle,
    /// Exact frame. Only worth its decode cost when the frame itself is the
    /// point, as in stepping backwards.
    Exact,
}

pub use clapper::PlayerState;

/// The concrete Clapper objects used by Warcraft Rustcorder's player UI.
#[derive(Clone)]
pub struct PlayerBackend {
    video: clapper_gtk::Video,
    player: clapper::Player,
}

#[derive(Clone)]
pub struct VideoStreamToken(clapper::VideoStream);

impl PlayerBackend {
    pub fn new() -> Result<Self, &'static str> {
        clapper::init()?;
        cap_demux_queues();
        let video = clapper_gtk::Video::new();
        let player = video.player().ok_or("ClapperGtk did not create a player")?;
        player.connect_current_video_decoder_notify(|player| {
            log_element("video decoder", player.current_video_decoder());
        });
        player.connect_current_audio_decoder_notify(|player| {
            log_element("audio decoder", player.current_audio_decoder());
        });
        log_playback_problems(&player);
        if cfg!(windows) {
            match front_stereo_sink() {
                Ok(sink) => player.set_audio_sink(Some(&sink)),
                Err(error) => tracing::warn!(%error, "keeping the default audio sink"),
            }
        }
        Ok(Self { video, player })
    }

    pub fn widget(&self) -> &clapper_gtk::Video {
        &self.video
    }

    /// `queue_bytes` comes from `queue_bytes_for`.
    pub fn open_uri(&self, uri: &str, queue_bytes: u32) -> Result<(), &'static str> {
        QUEUE_BYTES.store(queue_bytes, Ordering::Relaxed);
        let queue = self.player.queue().ok_or("Clapper player has no queue")?;
        let item = clapper::MediaItem::new(uri);
        queue.clear();
        queue.add_item(&item);
        if !queue.select_item(Some(&item)) {
            return Err("Clapper did not select the requested media item");
        }
        Ok(())
    }

    pub fn play(&self) {
        self.player.play();
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn seek(&self, position_seconds: f64, mode: SeekMode) {
        self.player.seek_custom(
            position_seconds,
            match mode {
                SeekMode::Preview => clapper::PlayerSeekMethod::Fast,
                SeekMode::Settle => clapper::PlayerSeekMethod::Normal,
                SeekMode::Exact => clapper::PlayerSeekMethod::Accurate,
            },
        );
    }

    pub fn set_volume(&self, volume: f64) {
        self.player.set_volume(volume);
    }

    pub fn set_muted(&self, muted: bool) {
        self.player.set_mute(muted);
    }

    pub fn set_speed(&self, speed: f64) {
        self.player.set_speed(speed);
    }

    pub fn advance_frame(&self) {
        self.player.advance_frame();
    }

    pub fn stop(&self) {
        self.player.stop();
    }

    pub fn video_stream_token(&self) -> Option<VideoStreamToken> {
        self.current_video_stream().map(VideoStreamToken)
    }

    /// Dimensions reported by a newly decoded active video stream.
    pub fn video_dimensions(
        &self,
        expected_uri: &str,
        previous_stream: Option<&VideoStreamToken>,
    ) -> Option<(u32, u32)> {
        if !self.is_ready() {
            return None;
        }
        let current_uri = self.player.queue()?.current_item()?.uri()?;
        if current_uri.as_str() != expected_uri {
            return None;
        }
        let stream = self.current_video_stream()?;
        if previous_stream.is_some_and(|previous| previous.0 == stream) {
            return None;
        }
        let width = u32::try_from(stream.width()).ok()?;
        let height = u32::try_from(stream.height()).ok()?;
        (width > 0 && height > 0).then_some((width, height))
    }

    fn current_video_stream(&self) -> Option<clapper::VideoStream> {
        self.player
            .video_streams()?
            .current_stream()?
            .downcast::<clapper::VideoStream>()
            .ok()
    }

    pub fn state(&self) -> PlayerState {
        self.player.state()
    }

    /// Playing or paused with media: seeks/steps are meaningful.
    pub fn is_ready(&self) -> bool {
        matches!(
            self.player.state(),
            clapper::PlayerState::Playing | clapper::PlayerState::Paused
        )
    }

    pub fn connect_position_updated(&self, callback: impl Fn(f64) + 'static) {
        self.player
            .connect_position_notify(move |player| callback(player.position()));
    }

    /// Typed player-state changes. Clapper suppresses these while a seek is
    /// progressing, so an arrival also proves any in-flight seek was
    /// abandoned and can never complete.
    pub fn connect_state_changed(&self, callback: impl Fn(PlayerState) + 'static) {
        self.player
            .connect_state_notify(move |player| callback(player.state()));
    }

    pub fn connect_seek_done(&self, callback: impl Fn() + 'static) {
        self.player.connect_seek_done(move |_| callback());
    }
}
/// Tunes every multiqueue as it joins a bin. Clapper hides its pipeline, and
/// Caps every multiqueue as it joins a bin. Clapper hides its pipeline, and
/// anything of ours it links in (a filter, the sink) only gets there once the
/// video chain is complete, by which time the source queue of the first
/// recording has read 140 MB ahead. A tracer is the one hook that runs first.
mod queue_cap {
    use gst::subclass::prelude::*;
    use gstreamer as gst;
    use gtk4::glib;

    #[derive(Default)]
    pub struct QueueCap;

    #[glib::object_subclass]
    impl ObjectSubclass for QueueCap {
        const NAME: &'static str = "WarcraftRustcorderQueueCap";
        type Type = super::QueueCap;
        type ParentType = gst::Tracer;
    }

    impl ObjectImpl for QueueCap {
        fn constructed(&self) {
            self.parent_constructed();
            self.register_hook(TracerHook::BinAddPost);
        }
    }

    impl GstObjectImpl for QueueCap {}

    impl TracerImpl for QueueCap {
        fn bin_add_post(&self, _ts: u64, _bin: &gst::Bin, element: &gst::Element, _added: bool) {
            super::cap_if_multiqueue(element);
        }
    }
}

glib::wrapper! {
    pub struct QueueCap(ObjectSubclass<queue_cap::QueueCap>) @extends gst::Tracer, gst::Object;
}

/// GStreamer never unregisters a tracer's hooks, so the one instance lives for
/// the process.
fn cap_demux_queues() {
    static TRACER: OnceLock<QueueCap> = OnceLock::new();
    TRACER.get_or_init(glib::Object::new);
}

fn cap_if_multiqueue(element: &gst::Element) {
    if element
        .factory()
        .is_some_and(|factory| factory.name() == "multiqueue")
    {
        element.set_property("max-size-bytes", QUEUE_BYTES.load(Ordering::Relaxed));
        // decodebin3 lowers the interleave again on every reset and latency
        // change, so the floor is reapplied whenever it moves.
        raise_interleave(element);
        element.connect_notify(Some("min-interleave-time"), |element, _| {
            raise_interleave(element);
        });
    }
}

fn raise_interleave(element: &gst::Element) {
    if element.property::<u64>("min-interleave-time") < DEMUX_MIN_INTERLEAVE_NS {
        element.set_property("min-interleave-time", DEMUX_MIN_INTERLEAVE_NS);
    }
}

/// An audio sink that plays stereo on the front left and right speakers only,
/// as Windows does for VLC. WASAPI only takes the device's own channel count,
/// and GStreamer's own stereo-to-5.1/7.1 upmix halves the fronts and spreads
/// the rest over center, LFE and surrounds, which virtual-surround headsets
/// render as muddy. `first-channels` maps channel n to channel n instead and
/// leaves the extra speakers silent. Nothing may convert between it and the
/// sink: it would let the matrix settle on stereo and upmix after all.
///
/// A mono or stereo device (a Bluetooth headset in hands-free mode is mono)
/// is offered as itself, so playsink's converter mixes to it as before rather
/// than the matrix dropping the right channel. The device is checked when a
/// recording opens; a mid-playback device switch is converted inside the sink.
fn front_stereo_sink() -> Result<gst::Bin, glib::Error> {
    let bin = gst::parse::bin_from_description(
        "audiomixmatrix name=matrix mode=first-channels ! wasapi2sink name=device",
        true,
    )?;
    let pad = |name| {
        bin.by_name(name)
            .and_then(|element| element.static_pad("sink"))
            .expect("the sink bin has both elements")
    };
    let device = pad("device");
    pad("matrix").add_probe(
        gst::PadProbeType::QUERY_DOWNSTREAM | gst::PadProbeType::PULL,
        move |_, info| {
            if let Some(gst::QueryViewMut::Caps(query)) = info.query_mut().map(|q| q.view_mut())
                && let Some(result) = query.result_owned()
            {
                let device_caps = device.query_caps(None);
                if at_most_stereo(&device_caps) {
                    query.set_result(&result.intersect(&device_caps));
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
    Ok(bin)
}

fn at_most_stereo(caps: &gst::Caps) -> bool {
    !caps.is_any()
        && !caps.is_empty()
        && caps
            .iter()
            .all(|structure| matches!(structure.get::<i32>("channels"), Ok(1 | 2)))
}

/// GStreamer errors and warnings that Clapper reports, which otherwise vanish.
/// The bindings skip these signals, so they are connected by name. A broken
/// file or device can repeat a warning on every buffer, hence the cap.
fn log_playback_problems(player: &clapper::Player) {
    const LIMIT: u32 = 50;
    static REPORTED: AtomicU32 = AtomicU32::new(0);
    for signal in ["error", "warning"] {
        player.connect_local(signal, false, move |args| {
            let count = REPORTED.fetch_add(1, Ordering::Relaxed);
            if count < LIMIT {
                let error = args
                    .get(1)
                    .and_then(|value| value.get::<glib::Error>().ok());
                let details = args.get(2).and_then(|value| value.get::<String>().ok());
                tracing::warn!(
                    error = error.as_ref().map(|error| error.message()),
                    details,
                    "playback {signal}"
                );
            } else if count == LIMIT {
                tracing::warn!("further playback errors suppressed this session");
            }
            None
        });
    }
    player.connect_missing_plugin(|_, name, _| {
        tracing::warn!(name, "playback is missing a GStreamer plugin");
    });
}

/// Which decoder playback picked: hardware (d3d11*, va*, nv*) or software.
fn log_element(role: &str, element: Option<gstreamer::Element>) {
    use gstreamer::prelude::*;
    if let Some(factory) = element.and_then(|element| element.factory()) {
        tracing::info!(element = factory.name().as_str(), "{role}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_bytes_grow_only_for_high_bitrates() {
        let minute_at = |mbps: u64| mbps * 1_000_000 / 8 * 60;
        assert_eq!(queue_bytes_for(minute_at(20), 60_000), DEMUX_QUEUE_BYTES);
        // 200 Mbps: 2 s is 50 MB, plus headroom.
        assert_eq!(queue_bytes_for(minute_at(200), 60_000), 75_000_000);
        assert_eq!(queue_bytes_for(minute_at(200), 0), DEMUX_QUEUE_BYTES);
    }
}
