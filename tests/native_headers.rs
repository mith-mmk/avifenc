use avifenc_rust::{
    ColorInformation, EncoderOptions, FrameBuffers, NativeColorInformation, NativeEncodeOptions,
    NativeSubsampling, NclxColorInformation, PlaneBuffer, PlaneLayout,
};

fn box_at(data: &[u8], offset: usize) -> (usize, [u8; 4], &[u8]) {
    assert!(data.len() - offset >= 8);
    let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    assert!((8..=data.len() - offset).contains(&size));
    (
        size,
        data[offset + 4..offset + 8].try_into().unwrap(),
        &data[offset + 8..offset + size],
    )
}

fn boxes(mut data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut result = Vec::new();
    while !data.is_empty() {
        let (size, kind, payload) = box_at(data, 0);
        result.push((kind, payload));
        data = &data[size..];
    }
    result
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
    let matches = boxes(data)
        .into_iter()
        .filter(|(actual, _)| actual == kind)
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected one {kind:?} box");
    matches[0].1
}

fn sequence_payloads(data: &[u8]) -> Vec<&[u8]> {
    let mdat = boxes(data)
        .into_iter()
        .find(|(kind, _)| kind == b"mdat")
        .map(|(_, payload)| payload)
        .expect("mdat");
    let mut payloads = Vec::new();
    let mut offset = 0;
    while offset < mdat.len() {
        let header = mdat[offset];
        offset += 1;
        assert_eq!(header & 0x80, 0, "forbidden OBU bit");
        let obu_type = (header >> 3) & 0x0f;
        if header & 0x04 != 0 {
            assert!(offset < mdat.len());
            offset += 1;
        }
        assert_ne!(header & 0x02, 0, "native still OBUs require a size field");
        let mut size = 0usize;
        let mut shift = 0;
        loop {
            assert!(offset < mdat.len());
            let byte = mdat[offset];
            offset += 1;
            size |= usize::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            assert!(shift < usize::BITS);
        }
        assert!(size <= mdat.len() - offset);
        if obu_type == 1 {
            payloads.push(&mdat[offset..offset + size]);
        }
        offset += size;
    }
    payloads
}

struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl BitReader<'_> {
    fn read(&mut self, count: usize) -> u32 {
        assert!(count <= 32);
        assert!(count <= self.data.len() * 8 - self.position);
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1)
                | u32::from((self.data[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        value
    }

    fn bit(&mut self) -> bool {
        self.read(1) != 0
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SequenceColor {
    bit_depth: u8,
    monochrome: bool,
    full_range: bool,
    subsampling_x: bool,
    subsampling_y: bool,
    color_primaries: u8,
    transfer_characteristics: u8,
    matrix_coefficients: u8,
}

fn sequence_color(payload: &[u8]) -> SequenceColor {
    let mut reader = BitReader {
        data: payload,
        position: 0,
    };
    let profile = reader.read(3);
    assert!(profile <= 2);
    assert!(reader.bit()); // still_picture
    assert!(reader.bit()); // reduced_still_picture_header
    reader.read(5); // seq_level_idx[0]
    let width_bits = reader.read(4) + 1;
    let height_bits = reader.read(4) + 1;
    reader.read(width_bits as usize);
    reader.read(height_bits as usize);
    reader.read(3); // use_128x128_superblock, filter_intra, intra_edge_filter
    reader.read(3); // superres, cdef, restoration

    let high_bitdepth = reader.bit();
    let twelve_bit = profile == 2 && high_bitdepth && reader.bit();
    let bit_depth = if profile == 2 && high_bitdepth {
        if twelve_bit { 12 } else { 10 }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    let monochrome = profile != 1 && reader.bit();
    let (color_primaries, transfer_characteristics, matrix_coefficients) = if reader.bit() {
        (
            reader.read(8) as u8,
            reader.read(8) as u8,
            reader.read(8) as u8,
        )
    } else {
        (0, 0, 0)
    };
    if monochrome {
        SequenceColor {
            bit_depth,
            monochrome,
            full_range: reader.bit(),
            subsampling_x: true,
            subsampling_y: true,
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
        }
    } else if (
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
    ) == (1, 13, 0)
    {
        reader.bit(); // separate_uv_delta_q
        SequenceColor {
            bit_depth,
            monochrome,
            full_range: true,
            subsampling_x: false,
            subsampling_y: false,
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
        }
    } else {
        let full_range = reader.bit();
        let (subsampling_x, subsampling_y) = match profile {
            0 => (true, true),
            1 => (false, false),
            2 if bit_depth == 12 => {
                let x = reader.bit();
                (x, x && reader.bit())
            }
            2 => (true, false),
            _ => unreachable!(),
        };
        SequenceColor {
            bit_depth,
            monochrome,
            full_range,
            subsampling_x,
            subsampling_y,
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
        }
    }
}

fn item_properties(data: &[u8], item_id: u16) -> Vec<([u8; 4], &[u8])> {
    let meta = boxes(data)
        .into_iter()
        .find(|(actual, _)| actual == b"meta")
        .expect("meta")
        .1;
    let iprp = child(&meta[4..], b"iprp");
    let properties = boxes(child(iprp, b"ipco"));
    let ipma = child(iprp, b"ipma");
    let entry_count = u32::from_be_bytes(ipma[4..8].try_into().unwrap()) as usize;
    let mut cursor = 8;
    for _ in 0..entry_count {
        let actual_id = u16::from_be_bytes(ipma[cursor..cursor + 2].try_into().unwrap());
        let count = usize::from(ipma[cursor + 2]);
        cursor += 3;
        if actual_id == item_id {
            return ipma[cursor..cursor + count]
                .iter()
                .map(|index| properties[usize::from(index & 0x7f) - 1])
                .collect();
        }
        cursor += count;
    }
    panic!("item {item_id} missing");
}

fn item_property<'a>(data: &'a [u8], item_id: u16, kind: &[u8; 4]) -> &'a [u8] {
    item_properties(data, item_id)
        .into_iter()
        .find(|(actual, _)| actual == kind)
        .map(|(_, payload)| payload)
        .unwrap_or_else(|| panic!("property {kind:?} missing from item {item_id}"))
}

fn buffers(subsampling: NativeSubsampling) -> FrameBuffers {
    let (width, height): (usize, usize) = (4, 4);
    let (sx, sy) = match subsampling {
        NativeSubsampling::Cs420 => (1, 1),
        NativeSubsampling::Cs422 => (1, 0),
        NativeSubsampling::Cs400 | NativeSubsampling::Cs444 => (0, 0),
    };
    let color_planes = if subsampling == NativeSubsampling::Cs400 {
        1
    } else {
        3
    };
    let mut planes = (0..color_planes)
        .map(|plane| {
            let (plane_width, plane_height) = if plane == 0 {
                (width, height)
            } else {
                (width.div_ceil(1usize << sx), height.div_ceil(1usize << sy))
            };
            PlaneBuffer {
                layout: PlaneLayout {
                    plane: plane as u8,
                    width: plane_width,
                    height: plane_height,
                    subsampling_x: if plane == 0 { 0 } else { sx },
                    subsampling_y: if plane == 0 { 0 } else { sy },
                    sample_count: plane_width * plane_height,
                },
                samples: vec![40 + plane as u16; plane_width * plane_height],
            }
        })
        .collect::<Vec<_>>();
    planes.push(PlaneBuffer {
        layout: PlaneLayout {
            plane: 3,
            width,
            height,
            subsampling_x: 0,
            subsampling_y: 0,
            sample_count: width * height,
        },
        samples: vec![128; width * height],
    });
    FrameBuffers {
        width,
        height,
        planes,
    }
}

fn options(subsampling: NativeSubsampling) -> NativeEncodeOptions {
    NativeEncodeOptions {
        bit_depth: 8,
        subsampling,
        encoder: EncoderOptions {
            quality: 80,
            quality_alpha: 80,
            color_information: Some(
                NclxColorInformation {
                    color_primaries: 1,
                    transfer_characteristics: 13,
                    matrix_coefficients: 6,
                    full_range_flag: true,
                }
                .to_color_information(),
            ),
            ..EncoderOptions::default()
        },
        color_information: NativeColorInformation {
            nclx: Some(NclxColorInformation {
                color_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients: 6,
                full_range_flag: true,
            }),
            icc_profile: Some(vec![9, 8, 7, 6]),
            icc_color_type: Some(*b"rICC"),
        },
    }
}

#[test]
fn native_associations_carry_exact_color_boxes_and_av1c_flags() {
    for lossless in [false, true] {
        for nested_icc in [false, true] {
            for subsampling in [NativeSubsampling::Cs444, NativeSubsampling::Cs400] {
                let mut native_options = options(subsampling);
                native_options.encoder.lossless = lossless;
                native_options.encoder.color_information = if nested_icc {
                    Some(ColorInformation {
                        color_type: *b"rICC",
                        payload: vec![9, 8, 7, 6],
                    })
                } else {
                    Some(
                        NclxColorInformation {
                            color_primaries: 1,
                            transfer_characteristics: 13,
                            matrix_coefficients: 6,
                            full_range_flag: true,
                        }
                        .to_color_information(),
                    )
                };
                let encoded =
                    avifenc_rust::encode_native_bytes(&buffers(subsampling), &native_options)
                        .unwrap();
                let sequences = sequence_payloads(&encoded);
                assert_eq!(sequences.len(), 2);
                let primary_sequence = sequence_color(sequences[0]);
                let alpha_sequence = sequence_color(sequences[1]);
                let expected_primary_mono = subsampling == NativeSubsampling::Cs400;
                assert_eq!(primary_sequence.monochrome, expected_primary_mono);
                assert_eq!(primary_sequence.subsampling_x, expected_primary_mono);
                assert_eq!(primary_sequence.subsampling_y, expected_primary_mono);
                assert_eq!(
                    (
                        primary_sequence.color_primaries,
                        primary_sequence.transfer_characteristics,
                        primary_sequence.matrix_coefficients,
                        primary_sequence.full_range,
                    ),
                    (1, 13, 6, true)
                );
                assert!(alpha_sequence.monochrome);
                assert!(alpha_sequence.full_range);
                assert_eq!(
                    (alpha_sequence.subsampling_x, alpha_sequence.subsampling_y),
                    (true, true)
                );

                let primary_av1c = item_property(&encoded, 1, b"av1C");
                let alpha_av1c = item_property(&encoded, 2, b"av1C");
                for (record, sequence) in [
                    (primary_av1c, &primary_sequence),
                    (alpha_av1c, &alpha_sequence),
                ] {
                    assert_eq!(record.len(), 4);
                    assert_eq!(record[2] & 0x40, 0);
                    assert_eq!(record[2] & 0x20, 0);
                    assert_eq!(record[2] & 0x10 != 0, sequence.monochrome);
                    assert_eq!(record[2] & 0x08 != 0, sequence.subsampling_x);
                    assert_eq!(record[2] & 0x04 != 0, sequence.subsampling_y);
                }
                let primary_color = item_property(&encoded, 1, b"colr");
                assert_eq!(&primary_color[..4], b"nclx");
                assert_eq!(&primary_color[4..], &[0, 1, 0, 13, 0, 6, 0x80]);
                let primary_colors = item_properties(&encoded, 1)
                    .into_iter()
                    .filter(|(kind, _)| kind == b"colr")
                    .map(|(_, payload)| payload)
                    .collect::<Vec<_>>();
                assert_eq!(primary_colors.len(), 2);
                assert_eq!(&primary_colors[1][..4], b"rICC");
                assert_eq!(&primary_colors[1][4..], &[9, 8, 7, 6]);
                assert!(
                    item_properties(&encoded, 2)
                        .iter()
                        .all(|(kind, _)| kind != b"colr")
                );
            }
        }
    }
}

// The AV1 sequence header is the authority for the duplicated av1C fields.
// Exercise legacy routes too: native encoding already normalized monochrome.
#[test]
fn legacy_monochrome_av1c_matches_the_actual_sequence_header() {
    for bits in [8, 10, 12] {
        for grayscale in [false, true] {
            for (quality, quality_alpha) in [(60, 60), (100, 60), (60, 100)] {
                // Legacy grayscale lossless accepts only implicit channel metadata.
                if grayscale && quality == 100 {
                    continue;
                }
                let options = EncoderOptions {
                    quality,
                    quality_alpha,
                    speed: 10,
                    pixel_information: Some(avifenc_rust::PixelInformation {
                        bits_per_channel: vec![bits; 3],
                        extended_channels: None,
                    }),
                    ..Default::default()
                };
                let rgba = (0..4)
                    .flat_map(|i| {
                        let gray = 20 + i * 40;
                        [
                            gray,
                            if grayscale { gray } else { 200 - i * 20 },
                            gray,
                            100 + i * 30,
                        ]
                    })
                    .collect::<Vec<u8>>();
                let encoded = if bits == 8 {
                    avifenc_rust::encode_bytes(
                        &avifenc_rust::ImageBuffer {
                            width: 2,
                            height: 2,
                            rgba,
                        },
                        &options,
                    )
                    .unwrap()
                } else {
                    avifenc_rust::encode_rgba16_bytes(
                        &avifenc_rust::Rgba16ImageBuffer {
                            width: 2,
                            height: 2,
                            rgba: rgba.into_iter().map(|v| u16::from(v) * 257).collect(),
                        },
                        &options,
                    )
                    .unwrap()
                };
                let sequences = sequence_payloads(&encoded);
                assert_eq!(sequences.len(), 2);
                for (i, payload) in sequences.into_iter().enumerate() {
                    let sequence = sequence_color(payload);
                    let record = item_property(&encoded, (i + 1) as u16, b"av1C");
                    assert_eq!(record[2] & 0x10 != 0, sequence.monochrome);
                    assert_eq!(record[2] & 0x08 != 0, sequence.subsampling_x);
                    assert_eq!(
                        record[2] & 0x04 != 0,
                        sequence.subsampling_y,
                        "bits={bits}, grayscale={grayscale}, quality={quality}, alpha={quality_alpha}, item={}",
                        i + 1
                    );
                }
            }
        }
        let mut frame = buffers(NativeSubsampling::Cs400);
        frame.planes.truncate(1);
        let options = EncoderOptions {
            speed: 10,
            pixel_information: Some(avifenc_rust::PixelInformation {
                bits_per_channel: vec![bits],
                extended_channels: None,
            }),
            ..Default::default()
        };
        let encoded = avifenc_rust::encode_frame_bytes(&frame, &options).unwrap();
        let sequence = sequence_color(sequence_payloads(&encoded)[0]);
        let record = item_property(&encoded, 1, b"av1C");
        assert!(sequence.monochrome);
        assert_eq!(record[2] & 0x0c, 0x0c, "legacy native frame bits={bits}");
    }
}
