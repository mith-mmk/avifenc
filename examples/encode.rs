use std::env;
use std::fs;

use avifenc_rust::{EncoderOptions, ImageBuffer, encode_bytes};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let output = args.next().unwrap_or_else(|| "image.avif".to_string());
    let with_alpha = args.next().as_deref() == Some("alpha");
    let width = 32;
    let height = 24;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[
                (x * 8) as u8,
                (y * 10) as u8,
                ((x + y) * 4) as u8,
                if with_alpha {
                    ((x + y) * 16).min(255) as u8
                } else {
                    255
                },
            ]);
        }
    }
    let bytes = encode_bytes(
        &ImageBuffer {
            width,
            height,
            rgba,
        },
        &EncoderOptions::default(),
    )?;
    fs::write(output, bytes)?;
    Ok(())
}
