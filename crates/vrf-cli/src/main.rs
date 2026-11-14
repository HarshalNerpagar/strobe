use std::io::{IsTerminal, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use vrf_blink::adapters::hyperframes;
use vrf_blink::adapters::remotion::{self, Options, RenderJob, Renderer, StillJob, VideoDecoder};
use vrf_blink::daemon::{self, Job, Reply, Request, Stream};
use vrf_blink::driver::{self, Plan};
use vrf_blink::{Mode, Ui, calibration};
use vrf_encode::{Codec, ColourSpace, EncodeSettings, Tune};

const COMMAND_START: &str = "VRF_COMMAND_START_US";
const SOFTWARE_WEBGL: &str = "VRF_SOFTWARE_WEBGL";
const DEFAULT_FPS: u32 = 30;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const REMOTION_CPU_RASTER: bool = cfg!(windows);

#[derive(Parser)]
#[command(name = "strobe", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render a Remotion or HyperFrames project to a video file.
    Render(RenderArgs),
    /// Keep the renderer warm between renders. `strobe render` uses a running daemon and falls
    /// back to rendering in its own process when there is none. Rasterising on the GPU and
    /// WebGL on SwiftShader are fixed for a running CEF, so each has a daemon of its own. With
    /// one running, a render that needs another starts it and waits for it.
    Daemon(DaemonArgs),
    /// Write single frames of a Remotion composition as PNG or JPEG images, through the same
    /// page, seek and paint as a render, without encoding a video. Uses a running daemon of the
    /// same mode like `strobe render`.
    Still(StillArgs),
    /// Load a Remotion project in a daemon and leave a ready view of the composition, so the
    /// next render or still of it starts warm. Starts a daemon of the mode the project renders
    /// in if none is running.
    Warm(WarmArgs),
    /// Print this binary's build id as JSON. A daemon serves only clients of its own build.
    BuildId,
}

#[derive(Args, Clone)]
struct RenderArgs {
    /// The project directory: a Remotion project with its dependencies installed, or a
    /// HyperFrames project (an `index.html`).
    project: PathBuf,
    /// The Remotion composition to render. Required for a Remotion project.
    #[arg(long)]
    composition: Option<String>,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long, value_enum, default_value = "h264")]
    codec: CodecArg,
    /// `fast` rasterises on the GPU, as HyperFrames' own `chrome-headless-shell` does, and is the
    /// default: up to 30% quicker on raster-bound pages. For Remotion projects blended fills and
    /// gradients land a level off Remotion's pixels and `shape-rendering: crispEdges` is
    /// anti-aliased, a difference seen only when zoomed in. `reference` rasterises in software
    /// and gives the pixels of Remotion's own `chrome-headless-shell`.
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// The frames to render, as `first-last` (inclusive) or one frame.
    #[arg(long, value_parser = parse_frames)]
    frames: Option<RangeInclusive<u64>>,
    /// Fail on any http(s) request that is not pinned in the project's `vrf.lock.json`.
    #[arg(long)]
    offline: bool,
    /// Remotion: the file that calls `registerRoot`, relative to the project. Found if absent.
    #[arg(long)]
    entry: Option<PathBuf>,
    /// HyperFrames: the installed `hyperframes` package. Found in `node_modules` above the
    /// project if absent.
    #[arg(long)]
    hyperframes: Option<PathBuf>,
    /// HyperFrames: frames per second. 30 if absent.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    fps: Option<u32>,
    /// Remotion: input props as JSON, or the path of a JSON file.
    #[arg(long)]
    props: Option<String>,
    /// Remotion: device pixels per CSS pixel, as Remotion's `--scale`. The composition keeps its
    /// layout size and the video is this many times larger, each side rounded to even: 1.3333
    /// turns 1920x1080 into 2560x1440 and 2 into 3840x2160. 1 if absent.
    #[arg(long, value_parser = parse_scale)]
    scale: Option<f64>,
    /// Remotion: milliseconds before a `delayRender` that is never cleared fails the frame.
    /// 30000 if absent.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: Option<u64>,
    /// Views rendering in parallel. Measured on this machine if absent.
    #[arg(long)]
    views: Option<usize>,
    /// Frames each view renders in a row. Measured on this machine if absent.
    #[arg(long)]
    run: Option<u64>,
    /// Frames a view seeks and paints before its run, and discards.
    #[arg(long, default_value_t = driver::LEAD_IN)]
    lead_in: u64,
    /// Leave the audio out of the file.
    #[arg(long)]
    no_audio: bool,
    /// What decodes embedded video: `native` (VideoToolbox on macOS, Media Foundation on
    /// Windows, falling back to the framework's own decoding for clips it cannot reproduce
    /// exactly) or `remotion` (the framework's own for all: Remotion's frame server,
    /// HyperFrames' ffmpeg extraction).
    #[arg(long, value_enum, default_value = "native")]
    video: VideoArg,
    /// Render every frame, even those the page proves unchanged.
    #[arg(long)]
    no_skip: bool,
    /// Print a JSON report on stdout instead of the summary line.
    #[arg(long)]
    json: bool,
    /// Render in this process even if a daemon is running.
    #[arg(long)]
    no_daemon: bool,
    /// Encode H.264 on VideoToolbox alone, without the libx264 lane.
    #[arg(long)]
    no_turbo: bool,
    /// NVENC sessions that share the encoding, a chunk of frames each in turn (Windows only).
    #[arg(long, default_value_t = 1)]
    encoder_sessions: usize,
    /// Remotion, NVENC: constant quality, 1 (best) to 51. 17 if no rate is given.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=51), conflicts_with = "bitrate")]
    quality: Option<u8>,
    /// Remotion, NVENC: variable bitrate with this average, in Mb/s.
    #[arg(long, value_parser = parse_megabits)]
    bitrate: Option<u32>,
    /// Remotion, NVENC: the peak bitrate in Mb/s, of `--bitrate` (twice it if absent) or of
    /// constant quality (240 for H.264 and none for HEVC and AV1 if absent).
    #[arg(long, value_parser = parse_megabits)]
    max_bitrate: Option<u32>,
    /// Remotion, NVENC: the preset, p1 (fastest, the default) to p7 (best).
    #[arg(long, value_enum)]
    preset: Option<PresetArg>,
    /// Remotion, NVENC: the tuning. `hq` if absent.
    #[arg(long, value_enum)]
    tune: Option<TuneArg>,
    /// Remotion: frames from one keyframe to the next. 240 if absent.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    gop: Option<u32>,
    /// Remotion, NVENC: consecutive B-frames. 0 if absent.
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=4))]
    bframes: Option<u32>,
    /// Remotion, NVENC: encode 10-bit, HEVC Main10 or AV1, from P010. Implies `--color bt709`.
    #[arg(long = "10bit")]
    ten_bit: bool,
    /// Remotion: the YCbCr matrix and range of the video. `remotion`, the default, is BT.601
    /// full range for H.264 and HEVC, which gives Remotion's pixels to a player that honours the
    /// tags, and BT.709 limited for AV1. `bt709` is BT.709 limited range for every codec, what
    /// broadcast and upload pipelines expect.
    #[arg(long, value_enum)]
    color: Option<ColorArg>,
}

