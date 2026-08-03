use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use avifenc_rust::{DataMap, EncoderOptions, ImageBuffer, encode_bytes};

struct BenchmarkCase {
    name: &'static str,
    options: EncoderOptions,
}

struct WebpBenchmarkCase {
    name: &'static str,
    quality: Option<u8>,
}

struct LibavifBenchmarkCase {
    name: &'static str,
    quality: Option<u8>,
    speed: u8,
}

struct RgbError {
    mae: f64,
    maximum: u8,
    psnr: f64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let input = PathBuf::from(args.next().ok_or(Usage)?);
    let width: usize = args.next().ok_or(Usage)?.parse()?;
    let height: usize = args.next().ok_or(Usage)?.parse()?;
    let ffmpeg = PathBuf::from(args.next().ok_or(Usage)?);
    let libavif = PathBuf::from(args.next().ok_or(Usage)?);
    let output_dir = PathBuf::from(args.next().ok_or(Usage)?);
    let iterations: usize = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1);
    if iterations == 0 {
        return Err("iterations must be greater than zero".into());
    }

    let rgba = fs::read(&input)?;
    let expected_bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("image dimensions overflow")?;
    if rgba.len() != expected_bytes {
        return Err(format!(
            "input RGBA length {} does not match {width}x{height} ({expected_bytes})",
            rgba.len()
        )
        .into());
    }
    fs::create_dir_all(&output_dir)?;
    let image = ImageBuffer {
        width,
        height,
        rgba,
    };
    let cases = benchmark_cases();
    let source_png = output_dir.join("source.png");
    encode_png_with_ffmpeg(&ffmpeg, &input, width, height, &source_png)?;

    println!(
        "codec,case,iterations,mean_ms,min_ms,bytes,size_ratio,mae_rgb,max_rgb,psnr_rgb,mae_alpha"
    );
    for case in cases {
        let mut encoded = None;
        let mut durations = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            let bytes = encode_bytes(&image, &case.options)?;
            durations.push(started.elapsed());
            encoded = Some(bytes);
        }
        let encoded = encoded.expect("at least one iteration must run");
        let output = output_dir.join(format!("{}.avif", case.name));
        fs::write(&output, &encoded)?;
        let decoded = decode_with_ffmpeg(
            &ffmpeg,
            &output,
            width,
            height,
            &output_dir,
            "avif",
            case.name,
        )?;
        print_measurement(
            "avif",
            case.name,
            iterations,
            &durations,
            encoded.len(),
            &image.rgba,
            &decoded,
        );
    }

    for case in webp_benchmark_cases() {
        let output = output_dir.join(format!("{}.webp", case.name));
        let mut durations = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            encode_webp_with_ffmpeg(&ffmpeg, &input, width, height, case.quality, &output)?;
            durations.push(started.elapsed());
        }
        let encoded = fs::read(&output)?;
        let decoded = decode_with_ffmpeg(
            &ffmpeg,
            &output,
            width,
            height,
            &output_dir,
            "webp",
            case.name,
        )?;
        print_measurement(
            "webp",
            case.name,
            iterations,
            &durations,
            encoded.len(),
            &image.rgba,
            &decoded,
        );
    }

    for case in libavif_benchmark_cases() {
        let output = output_dir.join(format!("libavif-{}.avif", case.name));
        let mut durations = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let started = Instant::now();
            encode_libavif_with_cli(&libavif, &source_png, case.quality, case.speed, &output)?;
            durations.push(started.elapsed());
        }
        let encoded = fs::read(&output)?;
        let decoded = decode_with_ffmpeg(
            &ffmpeg,
            &output,
            width,
            height,
            &output_dir,
            "libavif",
            case.name,
        )?;
        print_measurement(
            "libavif",
            case.name,
            iterations,
            &durations,
            encoded.len(),
            &image.rgba,
            &decoded,
        );
    }
    Ok(())
}

