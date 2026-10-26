// SPDX-License-Identifier: GPL-3.0-or-later

//! `wr-capture`: the Windows stand-in for gpu-screen-recorder's replay mode.
//!
//! It captures a window (by executable names) or the primary monitor with
//! Windows Graphics Capture, encodes with the GPU's Media Foundation encoder,
//! mixes WASAPI audio into AAC, and keeps the last seconds in RAM. Requests
//! arrive as GSR `-ipc` JSON lines on stdin and replies leave on stdout; logs
//! go to stderr. Closing stdin finishes any open file and exits.

// Only the Windows build uses the capture core; Linux runs its unit tests.
#![cfg_attr(not(windows), allow(dead_code))]

mod mkv;
mod packet;
mod router;
mod ts;
#[cfg(windows)]
mod win;

use std::path::PathBuf;

use packet::VideoCodec;

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// The first visible top-level window of any of these executables, e.g.
    /// `Wow.exe` and `WowClassic.exe`.
    Exe(Vec<String>),
    PrimaryMonitor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// Audio of the captured window's process tree only.
    App,
    /// Everything on the default output device.
    DefaultOutput,
    DefaultInput,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub target: Target,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub replay_seconds: u32,
    pub replay_dir: PathBuf,
    pub regular_dir: PathBuf,
    pub audio: Vec<AudioSource>,
    pub cursor: bool,
    /// Output size; defaults to the primary monitor.
    pub size: Option<(u32, u32)>,
}

const USAGE: &str = "usage: wr-capture -o <replay dir> -ro <regular dir> [-w Wow.exe|WowClassic.exe...|monitor] \
[-f fps] [-q kbps] [-k h264|hevc|av1] [-r seconds] [-a app|default_output|default_input ...] \
[-cursor yes|no] [-s WxH] | --version | --list-audio-devices";

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        target: Target::Exe(vec!["Wow.exe".into()]),
        fps: 60,
        bitrate_kbps: 20_000,
        codec: VideoCodec::H264,
        replay_seconds: 60,
        replay_dir: PathBuf::new(),
        regular_dir: PathBuf::new(),
        audio: Vec::new(),
        cursor: true,
        size: None,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<u32>()
                .map_err(|_| format!("{flag}: not a number: {value}"))
        };
        match flag.as_str() {
            "-w" if value == "monitor" => options.target = Target::PrimaryMonitor,
            "-w" => options.target = Target::Exe(value.split('|').map(String::from).collect()),
            "-f" => options.fps = number()?.clamp(1, 240),
            "-q" => options.bitrate_kbps = number()?.max(500),
            "-k" => {
                options.codec = match value.as_str() {
                    "h264" => VideoCodec::H264,
                    "hevc" => VideoCodec::Hevc,
                    "av1" => VideoCodec::Av1,
                    _ => return Err(format!("-k: unsupported codec {value}")),
                }
            }
            "-r" => options.replay_seconds = number()?.clamp(1, 1_200),
            "-o" => options.replay_dir = value.into(),
            "-ro" => options.regular_dir = value.into(),
            "-a" => {
                for source in value.split('|') {
                    options.audio.push(match source {
                        "app" => AudioSource::App,
                        "default_output" => AudioSource::DefaultOutput,
                        "default_input" => AudioSource::DefaultInput,
                        _ => return Err(format!("-a: unknown source {source}")),
                    });
                }
            }
            "-cursor" => options.cursor = value == "yes",
            "-s" => {
                let (width, height) = value.split_once('x').ok_or("-s: expected WxH")?;
                let width = width.parse::<u32>().map_err(|_| "-s: bad width")?;
                let height = height.parse::<u32>().map_err(|_| "-s: bad height")?;
                options.size = Some((width.max(64) & !1, height.max(64) & !1));
            }
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    if options.replay_dir.as_os_str().is_empty() || options.regular_dir.as_os_str().is_empty() {
        return Err("-o and -ro are required".into());
    }
    Ok(options)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--list-audio-devices") => {
            println!("default_output|Desktop audio");
            println!("app|Game audio only");
            println!("default_input|Default microphone");
            return;
        }
        _ => {}
    }
    let options = match parse(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            std::process::exit(2);
        }
    };
    #[cfg(windows)]
    std::process::exit(win::run(options));
    #[cfg(not(windows))]
    {
        let _ = options;
        eprintln!("wr-capture only runs on Windows");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    #[test]
    fn parses_gsr_style_flags() {
        let options = parse(&args("-w monitor -f 30 -q 8000 -k hevc -r 90 -o R -ro V -a app|default_input -cursor no -s 1921x1080")).unwrap();
        assert_eq!(options.target, Target::PrimaryMonitor);
        assert_eq!(
            (options.fps, options.bitrate_kbps, options.replay_seconds),
            (30, 8000, 90)
        );
        assert_eq!(options.codec, VideoCodec::Hevc);
        assert_eq!(options.audio, [AudioSource::App, AudioSource::DefaultInput]);
        assert!(!options.cursor);
        assert_eq!(options.size, Some((1920, 1080)));
        assert_eq!(
            parse(&args("-w Wow.exe|WowClassic.exe -o R -ro V"))
                .unwrap()
                .target,
            Target::Exe(vec!["Wow.exe".into(), "WowClassic.exe".into()])
        );
        assert!(parse(&args("-o R")).is_err());
        assert!(parse(&args("-o R -ro V -k vp9")).is_err());
    }
}