#[derive(Args)]
struct DaemonArgs {
    /// Seconds with nothing to do before the daemon exits.
    #[arg(long, default_value_t = 600)]
    idle_timeout: u64,
    /// The rasterisation this daemon renders with. A render in the other mode gets a daemon of
    /// its own. If absent, the mode a Remotion render takes by default on this platform:
    /// `reference` on Windows, `fast` elsewhere.
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// Projects whose bundle, frame server and views stay loaded.
    #[arg(long, default_value_t = 2)]
    projects: usize,
    /// Stop every running daemon after its current renders.
    #[arg(long)]
    stop: bool,
    /// Print every running daemon's build, mode, loaded projects and uptime as JSON, and exit.
    #[arg(long)]
    status: bool,
    /// A Remotion render whose props differ from a loaded page of the same composition gives
    /// the page the new props and mounts the composition again, instead of loading a new page.
    #[arg(long)]
    hot_props: bool,
}

impl DaemonArgs {
    fn cpu_raster(&self) -> bool {
        self.mode
            .map_or(REMOTION_CPU_RASTER, |mode| mode == ModeArg::Reference)
    }
}

/// The flags that choose a Remotion page, shared by `still` and `warm` so that both find the
/// page a render with the same flags loaded.
#[derive(Args, Clone)]
struct PageArgs {
    /// The Remotion project directory, with its dependencies installed.
    project: PathBuf,
    /// The composition.
    #[arg(long)]
    composition: String,
    /// Input props as JSON, or the path of a JSON file.
    #[arg(long)]
    props: Option<String>,
    /// As for `strobe render`. If absent, `reference` on Windows and `fast` elsewhere.
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// The file that calls `registerRoot`, relative to the project. Found if absent.
    #[arg(long)]
    entry: Option<PathBuf>,
    /// Milliseconds before a `delayRender` that is never cleared fails the frame.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: Option<u64>,
    /// Fail on any http(s) request that is not pinned in the project's `vrf.lock.json`.
    #[arg(long)]
    offline: bool,
    /// What decodes embedded video, as for `strobe render`.
    #[arg(long, value_enum, default_value = "native")]
    video: VideoArg,
    /// Load the page without the change tracker, as `strobe render --no-skip` does.
    #[arg(long)]
    no_skip: bool,
    /// Print a JSON report on stdout instead of the summary line.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone)]
struct StillArgs {
    #[command(flatten)]
    page: PageArgs,
    /// The frames to write, as a comma-separated list of frames and `first-last` ranges.
    #[arg(long, value_parser = parse_frame_list)]
    frames: FrameList,
    /// The directory the images go to, as `f<frame>.png` and `f<frame>.jpg`.
    #[arg(short, long)]
    output: PathBuf,
    /// Also write a JPEG of this quality (1-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
    jpeg: Option<u8>,
    /// Write no PNG, only the JPEG.
    #[arg(long, requires = "jpeg")]
    no_png: bool,
    /// The images are this many times the composition's size, at most 4. Above 1 the page
    /// rasterises at that device scale, as `strobe render --scale` does; at most 1 the capture
    /// is resized.
    #[arg(long, default_value_t = 1.0, value_parser = parse_scale)]
    scale: f64,
    /// Capture in this process even if a daemon is running.
    #[arg(long)]
    no_daemon: bool,
}

#[derive(Args, Clone)]
struct WarmArgs {
    #[command(flatten)]
    page: PageArgs,
    /// For a daemon this command starts: seconds with nothing to do before it exits.
    #[arg(long)]
    idle_timeout: Option<u64>,
    /// For a daemon this command starts: projects that stay loaded.
    #[arg(long)]
    projects: Option<usize>,
    /// For a daemon this command starts: see `strobe daemon --hot-props`.
    #[arg(long)]
    hot_props: bool,
    /// The device scale of the page, as for `strobe render --scale`, so a render or still at
    /// that scale starts warm. 1 if absent.
    #[arg(long, value_parser = parse_scale)]
    scale: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
struct FrameList(Vec<u64>);

fn parse_frame_list(text: &str) -> Result<FrameList, String> {
    let mut frames = Vec::new();
    for part in text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let range = parse_frames(part)?;
        if range.end() - range.start() >= 10_000 {
            return Err(format!("{part} holds more than 10000 frames"));
        }
        frames.extend(range);
    }
    if frames.is_empty() {
        return Err("no frames".to_owned());
    }
    Ok(FrameList(frames))
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum ModeArg {
    Fast,
    Reference,
}

#[derive(Clone, Copy, ValueEnum)]
enum PresetArg {
    P1,
    P2,
    P3,
    P4,
    P5,
    P6,
    P7,
}

#[derive(Clone, Copy, ValueEnum)]
enum TuneArg {
    Hq,
    Ll,
    Ull,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum ColorArg {
    Remotion,
    Bt709,
}

#[derive(Clone, Copy, ValueEnum)]
enum VideoArg {
    Native,
    Remotion,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    H264,
    Hevc,
    Prores,
    #[cfg(windows)]
    Av1,
}

impl From<CodecArg> for Codec {
    fn from(codec: CodecArg) -> Codec {
        match codec {
            CodecArg::H264 => Codec::H264,
            CodecArg::Hevc => Codec::Hevc,
            CodecArg::Prores => Codec::ProRes422,
            #[cfg(windows)]
            CodecArg::Av1 => Codec::Av1,
        }
    }
}

fn parse_frames(text: &str) -> Result<RangeInclusive<u64>, String> {
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let number = |part: &str| part.parse::<u64>().map_err(|e| format!("{part:?}: {e}"));
    let (first, last) = (number(first)?, number(last)?);
    if first > last {
        return Err(format!("frame {first} is after frame {last}"));
    }
    Ok(first..=last)
}

fn parse_scale(text: &str) -> Result<f64, String> {
    let scale: f64 = text.parse().map_err(|e| format!("{text:?}: {e}"))?;
    if !(scale.is_finite() && scale > 0.0 && scale <= 4.0) {
        return Err(format!("scale {scale} is not above 0 and at most 4"));
    }
    Ok(scale)
}

fn parse_megabits(text: &str) -> Result<u32, String> {
    let megabits: f64 = text.parse().map_err(|e| format!("{text:?}: {e}"))?;
    if !(megabits.is_finite() && megabits > 0.0 && megabits <= 4000.0) {
        return Err(format!("{megabits} Mb/s is not above 0 and at most 4000"));
    }
    Ok((megabits * 1_000_000.0).round() as u32)
}

fn props(argument: Option<String>) -> Result<Option<String>> {
    match argument {
        Some(text) if !text.trim_start().starts_with('{') => {
            let text = std::fs::read_to_string(&text)
                .with_context(|| format!("read props file {text}"))?;
            Ok(Some(text))
        }
        other => Ok(other),
    }
}

enum Work {
    Remotion(RenderJob),
    Hyperframes(hyperframes::RenderJob),
}

fn is_remotion(project: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(project.join("package.json")) else {
        return false;
    };
    let package: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    ["dependencies", "devDependencies"].iter().any(|kind| {
        package[kind]
            .as_object()
            .is_some_and(|deps| deps.contains_key("remotion"))
    })
}

impl RenderArgs {
    fn is_remotion(&self) -> bool {
        self.composition.is_some() || is_remotion(&self.project)
    }

