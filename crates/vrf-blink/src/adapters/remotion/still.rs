use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, ensure};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::imageops::{self, FilterType as Resize};
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use serde::{Deserialize, Serialize};
use vrf::colour::Rect;

use super::{Composition, Options, Page, RETRY_MARK, Ready, Renderer, Taken, Warmth};
use crate::Ui;
use crate::adapters::since;
use crate::driver::{Capture, Warm};

const WATCHDOG: Duration = Duration::from_millis(500);

fn yes() -> bool {
    true
}

fn one() -> f64 {
    1.0
}

/// Single frames of a composition written as images: the same page, seek, begin frame and
/// paint as a render, read back from the GPU before any colour conversion or encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StillJob {
    pub options: Options,
    /// Composition frames, in any order. Each is written once.
    pub frames: Vec<u64>,
    /// The directory the images go to. Created if missing.
    pub output: PathBuf,
    #[serde(default = "yes")]
    pub png: bool,
    /// JPEG quality, 1 to 100, for a JPEG beside or instead of the PNG.
    #[serde(default)]
    pub jpeg: Option<u8>,
    /// A factor the images are resized by after the capture, at most 1.
    #[serde(default = "one")]
    pub scale: f64,
}

/// One written frame.
#[derive(Debug, Serialize, Deserialize)]
pub struct Still {
    pub frame: u64,
    pub png: Option<PathBuf>,
    pub jpeg: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    /// Seek to the frame's pixels read back, in milliseconds.
    pub capture_ms: f64,
    /// The page showed no change after the seek, so this is the frame before it again.
    pub repeated: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StillStages {
    pub bundle_s: f64,
    pub bundle_cached: bool,
    pub frame_server_s: f64,
    pub props_s: f64,
    pub open_s: f64,
    pub capture_s: f64,
    pub write_s: f64,
    pub warm: Warmth,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StillReport {
    pub composition: Composition,
    pub stills: Vec<Still>,
    /// Command start to the view being ready to seek.
    pub setup_ms: f64,
    /// Command start to the last image written.
    pub total_ms: f64,
    pub stages: StillStages,
    pub capture_retries: u64,
    pub seek_retries: u64,
}

struct Captured {
    frame: u64,
    pixels: Arc<RgbImage>,
    capture_ms: f64,
    repeated: bool,
}

/// The visible part of a read-back texture of `width` texels a row, as RGB.
pub(crate) fn visible_rgb(
    bytes: &[u8],
    width: u32,
    format: wgpu::TextureFormat,
    visible: Rect,
) -> Result<RgbImage> {
    let bgra = match format {
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => true,
        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => false,
        other => return Err(anyhow!("cannot read a {other:?} paint")),
    };
    let stride = width as usize * 4;
    ensure!(
        visible.x + visible.width <= width
            && (visible.y + visible.height) as usize * stride <= bytes.len(),
        "the visible rect {visible:?} is outside the {width}-wide paint"
    );
    let mut rgb = Vec::with_capacity(visible.width as usize * visible.height as usize * 3);
    for y in visible.y..visible.y + visible.height {
        let start = y as usize * stride + visible.x as usize * 4;
        for pixel in bytes[start..start + visible.width as usize * 4].chunks_exact(4) {
            if bgra {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            } else {
                rgb.extend_from_slice(&pixel[..3]);
            }
        }
    }
    RgbImage::from_raw(visible.width, visible.height, rgb).context("the paint has no pixels")
}

/// The frames to capture: each once, in increasing order, as a render seeks them.
pub(crate) fn ordered(frames: &[u64]) -> Vec<u64> {
    let mut frames = frames.to_vec();
    frames.sort_unstable();
    frames.dedup();
    frames
}

fn scaled(pixels: &RgbImage, scale: f64) -> RgbImage {
    let size = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    imageops::resize(
        pixels,
        size(pixels.width()),
        size(pixels.height()),
        Resize::Triangle,
    )
}

fn write(job: &StillJob, captured: &Captured) -> Result<Still> {
    let resized;
    let pixels = if job.scale == 1.0 {
        captured.pixels.as_ref()
    } else {
        resized = scaled(&captured.pixels, job.scale);
        &resized
    };
    let name = |extension: &str| {
        job.output
            .join(format!("f{:06}.{extension}", captured.frame))
    };
    let mut still = Still {
        frame: captured.frame,
        png: None,
        jpeg: None,
        width: pixels.width(),
        height: pixels.height(),
        capture_ms: captured.capture_ms,
        repeated: captured.repeated,
    };
    if job.png {
        let path = name("png");
        let file = std::io::BufWriter::new(fs::File::create(&path)?);
        PngEncoder::new_with_quality(file, CompressionType::Fast, FilterType::Adaptive)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ExtendedColorType::Rgb8,
            )
            .with_context(|| format!("write {}", path.display()))?;
        still.png = Some(path);
    }
    if let Some(quality) = job.jpeg {
        let path = name("jpg");
        let file = std::io::BufWriter::new(fs::File::create(&path)?);
        JpegEncoder::new_with_quality(file, quality)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                ExtendedColorType::Rgb8,
            )
            .with_context(|| format!("write {}", path.display()))?;
        still.jpeg = Some(path);
    }
    Ok(still)
}