fn benchmark_cases() -> Vec<BenchmarkCase> {
    let mut cases = Vec::new();
    let lossless = EncoderOptions {
        lossless: true,
        quality: 100,
        quality_alpha: 100,
        ..EncoderOptions::default()
    };
    cases.push(BenchmarkCase {
        name: "lossless",
        options: lossless,
    });

    for quality in [90, 75, 60, 45, 30] {
        let options = EncoderOptions {
            quality,
            quality_alpha: quality,
            speed: 10,
            ..EncoderOptions::default()
        };
        cases.push(BenchmarkCase {
            name: match quality {
                90 => "q90-s10",
                75 => "q75-s10",
                60 => "q60-s10",
                45 => "q45-s10",
                30 => "q30-s10",
                _ => unreachable!(),
            },
            options,
        });
    }

    let q60_speed6 = EncoderOptions {
        quality: 60,
        quality_alpha: 60,
        ..EncoderOptions::default()
    };
    cases.push(BenchmarkCase {
        name: "q60-s6",
        options: q60_speed6,
    });

    let q60_speed6_psnr = EncoderOptions {
        quality: 60,
        quality_alpha: 60,
        advanced: HashMap::from([("tune".to_string(), DataMap::Ascii("psnr".to_string()))]),
        ..EncoderOptions::default()
    };
    cases.push(BenchmarkCase {
        name: "q60-s6-psnr",
        options: q60_speed6_psnr,
    });
    let q60_speed6_fast = EncoderOptions {
        advanced: HashMap::from([
            (
                "rdo_tx_decision".to_string(),
                DataMap::Ascii("false".to_string()),
            ),
            (
                "reduced_tx_set".to_string(),
                DataMap::Ascii("true".to_string()),
            ),
        ]),
        ..EncoderOptions::default()
    };
    cases.push(BenchmarkCase {
        name: "q60-s6-fast",
        options: q60_speed6_fast,
    });
    let q90_speed10_rdo = EncoderOptions {
        quality: 90,
        speed: 10,
        advanced: HashMap::from([
            (
                "rdo_tx_decision".to_string(),
                DataMap::Ascii("true".to_string()),
            ),
            (
                "reduced_tx_set".to_string(),
                DataMap::Ascii("false".to_string()),
            ),
        ]),
        ..EncoderOptions::default()
    };
    cases.push(BenchmarkCase {
        name: "q90-s10-rdo",
        options: q90_speed10_rdo,
    });
    cases
}

fn webp_benchmark_cases() -> Vec<WebpBenchmarkCase> {
    vec![
        WebpBenchmarkCase {
            name: "lossless",
            quality: None,
        },
        WebpBenchmarkCase {
            name: "q90",
            quality: Some(90),
        },
        WebpBenchmarkCase {
            name: "q75",
            quality: Some(75),
        },
        WebpBenchmarkCase {
            name: "q60",
            quality: Some(60),
        },
        WebpBenchmarkCase {
            name: "q45",
            quality: Some(45),
        },
        WebpBenchmarkCase {
            name: "q30",
            quality: Some(30),
        },
    ]
}

fn libavif_benchmark_cases() -> Vec<LibavifBenchmarkCase> {
    vec![
        LibavifBenchmarkCase {
            name: "lossless",
            quality: None,
            speed: 6,
        },
        LibavifBenchmarkCase {
            name: "q90-s10",
            quality: Some(90),
            speed: 10,
        },
        LibavifBenchmarkCase {
            name: "q75-s10",
            quality: Some(75),
            speed: 10,
        },
        LibavifBenchmarkCase {
            name: "q60-s10",
            quality: Some(60),
            speed: 10,
        },
        LibavifBenchmarkCase {
            name: "q45-s10",
            quality: Some(45),
            speed: 10,
        },
        LibavifBenchmarkCase {
            name: "q30-s10",
            quality: Some(30),
            speed: 10,
        },
        LibavifBenchmarkCase {
            name: "q60-s6",
            quality: Some(60),
            speed: 6,
        },
        LibavifBenchmarkCase {
            name: "q90-s6",
            quality: Some(90),
            speed: 6,
        },
    ]
}

fn encode_png_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    width: usize,
    height: usize,
    output: &Path,
) -> Result<(), Box<dyn Error>> {
    let result = Command::new(ffmpeg)
        .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .arg("-i")
        .arg(input.to_str().ok_or("raw input path is not valid UTF-8")?)
        .args(["-frames:v", "1", "-an", "-c:v", "png"])
        .arg(
            output
                .to_str()
                .ok_or("PNG output path is not valid UTF-8")?,
        )
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "FFmpeg PNG encode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(())
}