    /// GPU raster speeds Remotion's paint on macOS but halves it on Windows, where CPU raster
    /// also gives Remotion's exact pixels.
    fn cpu_raster(&self) -> bool {
        match self.mode {
            Some(mode) => mode == ModeArg::Reference,
            None if self.is_remotion() => REMOTION_CPU_RASTER,
            None => hyperframes::CPU_RASTER,
        }
    }

    fn check_framework_flags(&self) -> Result<()> {
        let (framework, given, other) = if self.is_remotion() {
            (
                "Remotion",
                vec![
                    ("--fps", self.fps.is_some()),
                    ("--hyperframes", self.hyperframes.is_some()),
                ],
                "HyperFrames",
            )
        } else {
            (
                "HyperFrames",
                vec![
                    ("--props", self.props.is_some()),
                    ("--timeout", self.timeout.is_some()),
                    ("--scale", self.scale.is_some()),
                    ("--quality", self.quality.is_some()),
                    ("--bitrate", self.bitrate.is_some()),
                    ("--max-bitrate", self.max_bitrate.is_some()),
                    ("--preset", self.preset.is_some()),
                    ("--tune", self.tune.is_some()),
                    ("--gop", self.gop.is_some()),
                    ("--bframes", self.bframes.is_some()),
                    ("--10bit", self.ten_bit),
                    ("--color", self.color.is_some()),
                ],
                "Remotion",
            )
        };
        let wrong: Vec<_> = given
            .into_iter()
            .filter_map(|(flag, given)| given.then_some(flag))
            .collect();
        ensure!(
            wrong.is_empty(),
            "{} only applies to a {other} project, and {} is a {framework} project",
            wrong.join(" and "),
            self.project.display()
        );
        Ok(())
    }

    fn encode(&self) -> Result<EncodeSettings> {
        let codec = Codec::from(self.codec);
        if self.ten_bit {
            ensure!(
                matches!(codec, Codec::Hevc | Codec::Av1),
                "--10bit needs --codec hevc or av1"
            );
            ensure!(
                cfg!(windows),
                "--10bit is NVENC only, and NVENC is Windows only"
            );
            ensure!(
                self.color != Some(ColorArg::Remotion) || codec == Codec::Av1,
                "--10bit converts to BT.709 limited range and cannot keep --color remotion"
            );
        }
        let colour = match self.color {
            Some(ColorArg::Bt709) => Some(ColourSpace::BT709),
            None if self.ten_bit => Some(ColourSpace::BT709),
            Some(ColorArg::Remotion) | None => None,
        };
        Ok(EncodeSettings {
            constant_quality: self.quality,
            bit_rate: self.bitrate,
            max_bit_rate: self.max_bitrate,
            preset: self.preset.map(|preset| preset as u8 + 1),
            tune: self.tune.map(|tune| match tune {
                TuneArg::Hq => Tune::HighQuality,
                TuneArg::Ll => Tune::LowLatency,
                TuneArg::Ull => Tune::UltraLowLatency,
            }),
            gop: self.gop,
            b_frames: self.bframes,
            ten_bit: self.ten_bit,
            colour,
        })
    }

    fn remembers_webgl(&self) -> bool {
        self.composition.as_deref().is_some_and(|composition| {
            remotion::known_webgl(&self.project, self.entry.as_deref(), composition)
        })
    }

