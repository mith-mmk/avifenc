use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use avifenc_rust::{
    EncoderOptions, FrameBuffers, ImageBuffer, PixelInformation, PlaneBuffer, PlaneLayout,
    Rgba16ImageBuffer, encode_bytes, encode_frame_bytes, encode_rgba16_bytes,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn ffmpeg_path() -> Option<PathBuf> {
    let executable = env::var_os("AVIF_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    Command::new(&executable)
        .arg("-version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| executable)
}

fn temporary_path(suffix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos();
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "avifenc-rust-{}-{stamp}-{id}.{suffix}",
        std::process::id(),
    ))
}

fn rgb_error(source: &[u8], decoded: &[u8]) -> (f64, u8) {
    let mut total = 0u64;
    let mut maximum = 0u8;
    let mut samples = 0u64;
    for (source_pixel, decoded_pixel) in source.chunks_exact(4).zip(decoded.chunks_exact(4)) {
        for channel in 0..3 {
            let error = source_pixel[channel].abs_diff(decoded_pixel[channel]);
            total += u64::from(error);
            maximum = maximum.max(error);
            samples += 1;
        }
        assert_eq!(decoded_pixel[3], 255, "opaque source must decode opaque");
    }
    (total as f64 / samples as f64, maximum)
}

fn gray_error(source: &[u8], decoded: &[u8]) -> (f64, u8) {
    let mut total = 0u64;
    let mut maximum = 0u8;
    for (&source_sample, &decoded_sample) in source.iter().zip(decoded) {
        let error = source_sample.abs_diff(decoded_sample);
        total += u64::from(error);
        maximum = maximum.max(error);
    }
    (total as f64 / source.len() as f64, maximum)
}

fn plane(
    plane: u8,
    width: usize,
    height: usize,
    subsampling_x: u8,
    subsampling_y: u8,
    samples: Vec<u16>,
) -> PlaneBuffer {
    PlaneBuffer {
        layout: PlaneLayout {
            plane,
            width,
            height,
            subsampling_x,
            subsampling_y,
            sample_count: width * height,
        },
        samples,
    }
}

fn assert_ffmpeg_raw_decode(ffmpeg: &Path, input: &Path, pixel_format: &str, expected_length: u64) {
    let raw = temporary_path("raw");
    let result = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pixel_format,
            raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg {pixel_format} decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::metadata(&raw).unwrap().len(), expected_length);
    let _ = fs::remove_file(raw);
}

fn assert_ffmpeg_raw_decode_equals(
    ffmpeg: &Path,
    input: &Path,
    pixel_format: &str,
    expected: &[u8],
) {
    assert_ffmpeg_raw_decode_equals_with_map(ffmpeg, input, None, pixel_format, expected);
}

fn assert_ffmpeg_raw_decode_equals_with_map(
    ffmpeg: &Path,
    input: &Path,
    map: Option<&str>,
    pixel_format: &str,
    expected: &[u8],
) {
    let raw = temporary_path("raw");
    let mut args = vec![
        "-v".to_string(),
        "error".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        input.to_str().unwrap().to_string(),
    ];
    if let Some(map) = map {
        args.extend(["-map".to_string(), map.to_string()]);
    }
    args.extend([
        "-frames:v".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "-pix_fmt".to_string(),
        pixel_format.to_string(),
        raw.to_str().unwrap().to_string(),
    ]);
    let result = Command::new(ffmpeg).args(args).output().unwrap();
    assert!(
        result.status.success(),
        "ffmpeg {pixel_format} decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&raw).unwrap(), expected);
    let _ = fs::remove_file(raw);
}

fn gradient(with_alpha: bool) -> ImageBuffer {
    let width = 17;
    let height = 13;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[
                (x * 7) as u8,
                (y * 9) as u8,
                ((x + y) * 5) as u8,
                if with_alpha {
                    ((x + y) * 8).min(255) as u8
                } else {
                    255
                },
            ]);
        }
    }
    ImageBuffer {
        width,
        height,
        rgba,
    }
}