fn check(job: &StillJob) -> Result<()> {
    ensure!(!job.frames.is_empty(), "no frames to capture");
    ensure!(
        job.png || job.jpeg.is_some(),
        "neither PNG nor JPEG asked for"
    );
    ensure!(
        job.jpeg.is_none_or(|quality| (1..=100).contains(&quality)),
        "JPEG quality is 1 to 100"
    );
    ensure!(
        job.scale > 0.0 && job.scale <= 1.0,
        "a still is scaled by more than 0 and at most 1 (rendering above the composition size is not implemented)"
    );
    Ok(())
}

fn create_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))
}

impl Renderer {
    /// Captures single frames of a Remotion composition through the page, seek and paint a
    /// render uses, and writes each as an image without encoding a video. A loaded project's
    /// ready view is used when there is one; the view is closed afterwards, since its frames
    /// were seeked out of a render's order, and the daemon opens a fresh spare.
    pub fn still(
        &mut self,
        ui: &Ui,
        job: &StillJob,
        command_start: SystemTime,
        progress: &mut (dyn FnMut(u64, u64) -> Result<()> + Send),
    ) -> Result<StillReport> {
        check(job)?;
        create_dir(&job.output)?;
        let gpu_task = self.warm.is_none().then(|| thread::spawn(Warm::new));
        let Taken {
            mut loaded,
            bundle,
            bundle_s,
            frame_server_s,
            frame_server_warm,
            props_s,
            props_swapped,
        } = self.take(&job.options)?;

        let step = Instant::now();
        let (composition, ready) = match (loaded.composition, loaded.take_view()) {
            (Some(composition), Some(view)) => (composition, Ready::Warm(view)),
            (Some(composition), None) => (composition, Ready::Open),
            (None, _) => {
                let (view, composition) = loaded.page.open_unmounted(ui)?;
                (composition, Ready::Unmounted(view))
            }
        };
        loaded.composition = Some(composition);
        let view_warm = matches!(ready, Ready::Warm(_));
        let frames = ordered(&job.frames);
        let last = *frames.last().expect("frames were checked");
        ensure!(
            last < composition.duration_in_frames,
            "frame {last} is outside the composition's {} frames",
            composition.duration_in_frames
        );
        let (width, height) = (composition.width as i32, composition.height as i32);
        let mut view = match ready {
            Ready::Warm(view) => view,
            Ready::Open => loaded.page.open(ui, width, height)?.0,
            Ready::Unmounted(mut view) => {
                Page::fit(&mut view, &composition)?;
                view
            }
        };
        let open_s = step.elapsed();
        let setup_ms = since(command_start);

        let gpu_warm = gpu_task.is_none();
        let warm = match gpu_task {
            Some(task) => self
                .warm
                .insert(task.join().map_err(|_| anyhow!("GPU setup panicked"))??),
            None => self.warm.as_mut().context("the GPU state is missing")?,
        };

        let page = &loaded.page;
        let mut capture = Capture::default();
        let mut seek_retries = 0;
        let mut previous: Option<Arc<RgbImage>> = None;
        let captured_at = Instant::now();
        let mut write_s = Duration::ZERO;
        let stills = thread::scope(|scope| -> Result<Vec<Still>> {
            let mut writers = Vec::new();
            for (done, &frame) in frames.iter().enumerate() {
                let started = Instant::now();
                let mut attempt = 1;
                loop {
                    match Page::seek(&mut view, frame, attempt, false) {
                        Ok(_) => break,
                        Err(error) if error.to_string().contains(RETRY_MARK) => {
                            let (fresh, _) = page.open(ui, width, height)?;
                            std::mem::replace(&mut view, fresh).close()?;
                            attempt += 1;
                            seek_retries += 1;
                        }
                        Err(error) => {
                            return Err(match view.check_project() {
                                Err(refused) => error.context(refused.to_string()),
                                Ok(()) => error,
                            }
                            .context(format!("seek to frame {frame}")));
                        }
                    }
                }
                let paint = capture
                    .next(&view, WATCHDOG)
                    .with_context(|| format!("capture frame {frame}"))?;
                let repeated = paint.is_none();
                let pixels = match paint {
                    Some(paint) => {
                        let (bytes, texture_width, format) = warm.read_paint(&paint)?;
                        let visible = paint.visible;
                        drop(paint);
                        Arc::new(visible_rgb(&bytes, texture_width, format, visible)?)
                    }
                    None => previous
                        .clone()
                        .context("the first still got no delivery")?,
                };
                previous = Some(pixels.clone());
                let captured = Captured {
                    frame,
                    pixels,
                    capture_ms: started.elapsed().as_secs_f64() * 1000.0,
                    repeated,
                };
                writers.push(scope.spawn(move || {
                    let started = Instant::now();
                    write(job, &captured).map(|still| (still, started.elapsed()))
                }));
                progress(done as u64 + 1, frames.len() as u64)?;
            }
            writers
                .into_iter()
                .map(|writer| {
                    let (still, took) = writer
                        .join()
                        .map_err(|_| anyhow!("writing a still panicked"))??;
                    write_s += took;
                    Ok(still)
                })
                .collect()
        })?;
        let capture_s = captured_at.elapsed();
        view.check_project()?;
        view.close()?;
        self.keep(loaded);

        Ok(StillReport {
            composition,
            stills,
            setup_ms,
            total_ms: since(command_start),
            stages: StillStages {
                bundle_s: bundle_s.as_secs_f64(),
                bundle_cached: bundle.cached,
                frame_server_s: frame_server_s.as_secs_f64(),
                props_s: props_s.as_secs_f64(),
                open_s: open_s.as_secs_f64(),
                capture_s: capture_s.as_secs_f64(),
                write_s: write_s.as_secs_f64(),
                warm: Warmth {
                    gpu: gpu_warm,
                    frame_server: frame_server_warm,
                    view: view_warm,
                    props_swapped,
                },
            },
            capture_retries: capture.retries,
            seek_retries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_captured_once_each_in_increasing_order() {
        assert_eq!(ordered(&[900, 120, 450, 120]), vec![120, 450, 900]);
        assert_eq!(ordered(&[7]), vec![7]);
    }

    #[test]
    fn the_visible_rect_of_a_bgra_paint_becomes_rgb() -> Result<()> {
        let mut bytes = Vec::new();
        for y in 0..3u8 {
            for x in 0..4u8 {
                bytes.extend_from_slice(&[x, y, 200, 255]);
            }
        }
        let rect = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let rgb = visible_rgb(&bytes, 4, wgpu::TextureFormat::Bgra8Unorm, rect)?;
        assert_eq!((rgb.width(), rgb.height()), (2, 2));
        assert_eq!(rgb.as_raw(), &[200, 1, 1, 200, 1, 2, 200, 2, 1, 200, 2, 2]);
        let rgba = visible_rgb(&bytes, 4, wgpu::TextureFormat::Rgba8Unorm, rect)?;
        assert_eq!(rgba.get_pixel(0, 0).0, [1, 1, 200]);
        let outside = Rect { x: 3, ..rect };
        assert!(visible_rgb(&bytes, 4, wgpu::TextureFormat::Bgra8Unorm, outside).is_err());
        Ok(())
    }

    #[test]
    fn a_still_job_without_an_image_or_beyond_its_size_is_refused() {
        let job = |png, jpeg, scale| StillJob {
            options: Options {
                project: PathBuf::from("p"),
                entry: None,
                composition: "C".into(),
                props: None,
                mode: crate::Mode::Online,
                timeout: Duration::from_secs(30),
                plan: crate::driver::Plan::single(),
                audio: false,
                video: super::super::VideoDecoder::Native,
                skip: true,
            },
            frames: vec![1],
            output: PathBuf::from("o"),
            png,
            jpeg,
            scale,
        };
        assert!(check(&job(true, None, 1.0)).is_ok());
        assert!(check(&job(false, Some(80), 0.5)).is_ok());
        assert!(check(&job(false, None, 1.0)).is_err());
        assert!(check(&job(true, Some(0), 1.0)).is_err());
        assert!(check(&job(true, None, 2.0)).is_err());
        assert!(check(&job(true, None, 0.0)).is_err());
        let wire: StillJob = serde_json::from_value(serde_json::json!({
            "options": serde_json::to_value(job(true, None, 1.0).options).unwrap(),
            "frames": [3],
            "output": "o",
        }))
        .unwrap();
        assert!(wire.png && wire.jpeg.is_none() && wire.scale == 1.0);
    }
}