    fn work(&self, plan: Plan) -> Result<Work> {
        let project = self.project.canonicalize().with_context(|| {
            format!(
                "project directory {} does not exist",
                self.project.display()
            )
        })?;
        let mode = if self.offline {
            Mode::Offline
        } else {
            Mode::Online
        };
        let plan = Plan {
            turbo: !self.no_turbo,
            sessions: self.encoder_sessions,
            ..plan
        };
        let output = std::path::absolute(&self.output)?;
        Ok(if self.is_remotion() {
            let options = Options {
                project,
                entry: self.entry.clone(),
                composition: self
                    .composition
                    .clone()
                    .context("a Remotion project needs --composition")?,
                props: props(self.props.clone())?,
                mode,
                timeout: Duration::from_millis(self.timeout.unwrap_or(DEFAULT_TIMEOUT_MS)),
                plan,
                audio: !self.no_audio,
                video: match self.video {
                    VideoArg::Native => VideoDecoder::Native,
                    VideoArg::Remotion => VideoDecoder::Remotion,
                },
                skip: !self.no_skip,
                scale: self.scale.unwrap_or(1.0),
            };
            Work::Remotion(RenderJob {
                options,
                frames: self.frames.clone(),
                codec: self.codec.into(),
                output,
                encode: self.encode()?,
            })
        } else {
            let options = hyperframes::Options {
                project,
                hyperframes: self.hyperframes.clone(),
                fps: self.fps.unwrap_or(DEFAULT_FPS),
                mode,
                plan,
                audio: !self.no_audio,
                native_video: matches!(self.video, VideoArg::Native),
                skip: !self.no_skip,
            };
            Work::Hyperframes(hyperframes::RenderJob {
                options,
                frames: self.frames.clone(),
                codec: self.codec.into(),
                output,
            })
        })
    }
}

fn progress() -> impl FnMut(u64, u64) {
    let terminal = std::io::stderr().is_terminal();
    move |frame, frames| {
        if terminal {
            let mut stderr = std::io::stderr();
            let _ = write!(stderr, "\r{frame}/{frames} frames");
            if frame == frames {
                let _ = write!(stderr, "\r\x1b[K");
            }
            let _ = stderr.flush();
        }
    }
}

fn command_start() -> SystemTime {
    if let Some(micros) = std::env::var(COMMAND_START)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
    {
        return UNIX_EPOCH + Duration::from_micros(micros);
    }
    let now = SystemTime::now();
    let micros = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    unsafe { std::env::set_var(COMMAND_START, micros.to_string()) };
    now
}

struct Rendered {
    report: Value,
    frames: u64,
    width: u32,
    height: u32,
    first_frame_ms: f64,
    total_ms: f64,
}

impl From<remotion::Report> for Rendered {
    fn from(report: remotion::Report) -> Rendered {
        Rendered {
            report: serde_json::to_value(&report).expect("a report serialises"),
            frames: report.frames,
            width: report.output_width,
            height: report.output_height,
            first_frame_ms: report.first_frame_ms,
            total_ms: report.total_ms,
        }
    }
}

impl From<hyperframes::Report> for Rendered {
    fn from(report: hyperframes::Report) -> Rendered {
        Rendered {
            report: serde_json::to_value(&report).expect("a report serialises"),
            frames: report.frames,
            width: report.composition.width,
            height: report.composition.height,
            first_frame_ms: report.first_frame_ms,
            total_ms: report.total_ms,
        }
    }
}

fn mode_name(cpu_raster: bool) -> &'static str {
    if cpu_raster { "reference" } else { "fast" }
}

fn start_daemon(cpu_raster: bool, socket: &Path) -> Option<Stream> {
    if !daemon::all_sockets().any(|other| daemon::connect(&other).is_some()) {
        return None;
    }
    let mode = mode_name(cpu_raster);
    eprintln!(
        "strobe: starting a {mode} daemon beside the running one; this render waits for it, later ones start warm"
    );
    spawn_daemon(cpu_raster, socket, &[])
}

fn spawn_daemon(cpu_raster: bool, socket: &Path, extra: &[String]) -> Option<Stream> {
    let mut command = Process::new(std::env::current_exe().ok()?);
    command.args(["daemon", "--mode", mode_name(cpu_raster)]);
    command.args(extra);
    if vrf_blink::software_webgl() {
        command.env(SOFTWARE_WEBGL, "1");
    }
    let mut child = daemon::spawn_detached(&mut command).ok()?;
    let deadline = Instant::now() + DAEMON_START;
    loop {
        if let Some(stream) = daemon::connect(socket) {
            return Some(stream);
        }
        if child.try_wait().ok()?.is_some() || Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How a request was served, for the `daemon` field of a JSON report.
fn served(cpu_raster: bool, by_daemon: Option<&Path>, reason: Option<&str>) -> Value {
    json!({
        "served": by_daemon.is_some(),
        "build": daemon::build_id().unwrap_or_default(),
        "mode": mode_name(cpu_raster),
        "socket": by_daemon,
        "reason": reason,
    })
}

fn render_remote(
    args: &RenderArgs,
    cpu_raster: bool,
    command_start: SystemTime,
    how: &mut Value,
) -> Result<Option<Rendered>> {
    let Some(plan) = calibration::cached_plan(args.views, args.run, args.lead_in)? else {
        *how = served(cpu_raster, None, Some("no calibration is cached yet"));
        return Ok(None);
    };
    let socket = daemon::socket_path();
    let Some(stream) = daemon::connect(&socket).or_else(|| start_daemon(cpu_raster, &socket))
    else {
        *how = served(cpu_raster, None, Some("no daemon of this mode is running"));
        return Ok(None);
    };
    let job = match args.work(plan)? {
        Work::Remotion(job) => Job::Render(job),
        Work::Hyperframes(job) => Job::Hyperframes(job),
    };
    let request = Request::new(cpu_raster, command_start, job)?;
    let mut show = progress();
    let reply = daemon::call(
        stream,
        &request,
        &mut |ahead| eprintln!("strobe: waiting for {ahead} render(s) ahead in the daemon"),
        &mut |frame, frames| show(frame, frames),
    )?;
    *how = served(cpu_raster, Some(socket.as_path()), None);
    Ok(match reply {
        Reply::Rendered(report, _) => Some((*report).into()),
        Reply::RenderedHyperframes(report) => Some((*report).into()),
        Reply::Refused(reason) => {
            eprintln!(
                "strobe: the daemon refused this render ({reason}); rendering in this process"
            );
            *how = served(cpu_raster, None, Some(&format!("refused: {reason}")));
            None
        }
        _ => {
            *how = served(cpu_raster, None, Some("the daemon went away"));
            None
        }
    })
}

fn render_local(ui: &Ui, args: &RenderArgs, command_start: SystemTime) -> Result<Rendered> {
    let plan = calibration::plan(ui, args.views, args.run, args.lead_in)?;
    let work = args.work(plan)?;
    let mut show = progress();
    let mut report = |frame, frames| {
        show(frame, frames);
        Ok(())
    };
    let mut renderer = Renderer::new(1);
    Ok(match work {
        Work::Remotion(job) => renderer
            .render(ui, &job, command_start, &mut report)?
            .into(),
        Work::Hyperframes(job) => renderer
            .render_hyperframes(ui, &job, command_start, &mut report)?
            .into(),
    })
}

fn with_daemon(mut report: Value, how: &Value) -> Value {
    if let Some(object) = report.as_object_mut() {
        object.insert("daemon".to_owned(), how.clone());
    }
    report
}

fn print_report(rendered: &Rendered, args: &RenderArgs, how: &Value) {
    if args.json {
        let report = with_daemon(rendered.report.clone(), how);
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a report serialises")
        );
    } else {
        println!(
            "{}: {} frames of {}x{} in {:.2} s = {:.1} fps, first frame at {:.0} ms",
            args.output.display(),
            rendered.frames,
            rendered.width,
            rendered.height,
            rendered.total_ms / 1000.0,
            rendered.frames as f64 / (rendered.total_ms / 1000.0),
            rendered.first_frame_ms
        );
    }
}

fn stop_daemons() -> Result<()> {
    let request = Request::new(false, SystemTime::now(), Job::Stop)?;
    let mut stopped = 0;
    for socket in daemon::all_sockets() {
        if let Some(stream) = daemon::connect(&socket) {
            daemon::call(stream, &request, &mut |_| {}, &mut |_, _| {})?;
            stopped += 1;
        }
    }
    match stopped {
        0 => println!("no daemon is running"),
        1 => println!("daemon stopped"),
        count => println!("{count} daemons stopped"),
    }
    Ok(())
}

fn needs_software_webgl(error: &anyhow::Error) -> bool {
    !vrf_blink::software_webgl() && format!("{error:#}").contains(remotion::NEEDS_SOFTWARE_WEBGL)
}

fn render_again_with_software_webgl() -> Result<()> {
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env(SOFTWARE_WEBGL, "1")
        .status()?;
    std::process::exit(status.code().unwrap_or(1));
}

fn no_daemon(flag: bool) -> Option<&'static str> {
    if flag {
        Some("--no-daemon")
    } else if std::env::var_os("VRF_NO_DAEMON").is_some() {
        Some("VRF_NO_DAEMON is set")
    } else {
        None
    }
}

fn run_render(args: RenderArgs, cpu_raster: bool, command_start: SystemTime) -> Result<()> {
    args.check_framework_flags()?;
    let mut how = served(cpu_raster, None, no_daemon(args.no_daemon));
    let remote = if no_daemon(args.no_daemon).is_some() {
        None
    } else {
        match render_remote(&args, cpu_raster, command_start, &mut how) {
            Err(error) if needs_software_webgl(&error) => {
                return render_again_with_software_webgl();
            }
            remote => remote?,
        }
    };
    let report = match remote {
        Some(report) => report,
        None => {
            unsafe { std::env::set_var("VRF_NO_DAEMON", "1") };
            let outcome = vrf_blink::run({
                let args = args.clone();
                move |ui| render_local(&ui, &args, command_start)
            });
            match outcome {
                Err(error) if needs_software_webgl(&error) => {
                    return render_again_with_software_webgl();
                }
                outcome => outcome?,
            }
        }
    };
    print_report(&report, &args, &how);
    Ok(())
}

impl PageArgs {
    fn render_args(&self, output: PathBuf) -> RenderArgs {
        RenderArgs {
            project: self.project.clone(),
            composition: Some(self.composition.clone()),
            output,
            codec: CodecArg::H264,
            mode: self.mode,
            frames: None,
            offline: self.offline,
            entry: self.entry.clone(),
            hyperframes: None,
            fps: None,
            props: self.props.clone(),
            timeout: self.timeout,
            views: None,
            run: None,
            lead_in: driver::LEAD_IN,
            no_audio: false,
            video: self.video,
            no_skip: self.no_skip,
            json: self.json,
            no_daemon: false,
            no_turbo: false,
            encoder_sessions: 1,
            scale: None,
            quality: None,
            bitrate: None,
            max_bitrate: None,
            preset: None,
            tune: None,
            gop: None,
            bframes: None,
            ten_bit: false,
            color: None,
        }
    }