fn large_lossless_gradient() -> ImageBuffer {
    let width = 128;
    let height = 128;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[
                ((x * 7 + y * 3) & 255) as u8,
                ((y * 11 + x * 5) & 255) as u8,
                ((x * 13 + y * 17) & 255) as u8,
                255,
            ]);
        }
    }
    ImageBuffer {
        width,
        height,
        rgba,
    }
}

#[test]
fn ffmpeg_decodes_the_encoded_still_when_available() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping standalone encoder oracle");
        return;
    };
    let input = temporary_path("avif");
    let raw = temporary_path("rgba");
    let image = gradient(false);
    let options = EncoderOptions {
        tile_rows_log2: 1,
        tile_cols_log2: 1,
        advanced: HashMap::from([
            (
                "cdef".to_string(),
                avifenc_rust::DataMap::Ascii("false".to_string()),
            ),
            (
                "lrf".to_string(),
                avifenc_rust::DataMap::Ascii("false".to_string()),
            ),
            (
                "rdo-lookahead-frames".to_string(),
                avifenc_rust::DataMap::UInt(1),
            ),
        ]),
        ..EncoderOptions::default()
    };
    fs::write(&input, encode_bytes(&image, &options).unwrap()).unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::metadata(&raw).unwrap().len(),
        (image.width * image.height * 4) as u64
    );
    let decoded = fs::read(&raw).unwrap();
    let (average, maximum) = rgb_error(&image.rgba, &decoded);
    assert!(average <= 20.0, "average RGB error {average} exceeds limit");
    assert!(maximum <= 96, "maximum RGB error {maximum} exceeds limit");
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(raw);
}

#[test]
fn ffmpeg_decodes_opaque_rgba8_lossless_bit_exactly_when_available() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping lossless standalone oracle");
        return;
    };
    let input = temporary_path("avif");
    let raw = temporary_path("rgba");
    let image = gradient(false);
    let options = EncoderOptions {
        lossless: true,
        ..EncoderOptions::default()
    };
    fs::write(&input, encode_bytes(&image, &options).unwrap()).unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg lossless decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&raw).unwrap(), image.rgba);
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(raw);
}

#[test]
fn ffmpeg_decodes_large_opaque_lossless_color_with_partition_refinement() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping large partition oracle");
        return;
    };
    let input = temporary_path("avif");
    let image = large_lossless_gradient();
    let options = EncoderOptions {
        lossless: true,
        ..EncoderOptions::default()
    };
    fs::write(&input, encode_bytes(&image, &options).unwrap()).unwrap();
    assert_ffmpeg_raw_decode_equals(&ffmpeg, &input, "rgba", &image.rgba);
    let _ = fs::remove_file(input);
}

#[test]
fn ffmpeg_sees_color_and_alpha_items_when_available() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping alpha stream oracle");
        return;
    };
    let input = temporary_path("avif");
    let color_raw = temporary_path("rgba");
    let alpha_raw = temporary_path("gray");
    let image = gradient(true);
    fs::write(
        &input,
        encode_bytes(&image, &EncoderOptions::default()).unwrap(),
    )
    .unwrap();
    // ffmpeg has no portable command-line filter that automatically composites
    // an AVIF auxiliary alpha item into `rgba`; decode and compare both mapped
    // streams independently instead.
    for (stream, output, pixel_format) in [("0:0", &color_raw, "rgba"), ("0:1", &alpha_raw, "gray")]
    {
        let result = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-i",
                input.to_str().unwrap(),
                "-map",
                stream,
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                pixel_format,
                output.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "ffmpeg alpha stream decode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let color = fs::read(&color_raw).unwrap();
    let alpha = fs::read(&alpha_raw).unwrap();
    assert_eq!(color.len(), image.rgba.len());
    assert_eq!(alpha.len(), image.rgba.len() / 4);
    let color_image = ImageBuffer {
        width: image.width,
        height: image.height,
        rgba: image
            .rgba
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
    };
    let (color_average, color_maximum) = rgb_error(&color_image.rgba, &color);
    let source_alpha: Vec<u8> = image.rgba.chunks_exact(4).map(|pixel| pixel[3]).collect();
    let (alpha_average, alpha_maximum) = gray_error(&source_alpha, &alpha);
    assert!(
        color_average <= 20.0,
        "average RGB error {color_average} exceeds alpha-image limit"
    );
    assert!(
        color_maximum <= 96,
        "maximum RGB error {color_maximum} exceeds alpha-image limit"
    );
    assert!(
        alpha_average <= 20.0,
        "average alpha error {alpha_average} exceeds limit"
    );
    assert!(
        alpha_maximum <= 96,
        "maximum alpha error {alpha_maximum} exceeds limit"
    );
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(color_raw);
    let _ = fs::remove_file(alpha_raw);
}

