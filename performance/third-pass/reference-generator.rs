use avifenc_rust::{EncoderOptions, ImageBuffer, PixelInformation, Rgba16ImageBuffer};

fn main() {
    let output = std::path::PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&output).unwrap();
    let (width, height) = (32, 24);
    let rgba: Vec<u8> = (0..width * height)
        .flat_map(|i| {
            let (x, y) = (i % width, i / width);
            [
                ((x * 7 + y * 3) % 256) as u8,
                ((x * 2 + y * 11) % 256) as u8,
                ((x ^ y) % 256) as u8,
                ((x * 5 + y * 7) % 256) as u8,
            ]
        })
        .collect();
    for depth in [8, 10, 12] {
        let options = EncoderOptions {
            quality: 60,
            quality_alpha: 60,
            speed: 10,
            jobs: 0,
            pixel_information: (depth != 8).then(|| PixelInformation {
                bits_per_channel: vec![depth; 3],
                extended_channels: None,
            }),
            ..Default::default()
        };
        let encoded = if depth == 8 {
            avifenc_rust::encode_bytes(
                &ImageBuffer {
                    width,
                    height,
                    rgba: rgba.clone(),
                },
                &options,
            )
            .unwrap()
        } else {
            avifenc_rust::encode_rgba16_bytes(
                &Rgba16ImageBuffer {
                    width,
                    height,
                    rgba: rgba.iter().map(|&v| u16::from(v) * 257).collect(),
                },
                &options,
            )
            .unwrap()
        };
        std::fs::write(output.join(format!("portable-{depth}bit.avif")), encoded).unwrap();
    }
}
