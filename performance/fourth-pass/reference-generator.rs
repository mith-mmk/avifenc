fn encode_case(depth: u8, quality: u8, quality_alpha: u8, opaque: bool, gray: bool) -> Vec<u8> {
    let (width, height) = (5, 3);
    let rgba = (0..width * height)
        .flat_map(|i| {
            let i = i as u16;
            let red = i.wrapping_mul(4253).wrapping_add(17);
            [
                red,
                if gray {
                    red
                } else {
                    i.wrapping_mul(2357).wrapping_add(901)
                },
                if gray {
                    red
                } else {
                    u16::MAX - i.wrapping_mul(1877)
                },
                if opaque {
                    u16::MAX
                } else {
                    i.wrapping_mul(3989).wrapping_add(991)
                },
            ]
        })
        .collect();
    let options = avifenc_rust::EncoderOptions {
        quality,
        quality_alpha,
        speed: 10,
        jobs: 0,
        pixel_information: (!gray).then(|| avifenc_rust::PixelInformation {
            bits_per_channel: vec![depth; 3],
            extended_channels: None,
        }),
        ..Default::default()
    };
    avifenc_rust::encode_rgba16_bytes(
        &avifenc_rust::Rgba16ImageBuffer {
            width,
            height,
            rgba,
        },
        &options,
    )
    .unwrap()
}

fn main() {
    let output = std::path::PathBuf::from(std::env::args().nth(1).expect("output directory"));
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(
        output.join("rgba16-8-100-100-alpha.avif"),
        encode_case(8, 100, 100, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-8-100-60-alpha.avif"),
        encode_case(8, 100, 60, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-8-60-100-alpha.avif"),
        encode_case(8, 60, 100, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-8-100-100-opaque.avif"),
        encode_case(8, 100, 100, true, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-10-100-100-alpha.avif"),
        encode_case(10, 100, 100, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-10-100-60-alpha.avif"),
        encode_case(10, 100, 60, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-10-60-100-alpha.avif"),
        encode_case(10, 60, 100, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-10-100-100-opaque.avif"),
        encode_case(10, 100, 100, true, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-12-100-100-alpha.avif"),
        encode_case(12, 100, 100, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-12-100-60-alpha.avif"),
        encode_case(12, 100, 60, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-12-60-100-alpha.avif"),
        encode_case(12, 60, 100, false, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-12-100-100-opaque.avif"),
        encode_case(12, 100, 100, true, false),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-gray12-alpha.avif"),
        encode_case(12, 100, 100, false, true),
    )
    .unwrap();
    std::fs::write(
        output.join("rgba16-gray12-opaque.avif"),
        encode_case(12, 100, 100, true, true),
    )
    .unwrap();
}