#[test]
fn ffmpeg_decodes_rgba8_lossless_alpha_bit_exactly_when_available() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping lossless alpha oracle");
        return;
    };
    let input = temporary_path("avif");
    let color_raw = temporary_path("rgba");
    let alpha_raw = temporary_path("gray");
    let image = gradient(true);
    let options = EncoderOptions {
        lossless: true,
        ..EncoderOptions::default()
    };
    fs::write(&input, encode_bytes(&image, &options).unwrap()).unwrap();
    for (stream, output, pixel_format) in [("0:0", &color_raw, "rgba"), ("0:1", &alpha_raw, "gray")]
    {
        let result = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-i",
                input.to_str().unwrap(),
                "-map",
                stream,
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                pixel_format,
                output.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "ffmpeg lossless alpha decode failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let color = fs::read(&color_raw).unwrap();
    let alpha = fs::read(&alpha_raw).unwrap();
    let expected_color = image
        .rgba
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
        .collect::<Vec<_>>();
    let expected_alpha = image
        .rgba
        .chunks_exact(4)
        .map(|pixel| pixel[3])
        .collect::<Vec<_>>();
    assert_eq!(color, expected_color);
    assert_eq!(alpha, expected_alpha);
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(color_raw);
    let _ = fs::remove_file(alpha_raw);
}

#[test]
fn ffmpeg_keeps_color_and_alpha_quality_independent() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping mixed-quality oracle");
        return;
    };
    let image = gradient(true);

    let alpha_lossless_input = temporary_path("avif");
    let alpha_lossless_raw = temporary_path("gray");
    let alpha_lossless_options = EncoderOptions {
        quality: 20,
        quality_alpha: 100,
        ..EncoderOptions::default()
    };
    fs::write(
        &alpha_lossless_input,
        encode_bytes(&image, &alpha_lossless_options).unwrap(),
    )
    .unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            alpha_lossless_input.to_str().unwrap(),
            "-map",
            "0:1",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            alpha_lossless_raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg mixed alpha decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let expected_alpha = image
        .rgba
        .chunks_exact(4)
        .map(|pixel| pixel[3])
        .collect::<Vec<_>>();
    assert_eq!(fs::read(&alpha_lossless_raw).unwrap(), expected_alpha);
    let _ = fs::remove_file(alpha_lossless_input);
    let _ = fs::remove_file(alpha_lossless_raw);

    let color_lossless_input = temporary_path("avif");
    let color_lossless_raw = temporary_path("rgba");
    let color_lossless_options = EncoderOptions {
        quality: 100,
        quality_alpha: 20,
        ..EncoderOptions::default()
    };
    fs::write(
        &color_lossless_input,
        encode_bytes(&image, &color_lossless_options).unwrap(),
    )
    .unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            color_lossless_input.to_str().unwrap(),
            "-map",
            "0:0",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            color_lossless_raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg mixed color decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let expected_color = image
        .rgba
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
        .collect::<Vec<_>>();
    assert_eq!(fs::read(&color_lossless_raw).unwrap(), expected_color);
    let _ = fs::remove_file(color_lossless_input);
    let _ = fs::remove_file(color_lossless_raw);
}