    fn options(&self) -> Result<Options> {
        match self.render_args(PathBuf::from("-")).work(Plan::single())? {
            Work::Remotion(job) => Ok(job.options),
            Work::Hyperframes(_) => unreachable!("a composition makes it a Remotion project"),
        }
    }

    fn cpu_raster(&self) -> bool {
        self.render_args(PathBuf::from("-")).cpu_raster()
    }

    fn set_modes(&self) -> bool {
        let cpu_raster = self.cpu_raster();
        vrf_blink::set_cpu_raster(cpu_raster);
        vrf_blink::set_software_webgl(
            std::env::var_os(SOFTWARE_WEBGL).is_some()
                || self.render_args(PathBuf::from("-")).remembers_webgl(),
        );
        cpu_raster
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("a report serialises")
    );
}

fn still_job(args: &StillArgs) -> Result<StillJob> {
    let (device, resize) = if args.scale > 1.0 {
        (args.scale, 1.0)
    } else {
        (1.0, args.scale)
    };
    let mut options = args.page.options()?;
    options.scale = device;
    Ok(StillJob {
        options,
        frames: args.frames.0.clone(),
        output: std::path::absolute(&args.output)?,
        png: !args.no_png,
        jpeg: args.jpeg,
        scale: resize,
    })
}

fn still_remote(
    job: &StillJob,
    cpu_raster: bool,
    command_start: SystemTime,
    how: &mut Value,
) -> Result<Option<remotion::StillReport>> {
    let socket = daemon::socket_path();
    let Some(stream) = daemon::connect(&socket).or_else(|| start_daemon(cpu_raster, &socket))
    else {
        *how = served(cpu_raster, None, Some("no daemon of this mode is running"));
        return Ok(None);
    };
    let request = Request::new(cpu_raster, command_start, Job::Still(job.clone()))?;
    let reply = daemon::call(
        stream,
        &request,
        &mut |ahead| eprintln!("strobe: waiting for {ahead} render(s) ahead in the daemon"),
        &mut |_, _| {},
    )?;
    Ok(match reply {
        Reply::Still(report) => {
            *how = served(cpu_raster, Some(socket.as_path()), None);
            Some(*report)
        }
        Reply::Refused(reason) => {
            eprintln!(
                "strobe: the daemon refused these stills ({reason}); capturing in this process"
            );
            *how = served(cpu_raster, None, Some(&format!("refused: {reason}")));
            None
        }
        _ => {
            *how = served(cpu_raster, None, Some("the daemon went away"));
            None
        }
    })
}

fn run_still(args: StillArgs, cpu_raster: bool, command_start: SystemTime) -> Result<()> {
    let job = still_job(&args)?;
    let mut how = served(cpu_raster, None, no_daemon(args.no_daemon));
    let remote = if no_daemon(args.no_daemon).is_some() {
        None
    } else {
        match still_remote(&job, cpu_raster, command_start, &mut how) {
            Err(error) if needs_software_webgl(&error) => {
                return render_again_with_software_webgl();
            }
            remote => remote?,
        }
    };
    let report = match remote {
        Some(report) => report,
        None => {
            unsafe { std::env::set_var("VRF_NO_DAEMON", "1") };
            let outcome = vrf_blink::run({
                let job = job.clone();
                move |ui| Renderer::new(1).still(&ui, &job, command_start, &mut |_, _| Ok(()))
            });
            match outcome {
                Err(error) if needs_software_webgl(&error) => {
                    return render_again_with_software_webgl();
                }
                outcome => outcome?,
            }
        }
    };
    if args.page.json {
        print_json(&with_daemon(serde_json::to_value(&report)?, &how));
    } else {
        for still in &report.stills {
            let path = still.png.as_ref().or(still.jpeg.as_ref());
            println!(
                "{}: frame {} {}x{}",
                path.map(|path| path.display().to_string())
                    .unwrap_or_default(),
                still.frame,
                still.width,
                still.height
            );
        }
        println!(
            "{} stills in {:.2} s",
            report.stills.len(),
            report.total_ms / 1000.0
        );
    }
    Ok(())
}

const DAEMON_START: Duration = Duration::from_secs(120);

fn run_warm(args: WarmArgs, cpu_raster: bool, command_start: SystemTime) -> Result<()> {
    let mut options = args.page.options()?;
    options.scale = args.scale.unwrap_or(1.0);
    let socket = daemon::socket_path();
    let mut started = false;
    let stream = match daemon::connect(&socket) {
        Some(stream) => stream,
        None => {
            let mut extra = Vec::new();
            if let Some(idle) = args.idle_timeout {
                extra.extend(["--idle-timeout".to_owned(), idle.to_string()]);
            }
            if let Some(projects) = args.projects {
                extra.extend(["--projects".to_owned(), projects.to_string()]);
            }
            if args.hot_props {
                extra.push("--hot-props".to_owned());
            }
            started = true;
            spawn_daemon(cpu_raster, &socket, &extra).with_context(|| {
                format!(
                    "no {} daemon answered on {} within {} s",
                    mode_name(cpu_raster),
                    socket.display(),
                    DAEMON_START.as_secs()
                )
            })?
        }
    };
    let request = Request::new(cpu_raster, command_start, Job::Warm(options))?;
    let reply = daemon::call(
        stream,
        &request,
        &mut |ahead| eprintln!("strobe: waiting for {ahead} render(s) ahead in the daemon"),
        &mut |_, _| {},
    )?;
    let report = match reply {
        Reply::Warmed(report) => report,
        Reply::Refused(reason) => {
            anyhow::bail!("the daemon on {} refused: {reason}", socket.display())
        }
        _ => anyhow::bail!("the daemon on {} went away", socket.display()),
    };
    let mut how = served(cpu_raster, Some(socket.as_path()), None);
    how["started"] = Value::Bool(started);
    if args.page.json {
        print_json(&with_daemon(serde_json::to_value(&report)?, &how));
    } else {
        println!(
            "{} warm in a {} daemon ({}x{}, {} frames) after {:.2} s",
            args.page.composition,
            mode_name(cpu_raster),
            report.composition.width,
            report.composition.height,
            report.composition.duration_in_frames,
            report.total_ms / 1000.0
        );
    }
    Ok(())
}

fn daemon_status() -> Result<()> {
    let request = Request::new(false, SystemTime::now(), Job::Status)?;
    let mut daemons = Vec::new();
    for socket in daemon::all_sockets() {
        let Some(stream) = daemon::connect(&socket) else {
            continue;
        };
        daemons.push(
            match daemon::call(stream, &request, &mut |_| {}, &mut |_, _| {}) {
                Ok(Reply::Status(status)) => serde_json::to_value(&*status)?,
                Ok(_) => json!({ "socket": socket, "error": "no status in the reply" }),
                Err(error) => json!({ "socket": socket, "error": format!("{error:#}") }),
            },
        );
    }
    print_json(&json!({ "client_build": daemon::build_id()?, "daemons": daemons }));
    Ok(())
}

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg.starts_with("--type=")) {
        vrf_blink::run(|_| ());
        return ExitCode::FAILURE;
    }
    let command_start = command_start();
    let outcome = match Cli::parse().command {
        Command::Render(args) => {
            let cpu_raster = args.cpu_raster();
            vrf_blink::set_cpu_raster(cpu_raster);
            vrf_blink::set_software_webgl(
                std::env::var_os(SOFTWARE_WEBGL).is_some() || args.remembers_webgl(),
            );
            run_render(args, cpu_raster, command_start)
        }
        Command::Daemon(args) if args.stop => stop_daemons(),
        Command::Daemon(args) if args.status => daemon_status(),
        Command::Daemon(args) => {
            vrf_blink::set_software_webgl(std::env::var_os(SOFTWARE_WEBGL).is_some());
            let cpu_raster = args.cpu_raster();
            vrf_blink::set_cpu_raster(cpu_raster);
            let config = daemon::Config {
                socket: daemon::socket_path(),
                idle: Duration::from_secs(args.idle_timeout),
                cpu_raster,
                projects: args.projects,
                hot_props: args.hot_props,
            };
            vrf_blink::run(move |ui| daemon::serve(&ui, &config))
        }
        Command::Still(args) => {
            let cpu_raster = args.page.set_modes();
            run_still(args, cpu_raster, command_start)
        }
        Command::Warm(args) => {
            let cpu_raster = args.page.set_modes();
            run_warm(args, cpu_raster, command_start)
        }
        Command::BuildId => daemon::build_id().map(|build| {
            print_json(&json!({
                "build": build,
                "version": env!("CARGO_PKG_VERSION"),
                "exe": std::env::current_exe().ok(),
            }))
        }),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_args(project: &Path, extra: &[&str]) -> RenderArgs {
        let mut argv = vec![
            "strobe",
            "render",
            project.to_str().unwrap(),
            "-o",
            "out.mp4",
        ];
        argv.extend(extra);
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Render(args) => args,
            _ => unreachable!(),
        }
    }