fn encode_libavif_with_cli(
    libavif: &Path,
    input: &Path,
    quality: Option<u8>,
    speed: u8,
    output: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new(libavif);
    command
        .args(["--jobs", "all", "--speed"])
        .arg(speed.to_string());
    match quality {
        Some(value) => {
            command
                .args(["--yuv", "420"])
                .args(["--qcolor"])
                .arg(value.to_string())
                .args(["--qalpha"])
                .arg(value.to_string());
        }
        None => {
            command.args(["--yuv", "444", "--lossless"]);
        }
    }
    let result = command
        .arg(input.to_str().ok_or("PNG input path is not valid UTF-8")?)
        .arg(
            output
                .to_str()
                .ok_or("AVIF output path is not valid UTF-8")?,
        )
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "libavif avifenc failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(())
}

fn encode_webp_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    width: usize,
    height: usize,
    quality: Option<u8>,
    output: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new(ffmpeg);
    command
        .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .arg("-i")
        .arg(input.to_str().ok_or("raw input path is not valid UTF-8")?)
        .args(["-frames:v", "1", "-an", "-c:v", "libwebp"]);
    match quality {
        Some(value) => {
            command
                .args(["-lossless", "0", "-q:v"])
                .arg(value.to_string());
        }
        None => {
            command.args(["-lossless", "1", "-compression_level", "6"]);
        }
    }
    let result = command
        .arg(
            output
                .to_str()
                .ok_or("WebP output path is not valid UTF-8")?,
        )
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "FFmpeg WebP encode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(())
}

fn print_measurement(
    codec: &str,
    case: &str,
    iterations: usize,
    durations: &[Duration],
    encoded_bytes: usize,
    source: &[u8],
    decoded: &[u8],
) {
    let rgb_error = rgb_error(source, decoded);
    let alpha_error = alpha_mae(source, decoded);
    let mean = durations.iter().copied().sum::<Duration>() / iterations as u32;
    let minimum = durations.iter().copied().min().unwrap_or_default();
    let size_ratio = encoded_bytes as f64 / source.len() as f64;
    println!(
        "{codec},{case},{iterations},{:.3},{:.3},{encoded_bytes},{size_ratio:.6},{:.6},{},{:.3},{:.6}",
        mean.as_secs_f64() * 1000.0,
        minimum.as_secs_f64() * 1000.0,
        rgb_error.mae,
        rgb_error.maximum,
        rgb_error.psnr,
        alpha_error,
    );
}

fn decode_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    width: usize,
    height: usize,
    output_dir: &Path,
    codec: &str,
    case: &str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let raw = output_dir.join(format!("{codec}-{case}.rgba"));
    let result = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input
                .to_str()
                .ok_or("encoded input path is not valid UTF-8")?,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            raw.to_str().ok_or("raw path is not valid UTF-8")?,
        ])
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "FFmpeg decode failed for {case}: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    let decoded = fs::read(&raw)?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("decoded image dimensions overflow")?;
    if decoded.len() != expected {
        return Err(format!(
            "FFmpeg decoded {codec}/{case} to {} bytes; expected {expected}",
            decoded.len()
        )
        .into());
    }
    let _ = fs::remove_file(raw);
    Ok(decoded)
}

fn rgb_error(source: &[u8], decoded: &[u8]) -> RgbError {
    let mut absolute = 0u64;
    let mut squared = 0f64;
    let mut maximum = 0u8;
    let mut samples = 0u64;
    for (source_pixel, decoded_pixel) in source.chunks_exact(4).zip(decoded.chunks_exact(4)) {
        for channel in 0..3 {
            let error = source_pixel[channel].abs_diff(decoded_pixel[channel]);
            absolute += u64::from(error);
            squared += f64::from(error) * f64::from(error);
            maximum = maximum.max(error);
            samples += 1;
        }
    }
    let mae = absolute as f64 / samples as f64;
    let mse = squared / samples as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * ((255.0 * 255.0) / mse).log10()
    };
    RgbError { mae, maximum, psnr }
}

fn alpha_mae(source: &[u8], decoded: &[u8]) -> f64 {
    let total = source
        .chunks_exact(4)
        .zip(decoded.chunks_exact(4))
        .map(|(source, decoded)| f64::from(source[3].abs_diff(decoded[3])))
        .sum::<f64>();
    total / (source.len() / 4) as f64
}

struct Usage;

impl std::fmt::Display for Usage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "usage: benchmark <input.rgba> <width> <height> <ffmpeg> <libavif-avifenc> <output-dir> [iterations]",
        )
    }
}

impl std::fmt::Debug for Usage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("benchmark usage error")
    }
}

impl Error for Usage {}