#[test]
fn ffmpeg_decodes_native_high_bit_depth_sampling() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping native-plane oracle");
        return;
    };

    let ten_bit = FrameBuffers {
        width: 5,
        height: 3,
        planes: vec![
            plane(
                0,
                5,
                3,
                0,
                0,
                (0..15).map(|sample| (sample * 71) as u16).collect(),
            ),
            plane(1, 3, 3, 1, 0, vec![512; 9]),
            plane(2, 3, 3, 1, 0, vec![512; 9]),
        ],
    };
    let ten_options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![10, 10, 10],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let ten_input = temporary_path("avif");
    fs::write(
        &ten_input,
        encode_frame_bytes(&ten_bit, &ten_options).unwrap(),
    )
    .unwrap();
    // 5x3 luma plus two ceil(5/2)x3 chroma planes, 16 bits per sample.
    assert_ffmpeg_raw_decode(&ffmpeg, &ten_input, "yuv422p10le", 66);

    let twelve_options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12, 12, 12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let twelve_bit = FrameBuffers {
        width: 3,
        height: 2,
        planes: vec![
            plane(0, 3, 2, 0, 0, vec![0, 1024, 4095, 512, 1536, 3072]),
            plane(1, 3, 2, 0, 0, vec![2048; 6]),
            plane(2, 3, 2, 0, 0, vec![2048; 6]),
        ],
    };
    let twelve_input = temporary_path("avif");
    fs::write(
        &twelve_input,
        encode_frame_bytes(&twelve_bit, &twelve_options).unwrap(),
    )
    .unwrap();
    assert_ffmpeg_raw_decode(&ffmpeg, &twelve_input, "yuv444p12le", 36);

    let gray = FrameBuffers {
        width: 3,
        height: 2,
        planes: vec![plane(0, 3, 2, 0, 0, vec![0, 1024, 4095, 512, 1536, 3072])],
    };
    let gray_options = EncoderOptions {
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let gray_input = temporary_path("avif");
    fs::write(
        &gray_input,
        encode_frame_bytes(&gray, &gray_options).unwrap(),
    )
    .unwrap();
    assert_ffmpeg_raw_decode(&ffmpeg, &gray_input, "gray12le", 12);

    let _ = fs::remove_file(ten_input);
    let _ = fs::remove_file(twelve_input);
    let _ = fs::remove_file(gray_input);
}

#[test]
fn ffmpeg_decodes_native_8_bit_444_lossless_bit_exactly_when_available() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping native lossless oracle");
        return;
    };
    let width = 5;
    let height = 3;
    let y: Vec<u16> = (0..width * height)
        .map(|index| (index * 7) as u16)
        .collect();
    let u: Vec<u16> = (0..width * height)
        .map(|index| (128 + index) as u16)
        .collect();
    let v: Vec<u16> = (0..width * height)
        .map(|index| (255 - index) as u16)
        .collect();
    let buffers = FrameBuffers {
        width,
        height,
        planes: vec![
            plane(0, width, height, 0, 0, y.clone()),
            plane(1, width, height, 0, 0, u.clone()),
            plane(2, width, height, 0, 0, v.clone()),
        ],
    };
    let options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![8, 8, 8],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let input = temporary_path("avif");
    let raw = temporary_path("rgb");
    fs::write(&input, encode_frame_bytes(&buffers, &options).unwrap()).unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg native lossless decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let expected = (0..width * height)
        .flat_map(|index| [v[index] as u8, y[index] as u8, u[index] as u8])
        .collect::<Vec<_>>();
    assert_eq!(fs::read(&raw).unwrap(), expected);
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(raw);
}

