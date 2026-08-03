use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use avifenc_rust::{EncoderOptions, ImageBuffer, encode_bytes};

struct BenchmarkCase {
    name: &'static str,
    options: EncoderOptions,
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

    println!(
        "case,iterations,mean_ms,min_ms,avif_bytes,size_ratio,mae_rgb,max_rgb,psnr_rgb,mae_alpha"
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
        let decoded = decode_with_ffmpeg(&ffmpeg, &output, width, height, &output_dir, case.name)?;
        let rgb_error = rgb_error(&image.rgba, &decoded);
        let alpha_error = alpha_mae(&image.rgba, &decoded);
        let mean = durations.iter().copied().sum::<Duration>() / iterations as u32;
        let minimum = durations.iter().copied().min().unwrap_or_default();
        let size_ratio = encoded.len() as f64 / image.rgba.len() as f64;
        println!(
            "{},{iterations},{:.3},{:.3},{},{:.6},{:.6},{},{:.3},{:.6}",
            case.name,
            mean.as_secs_f64() * 1000.0,
            minimum.as_secs_f64() * 1000.0,
            encoded.len(),
            size_ratio,
            rgb_error.mae,
            rgb_error.maximum,
            rgb_error.psnr,
            alpha_error,
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
    cases
}

fn decode_with_ffmpeg(
    ffmpeg: &Path,
    input: &Path,
    width: usize,
    height: usize,
    output_dir: &Path,
    case: &str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let raw = output_dir.join(format!("{case}.rgba"));
    let result = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input.to_str().ok_or("AVIF path is not valid UTF-8")?,
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
            "FFmpeg decoded {case} to {} bytes; expected {expected}",
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
            "usage: benchmark <input.rgba> <width> <height> <ffmpeg> <output-dir> [iterations]",
        )
    }
}

impl std::fmt::Debug for Usage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("benchmark usage error")
    }
}

impl Error for Usage {}