    fn project(name: &str, package: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vrf-cli-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(package) = package {
            std::fs::write(dir.join("package.json"), package).unwrap();
        }
        dir
    }

    #[test]
    fn a_hyperframes_project_rasterises_on_the_gpu_and_a_remotion_project_on_the_gpu_only_off_windows()
     {
        let hyperframes = project("hyperframes", None);
        let remotion = project("remotion", Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#));
        assert!(!render_args(&hyperframes, &[]).cpu_raster());
        assert_eq!(render_args(&remotion, &[]).cpu_raster(), cfg!(windows));
    }

    #[test]
    fn frames_are_one_frame_or_an_ascending_range() {
        assert_eq!(parse_frames("5"), Ok(5..=5));
        assert_eq!(parse_frames("0-89"), Ok(0..=89));
        assert_eq!(parse_frames("7-7"), Ok(7..=7));
        assert!(parse_frames("5-3").is_err());
        assert!(parse_frames("").is_err());
        assert!(parse_frames("-5").is_err());
        assert!(parse_frames("a-b").is_err());
    }

    #[test]
    fn a_flag_of_the_other_framework_is_an_error() {
        let hyperframes = project("hyperframes-flags", None);
        let remotion = project(
            "remotion-flags",
            Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#),
        );
        let check = |project: &Path, extra: &[&str]| {
            render_args(project, extra)
                .check_framework_flags()
                .map_err(|error| error.to_string())
        };
        assert!(check(&hyperframes, &["--fps", "24", "--hyperframes", "/x"]).is_ok());
        assert!(check(&remotion, &["--props", "{}", "--timeout", "5"]).is_ok());
        assert!(check(&hyperframes, &[]).is_ok());
        assert!(check(&remotion, &[]).is_ok());
        for flags in [&["--props", "{}"][..], &["--timeout", "5"]] {
            let message = check(&hyperframes, flags).unwrap_err();
            assert!(message.contains(flags[0]), "{message}");
            assert!(message.contains("is a HyperFrames project"), "{message}");
        }
        for flags in [&["--fps", "24"][..], &["--hyperframes", "/x"]] {
            let message = check(&remotion, flags).unwrap_err();
            assert!(message.contains(flags[0]), "{message}");
            assert!(message.contains("is a Remotion project"), "{message}");
        }
        let both = check(&hyperframes, &["--props", "{}", "--timeout", "5"]).unwrap_err();
        assert!(both.contains("--props and --timeout"), "{both}");
    }

    #[test]
    fn a_scale_is_above_zero_and_at_most_four_and_for_remotion_only() {
        for scale in ["0", "-1", "4.5", "nan", "inf", "x"] {
            let argv = ["strobe", "render", "p", "-o", "o.mp4", "--scale", scale];
            assert!(Cli::try_parse_from(argv).is_err(), "--scale {scale}");
        }
        let remotion = project(
            "remotion-scale",
            Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#),
        );
        let args = render_args(&remotion, &["--scale", "1.3333"]);
        assert_eq!(args.scale, Some(1.3333));
        assert!(args.check_framework_flags().is_ok());
        let hyperframes = project("hyperframes-scale", None);
        let message = render_args(&hyperframes, &["--scale", "2"])
            .check_framework_flags()
            .unwrap_err()
            .to_string();
        assert!(message.contains("--scale"), "{message}");
    }

    #[test]
    fn encode_flags_map_to_settings_and_default_to_upstream() {
        let remotion = project(
            "remotion-encode",
            Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#),
        );
        assert_eq!(
            render_args(&remotion, &[]).encode().unwrap(),
            EncodeSettings::default()
        );
        let master = render_args(
            &remotion,
            &[
                "--codec",
                "hevc",
                "--quality",
                "19",
                "--max-bitrate",
                "80",
                "--preset",
                "p7",
                "--tune",
                "hq",
                "--gop",
                "60",
                "--bframes",
                "3",
            ],
        );
        assert!(master.check_framework_flags().is_ok());
        assert_eq!(
            master.encode().unwrap(),
            EncodeSettings {
                constant_quality: Some(19),
                max_bit_rate: Some(80_000_000),
                preset: Some(7),
                tune: Some(Tune::HighQuality),
                gop: Some(60),
                b_frames: Some(3),
                ..Default::default()
            }
        );
        let vbr = render_args(&remotion, &["--bitrate", "12.5", "--color", "bt709"]);
        let settings = vbr.encode().unwrap();
        assert_eq!(settings.bit_rate, Some(12_500_000));
        assert_eq!(settings.colour, Some(ColourSpace::BT709));
        let argv = ["strobe", "render", "p", "-o", "o.mp4", "--quality", "18"];
        assert!(Cli::try_parse_from([&argv[..], &["--bitrate", "20"][..]].concat()).is_err());
        assert!(render_args(&remotion, &["--10bit"]).encode().is_err());
        let ten_bit = render_args(&remotion, &["--codec", "hevc", "--10bit"]).encode();
        if cfg!(windows) {
            let ten_bit = ten_bit.unwrap();
            assert!(ten_bit.ten_bit);
            assert_eq!(ten_bit.colour, Some(ColourSpace::BT709));
            let kept = ["--codec", "hevc", "--10bit", "--color", "remotion"];
            assert!(render_args(&remotion, &kept).encode().is_err());
        } else {
            assert!(ten_bit.is_err());
        }
        let hyperframes = project("hyperframes-encode", None);
        let message = render_args(&hyperframes, &["--gop", "60", "--10bit"])
            .check_framework_flags()
            .unwrap_err()
            .to_string();
        assert!(message.contains("--gop and --10bit"), "{message}");
    }

    #[test]
    fn a_zero_fps_or_timeout_is_refused() {
        for flag in ["--fps", "--timeout"] {
            let argv = ["strobe", "render", "p", "-o", "o.mp4", flag, "0"];
            assert!(Cli::try_parse_from(argv).is_err(), "{flag} 0");
        }
    }

    #[test]
    fn a_frame_list_is_frames_and_ranges() {
        assert_eq!(
            parse_frame_list("120,450,900"),
            Ok(FrameList(vec![120, 450, 900]))
        );
        assert_eq!(parse_frame_list("5-7, 9"), Ok(FrameList(vec![5, 6, 7, 9])));
        assert!(parse_frame_list("").is_err());
        assert!(parse_frame_list(",").is_err());
        assert!(parse_frame_list("9-3").is_err());
        assert!(parse_frame_list("0-20000").is_err());
    }

    fn daemon_args(extra: &[&str]) -> DaemonArgs {
        let argv = [&["strobe", "daemon"][..], extra].concat();
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Daemon(args) => args,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_daemon_without_a_mode_serves_a_remotion_render_without_one() {
        let remotion = project(
            "remotion-daemon",
            Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#),
        );
        assert_eq!(
            daemon_args(&[]).cpu_raster(),
            render_args(&remotion, &[]).cpu_raster()
        );
        assert!(!daemon_args(&["--mode", "fast"]).cpu_raster());
        assert!(daemon_args(&["--mode", "reference"]).cpu_raster());
        assert!(daemon_args(&["--status"]).status);
        assert!(daemon_args(&["--hot-props"]).hot_props);
    }

    fn still_args(project: &Path, extra: &[&str]) -> Result<StillArgs, clap::Error> {
        let argv = [
            &[
                "strobe",
                "still",
                project.to_str().unwrap(),
                "--composition",
                "C",
                "-o",
                "stills",
            ][..],
            extra,
        ]
        .concat();
        Cli::try_parse_from(argv).map(|cli| match cli.command {
            Command::Still(args) => args,
            _ => unreachable!(),
        })
    }

    #[test]
    fn a_still_chooses_its_page_and_mode_as_a_render_does() {
        let remotion = project(
            "remotion-still",
            Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#),
        );
        let args = still_args(&remotion, &["--frames", "1,3-4"]).unwrap();
        assert_eq!(args.frames, FrameList(vec![1, 3, 4]));
        assert_eq!(
            args.page.cpu_raster(),
            render_args(&remotion, &[]).cpu_raster()
        );
        let fast = still_args(&remotion, &["--frames", "1", "--mode", "fast"]).unwrap();
        assert!(!fast.page.cpu_raster());
        let render = render_args(&remotion, &["--composition", "C", "--props", "{\"a\":1}"]);
        let still = still_args(&remotion, &["--frames", "1", "--props", "{\"a\":1}"]).unwrap();
        let options = |work: Work| match work {
            Work::Remotion(job) => job.options,
            Work::Hyperframes(_) => unreachable!(),
        };
        assert_eq!(
            still.page.options().unwrap(),
            options(render.work(Plan::single()).unwrap())
        );
        let job = |scale: &str| {
            still_job(&still_args(&remotion, &["--frames", "1", "--scale", scale]).unwrap())
                .unwrap()
        };
        let larger = job("2");
        assert_eq!((larger.options.scale, larger.scale), (2.0, 1.0));
        let smaller = job("0.5");
        assert_eq!((smaller.options.scale, smaller.scale), (1.0, 0.5));
        assert!(still_args(&remotion, &["--frames", "1", "--scale", "5"]).is_err());
        assert!(still_args(&remotion, &["--frames", "1", "--no-png"]).is_err());
        assert!(still_args(&remotion, &["--frames", "1", "--jpeg", "0"]).is_err());
        assert!(still_args(&remotion, &[]).is_err());
    }

    #[test]
    fn mode_overrides_the_framework_default() {
        let hyperframes = project("hyperframes-mode", None);
        assert!(render_args(&hyperframes, &["--mode", "reference"]).cpu_raster());
        let remotion = project(
            "remotion-mode",
            Some(r#"{"dependencies":{"remotion":"4.0.0"}}"#),
        );
        assert!(!render_args(&remotion, &["--mode", "fast"]).cpu_raster());
    }
}