#[test]
fn ffmpeg_decodes_native_high_bit_depth_444_and_gray_losslessly() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping high-bit-depth lossless oracle");
        return;
    };
    let width = 5;
    let height = 3;
    let y: Vec<u16> = (0..width * height)
        .map(|index| (index * 71) as u16)
        .collect();
    let u: Vec<u16> = (0..width * height)
        .map(|index| 1023 - (index * 31) as u16)
        .collect();
    let v: Vec<u16> = (0..width * height)
        .map(|index| (index * 19) as u16)
        .collect();
    let buffers = FrameBuffers {
        width,
        height,
        planes: vec![
            plane(0, width, height, 0, 0, y.clone()),
            plane(1, width, height, 0, 0, u.clone()),
            plane(2, width, height, 0, 0, v.clone()),
        ],
    };
    let options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![10, 10, 10],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let input = temporary_path("avif");
    let raw = temporary_path("gbrp10");
    fs::write(&input, encode_frame_bytes(&buffers, &options).unwrap()).unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            input.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gbrp10le",
            raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg 10-bit lossless decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let expected = y
        .iter()
        .chain(u.iter())
        .chain(v.iter())
        .flat_map(|sample| sample.to_le_bytes())
        .collect::<Vec<_>>();
    assert_eq!(fs::read(&raw).unwrap(), expected);
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(raw);

    let gray = FrameBuffers {
        width,
        height,
        planes: vec![plane(
            0,
            width,
            height,
            0,
            0,
            (0..width * height)
                .map(|index| (index * 271) as u16)
                .collect(),
        )],
    };
    let gray_options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let gray_input = temporary_path("avif");
    let gray_raw = temporary_path("gray12");
    fs::write(
        &gray_input,
        encode_frame_bytes(&gray, &gray_options).unwrap(),
    )
    .unwrap();
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-y",
            "-i",
            gray_input.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray12le",
            gray_raw.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "ffmpeg 12-bit gray lossless decode failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let expected_gray = (0..width * height)
        .map(|index| (index * 271) as u16)
        .flat_map(|sample| sample.to_le_bytes())
        .collect::<Vec<_>>();
    assert_eq!(fs::read(&gray_raw).unwrap(), expected_gray);
    let _ = fs::remove_file(gray_input);
    let _ = fs::remove_file(gray_raw);
}

#[test]
fn ffmpeg_decodes_rgba16_lossless_color_and_alpha_bit_exactly() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping RGBA16 lossless oracle");
        return;
    };
    let image = Rgba16ImageBuffer {
        width: 2,
        height: 2,
        rgba: vec![
            0, 16384, 32768, 65535, 65535, 32768, 16384, 32768, 1024, 2048, 4096, 8192, 8192, 4096,
            2048, 4096,
        ],
    };
    let options = EncoderOptions {
        lossless: true,
        pixel_information: Some(PixelInformation {
            bits_per_channel: vec![12, 12, 12],
            extended_channels: None,
        }),
        ..EncoderOptions::default()
    };
    let input = temporary_path("avif");
    fs::write(&input, encode_rgba16_bytes(&image, &options).unwrap()).unwrap();
    let scale = |value: u16| ((u32::from(value) * 4095 + 32767) / 65535) as u16;
    let pixels = image.rgba.chunks_exact(4).collect::<Vec<_>>();
    let expected_color = [1usize, 2, 0]
        .into_iter()
        .flat_map(|channel| {
            pixels
                .iter()
                .map(move |pixel| scale(pixel[channel]).to_le_bytes())
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_ffmpeg_raw_decode_equals(&ffmpeg, &input, "gbrp12le", &expected_color);
    let expected_alpha = pixels
        .iter()
        .flat_map(|pixel| scale(pixel[3]).to_le_bytes())
        .collect::<Vec<_>>();
    assert_ffmpeg_raw_decode_equals_with_map(
        &ffmpeg,
        &input,
        Some("0:1"),
        "gray12le",
        &expected_alpha,
    );
    let _ = fs::remove_file(input);
}

#[test]
fn ffmpeg_decodes_native_subsampled_lossless_planes_exactly() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping subsampled lossless oracle");
        return;
    };
    for (subsampling_x, subsampling_y, chroma_width, chroma_height, pixel_format) in
        [(1, 1, 4, 4, "yuv420p"), (1, 0, 4, 8, "yuv422p")]
    {
        let width = 8;
        let height = 8;
        let y: Vec<u16> = (0..width * height)
            .map(|index| (index * 3) as u16)
            .collect();
        let u: Vec<u16> = (0..chroma_width * chroma_height)
            .map(|index| (64 + index) as u16)
            .collect();
        let v: Vec<u16> = (0..chroma_width * chroma_height)
            .map(|index| (192 - index) as u16)
            .collect();
        let buffers = FrameBuffers {
            width,
            height,
            planes: vec![
                plane(0, width, height, 0, 0, y.clone()),
                plane(
                    1,
                    chroma_width,
                    chroma_height,
                    subsampling_x,
                    subsampling_y,
                    u.clone(),
                ),
                plane(
                    2,
                    chroma_width,
                    chroma_height,
                    subsampling_x,
                    subsampling_y,
                    v.clone(),
                ),
            ],
        };
        let options = EncoderOptions {
            lossless: true,
            pixel_information: Some(PixelInformation {
                bits_per_channel: vec![8, 8, 8],
                extended_channels: None,
            }),
            ..EncoderOptions::default()
        };
        let input = temporary_path("avif");
        fs::write(&input, encode_frame_bytes(&buffers, &options).unwrap()).unwrap();
        let expected = y
            .iter()
            .chain(u.iter())
            .chain(v.iter())
            .map(|sample| *sample as u8)
            .collect::<Vec<_>>();
        assert_ffmpeg_raw_decode_equals(&ffmpeg, &input, pixel_format, &expected);
        let _ = fs::remove_file(input);
    }
}

