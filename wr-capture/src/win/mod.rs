// SPDX-License-Identifier: GPL-3.0-or-later

//! Thread layout: the video thread captures and encodes at a fixed frame
//! rate, the audio thread mixes and encodes, a reader thread turns stdin lines
//! into requests, and the main thread owns the router and writes replies.

mod audio;
mod capture;
mod convert;
mod encoder;
mod stats;

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use crate::Options;
use crate::packet::Packet;
use crate::router::Router;

pub enum Message {
    Packet(Packet),
    Request(Value),
    /// stdin closed: the app wants us gone.
    Quit,
    /// A capture or encode thread failed for good.
    Fatal(String),
}

/// State the main thread shares with the capture threads.
pub struct Shared {
    /// The router wants the next video frame to be a keyframe.
    pub keyframe: AtomicBool,
    /// Process id of the captured window, 0 while there is none; the audio
    /// thread follows it for app-only audio.
    pub target_pid: AtomicU32,
    pub frames: stats::FrameStats,
}

/// 100 ns ticks since the first call, from the QPC clock that both Windows
/// Graphics Capture and WASAPI timestamps use.
pub fn now_100ns() -> i64 {
    qpc_100ns() - *START.get_or_init(qpc_100ns)
}

static START: OnceLock<i64> = OnceLock::new();

/// QPC converted to 100 ns units: the scale of WASAPI's QPC positions.
pub fn qpc_100ns() -> i64 {
    let mut counter = 0;
    let mut frequency = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (i128::from(counter) * 10_000_000 / i128::from(frequency.max(1))) as i64
}

/// The clock's origin in absolute `qpc_100ns` units.
pub fn clock_origin() -> i64 {
    now_100ns();
    *START.get().expect("initialized above")
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

pub fn run(options: Options) -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            eprintln!("Media Foundation is unavailable: {error}");
            return 1;
        }
    }
    for dir in [&options.replay_dir, &options.regular_dir] {
        if let Err(error) = std::fs::create_dir_all(dir) {
            eprintln!("cannot create {}: {error}", dir.display());
            return 1;
        }
    }
    now_100ns();
    let shared = Arc::new(Shared {
        keyframe: AtomicBool::new(false),
        target_pid: AtomicU32::new(0),
        frames: stats::FrameStats::default(),
    });
    let (sender, receiver) = mpsc::channel();

    // Video setup errors (no encoder, no capture API) end the process before
    // the app considers it armed.
    let (ready_sender, ready) = mpsc::channel();
    spawn("video", sender.clone(), {
        let options = options.clone();
        let shared = shared.clone();
        move |sender| capture::run(&options, &shared, sender, ready_sender)
    });
    let size = match ready.recv() {
        Ok(Ok(size)) => size,
        Ok(Err(message)) => {
            eprintln!("{message}");
            return 1;
        }
        Err(_) => return 1,
    };
    if !options.audio.is_empty() {
        let options = options.clone();
        let shared = shared.clone();
        spawn("audio", sender.clone(), move |sender| {
            audio::run(&options, &shared, sender)
        });
    }
    std::thread::spawn({
        let sender = sender.clone();
        move || read_requests(sender)
    });
    std::thread::spawn({
        let shared = shared.clone();
        move || stats::run(shared)
    });
    drop(sender);

    let mut router = Router::new(
        options.codec,
        size,
        !options.audio.is_empty(),
        options.replay_seconds,
        options.replay_dir.clone(),
        options.regular_dir.clone(),
    );
    let mut stdout = std::io::stdout().lock();
    let code = loop {
        let replies = match receiver.recv() {
            Ok(Message::Packet(packet)) => router.push(packet, unix_ms()),
            Ok(Message::Request(request)) => router.request(&request, unix_ms()),
            Ok(Message::Quit) | Err(_) => break 0,
            Ok(Message::Fatal(message)) => {
                eprintln!("{message}");
                break 3;
            }
        };
        if std::mem::take(&mut router.want_keyframe) {
            shared.keyframe.store(true, Ordering::Relaxed);
        }
        for reply in replies {
            let _ = writeln!(stdout, "{reply}");
            let _ = stdout.flush();
        }
    };
    router.finish();
    code
}

/// Run a capture thread; its error becomes a fatal message.
fn spawn(
    name: &str,
    sender: Sender<Message>,
    body: impl FnOnce(Sender<Message>) -> Result<(), String> + Send + 'static,
) {
    let name = name.to_string();
    std::thread::spawn(move || {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        if let Err(message) = body(sender.clone()) {
            let _ = sender.send(Message::Fatal(format!("{name}: {message}")));
        }
    });
}

fn read_requests(sender: Sender<Message>) {
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<Value>(&line) {
            Ok(request) => {
                if sender.send(Message::Request(request)).is_err() {
                    return;
                }
            }
            Err(error) => eprintln!("ignored a malformed request: {error}"),
        }
    }
    let _ = sender.send(Message::Quit);
}

/// Turn a Windows error into "what: error".
pub fn fail(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |error| format!("{what}: {error}")
}
