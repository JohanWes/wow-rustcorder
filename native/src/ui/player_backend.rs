// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::OnceLock;

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
/// than a few frames queued.
const DEMUX_QUEUE_BYTES: u32 = 8 * 1024 * 1024;

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

/// The concrete Clapper objects used by Warcraft Recorder's player UI.
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
        Ok(Self { video, player })
    }

    pub fn widget(&self) -> &clapper_gtk::Video {
        &self.video
    }

    pub fn open_uri(&self, uri: &str) -> Result<(), &'static str> {
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
        const NAME: &'static str = "WarcraftRecorderQueueCap";
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
        element.set_property("max-size-bytes", DEMUX_QUEUE_BYTES);
    }
}

/// Which decoder playback picked: hardware (d3d11*, va*, nv*) or software.
fn log_element(role: &str, element: Option<gstreamer::Element>) {
    use gstreamer::prelude::*;
    if let Some(factory) = element.and_then(|element| element.factory()) {
        tracing::info!(element = factory.name().as_str(), "{role}");
    }
}