#[test]
fn ffmpeg_decodes_native_high_bit_depth_subsampled_lossless_planes_exactly() {
    let Some(ffmpeg) = ffmpeg_path() else {
        eprintln!("ffmpeg is not available; skipping high-bit-depth subsampled oracle");
        return;
    };

    for (bit_depth, subsampling_x, subsampling_y, chroma_width, chroma_height, pixel_format) in [
        (10u8, 1u8, 1u8, 4usize, 4usize, "yuv420p10le"),
        (12u8, 1u8, 0u8, 4usize, 8usize, "yuv422p12le"),
    ] {
        let width = 8;
        let height = 8;
        let max = 1u16 << bit_depth;
        let y: Vec<u16> = (0..width * height)
            .map(|index| ((index * 37) as u16) % max)
            .collect();
        let u: Vec<u16> = (0..chroma_width * chroma_height)
            .map(|index| 512 + (index as u16 % (max - 512)))
            .collect();
        let v: Vec<u16> = (0..chroma_width * chroma_height)
            .map(|index| 256 + (index as u16 % (max - 256)))
            .collect();
        let buffers = FrameBuffers {
            width,
            height,
            planes: vec![
                plane(0, width, height, 0, 0, y.clone()),
                plane(
                    1,
                    chroma_width,
                    chroma_height,
                    subsampling_x,
                    subsampling_y,
                    u.clone(),
                ),
                plane(
                    2,
                    chroma_width,
                    chroma_height,
                    subsampling_x,
                    subsampling_y,
                    v.clone(),
                ),
            ],
        };
        let options = EncoderOptions {
            lossless: true,
            pixel_information: Some(PixelInformation {
                bits_per_channel: vec![bit_depth; 3],
                extended_channels: None,
            }),
            ..EncoderOptions::default()
        };
        let input = temporary_path("avif");
        fs::write(&input, encode_frame_bytes(&buffers, &options).unwrap()).unwrap();
        let expected = y
            .iter()
            .chain(u.iter())
            .chain(v.iter())
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        assert_ffmpeg_raw_decode_equals(&ffmpeg, &input, pixel_format, &expected);
        let _ = fs::remove_file(input);
    }
}
