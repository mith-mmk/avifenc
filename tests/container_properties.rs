use avifenc_rust::{
    ColorInformation, EncoderOptions, FrameBuffers, NativeColorInformation, NativeEncodeOptions,
    NativeSubsampling, PlaneBuffer, PlaneLayout,
};

fn native_buffers(with_alpha: bool) -> FrameBuffers {
    let mut planes = (0..3)
        .map(|plane| PlaneBuffer {
            layout: PlaneLayout {
                plane,
                width: 2,
                height: 2,
                subsampling_x: 0,
                subsampling_y: 0,
                sample_count: 4,
            },
            samples: vec![32 + u16::from(plane); 4],
        })
        .collect::<Vec<_>>();
    if with_alpha {
        planes.push(PlaneBuffer {
            layout: PlaneLayout {
                plane: 3,
                width: 2,
                height: 2,
                subsampling_x: 0,
                subsampling_y: 0,
                sample_count: 4,
            },
            samples: vec![128; 4],
        });
    }
    FrameBuffers {
        width: 2,
        height: 2,
        planes,
    }
}

fn native_options(color_information: NativeColorInformation) -> NativeEncodeOptions {
    NativeEncodeOptions {
        bit_depth: 8,
        subsampling: NativeSubsampling::Cs444,
        encoder: EncoderOptions {
            quality: 80,
            quality_alpha: 80,
            color_information: None,
            ..EncoderOptions::default()
        },
        color_information,
    }
}

fn box_at(data: &[u8], offset: usize) -> (usize, [u8; 4], &[u8]) {
    assert!(offset <= data.len());
    assert!(data.len() - offset >= 8, "truncated box header at {offset}");
    let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    assert!(size >= 8, "invalid box size {size} at {offset}");
    assert!(
        size <= data.len() - offset,
        "box exceeds containing payload"
    );
    let kind = data[offset + 4..offset + 8].try_into().unwrap();
    (size, kind, &data[offset + 8..offset + size])
}

fn child_boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let (size, kind, payload) = box_at(data, offset);
        result.push((kind, payload));
        offset += size;
    }
    assert_eq!(offset, data.len(), "child parser did not reach EOF");
    result
}

fn top_level_boxes(data: &[u8]) -> Vec<(usize, [u8; 4], &[u8])> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let (size, kind, payload) = box_at(data, offset);
        result.push((offset, kind, payload));
        offset += size;
    }
    assert_eq!(offset, data.len(), "top-level parser did not reach EOF");
    result
}

fn find_child<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> &'a [u8] {
    let matches = boxes
        .iter()
        .filter(|(actual, _)| actual == kind)
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected one {kind:?} child");
    matches[0].1
}

fn meta_payload(data: &[u8]) -> &[u8] {
    let boxes = top_level_boxes(data);
    let matches = boxes
        .iter()
        .filter(|(_, kind, _)| kind == b"meta")
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected one top-level meta box");
    matches[0].2
}

fn read16(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn read32(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn ipco_properties(encoded: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let meta_children = child_boxes(&meta_payload(encoded)[4..]);
    let iprp_payload = find_child(&meta_children, b"iprp");
    let iprp_children = child_boxes(iprp_payload);
    let ipco_payload = find_child(&iprp_children, b"ipco");
    child_boxes(ipco_payload)
}

fn property_types(encoded: &[u8]) -> Vec<[u8; 4]> {
    ipco_properties(encoded)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

fn associations(encoded: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let meta_children = child_boxes(&meta_payload(encoded)[4..]);
    let iprp_payload = find_child(&meta_children, b"iprp");
    let iprp_children = child_boxes(iprp_payload);
    let ipma_payload = find_child(&iprp_children, b"ipma");
    assert!(ipma_payload.len() >= 8);
    let count = read32(ipma_payload, 4) as usize;
    let mut offset = 8;
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        assert!(offset + 3 <= ipma_payload.len());
        let item_id = read16(ipma_payload, offset);
        let association_count = ipma_payload[offset + 2] as usize;
        let start = offset + 3;
        assert!(start + association_count <= ipma_payload.len());
        result.push((
            item_id,
            ipma_payload[start..start + association_count].to_vec(),
        ));
        offset = start + association_count;
    }
    assert_eq!(offset, ipma_payload.len(), "ipma parser did not reach EOF");
    result
}

fn color_information(nclx: bool, icc: bool) -> NativeColorInformation {
    NativeColorInformation {
        nclx: nclx.then_some(avifenc_rust::NclxColorInformation {
            color_primaries: 1,
            transfer_characteristics: 13,
            matrix_coefficients: 6,
            full_range_flag: true,
        }),
        icc_profile: icc.then_some(vec![1, 2, 3, 4]),
        icc_color_type: icc.then_some(*b"prof"),
    }
}

#[test]
fn native_color_properties_keep_primary_and_alpha_indices_aligned() {
    for (nclx, icc) in [(false, false), (true, false), (false, true), (true, true)] {
        let expected_color_types = if nclx && icc {
            vec![*b"nclx", *b"prof"]
        } else if nclx {
            vec![*b"nclx"]
        } else if icc {
            vec![*b"prof"]
        } else {
            vec![*b"nclx"]
        };
        for with_alpha in [false, true] {
            if !nclx {
                let result = avifenc_rust::encode_native_bytes(
                    &native_buffers(with_alpha),
                    &native_options(color_information(nclx, icc)),
                );
                assert!(result.is_err(), "native CICP omission must be rejected");
                continue;
            }
            let output = avifenc_rust::encode_native_bytes(
                &native_buffers(with_alpha),
                &native_options(color_information(nclx, icc)),
            )
            .unwrap();
            let types = property_types(&output);
            let mut expected_types = vec![*b"ispe", *b"pixi"];
            expected_types.extend(std::iter::repeat_n(*b"colr", expected_color_types.len()));
            expected_types.push(*b"av1C");
            if with_alpha {
                expected_types.extend([*b"pixi", *b"av1C", *b"auxC"]);
            }
            assert_eq!(
                types, expected_types,
                "nclx={nclx}, icc={icc}, alpha={with_alpha}"
            );
            let actual_color_types = ipco_properties(&output)
                .into_iter()
                .filter(|(kind, _)| kind == b"colr")
                .map(|(_, payload)| payload[..4].try_into().unwrap())
                .collect::<Vec<[u8; 4]>>();
            assert_eq!(actual_color_types, expected_color_types);

            let entries = associations(&output);
            assert_eq!(entries[0].0, 1);
            let primary_count = 3 + expected_color_types.len();
            let primary = entries[0].1.as_slice();
            assert_eq!(primary.len(), primary_count);
            assert!(primary.iter().all(|association| association & 0x80 != 0));
            assert_eq!(
                primary
                    .iter()
                    .map(|association| association & 0x7f)
                    .collect::<Vec<_>>(),
                (1..=primary_count as u8).collect::<Vec<_>>()
            );
            if with_alpha {
                assert_eq!(entries[1].0, 2);
                let alpha = &entries[1].1;
                assert_eq!(
                    alpha,
                    &[
                        0x81,
                        (primary_count as u8 + 1) | 0x80,
                        (primary_count as u8 + 2) | 0x80,
                        (primary_count as u8 + 3) | 0x80
                    ]
                );
            }
        }
    }
}

#[test]
fn native_property_graph_keeps_item_ids_references_and_extents_consistent() {
    for (nclx, icc) in [(false, false), (true, false), (false, true), (true, true)] {
        for with_alpha in [false, true] {
            if !nclx {
                let result = avifenc_rust::encode_native_bytes(
                    &native_buffers(with_alpha),
                    &native_options(color_information(nclx, icc)),
                );
                assert!(result.is_err(), "native CICP omission must be rejected");
                continue;
            }
            let output = avifenc_rust::encode_native_bytes(
                &native_buffers(with_alpha),
                &native_options(color_information(nclx, icc)),
            )
            .unwrap();
            let top = top_level_boxes(&output);
            let meta = meta_payload(&output);
            assert_eq!(&meta[..4], &[0, 0, 0, 0]);
            let meta_children = child_boxes(&meta[4..]);

            let iinf = find_child(&meta_children, b"iinf");
            assert!(iinf.len() >= 6);
            let expected_item_count = if with_alpha { 2 } else { 1 };
            assert_eq!(read16(iinf, 4), expected_item_count);
            let info_entries = child_boxes(&iinf[6..]);
            assert_eq!(info_entries.len(), usize::from(expected_item_count));
            let item_ids = info_entries
                .iter()
                .map(|(kind, payload)| {
                    assert_eq!(kind, b"infe");
                    assert!(payload.len() >= 6);
                    read16(payload, 4)
                })
                .collect::<Vec<_>>();
            assert_eq!(item_ids, if with_alpha { vec![1, 2] } else { vec![1] });

            let references = meta_children
                .iter()
                .filter(|(kind, _)| kind == b"iref")
                .collect::<Vec<_>>();
            assert_eq!(references.len(), usize::from(with_alpha));
            if with_alpha {
                let iref = references[0].1;
                assert!(iref.len() >= 4);
                let reference_entries = child_boxes(&iref[4..]);
                assert_eq!(reference_entries.len(), 1);
                assert_eq!(reference_entries[0].0, *b"auxl");
                // Legacy writer direction is intentionally characterized as 1 -> 2.
                assert_eq!(reference_entries[0].1, &[0, 1, 0, 1, 0, 2]);
                assert!(item_ids.contains(&read16(reference_entries[0].1, 0)));
                assert!(item_ids.contains(&read16(reference_entries[0].1, 4)));
            }

            let iloc = find_child(&meta_children, b"iloc");
            assert!(iloc.len() >= 8);
            assert_eq!(&iloc[..6], &[0, 0, 0, 0, 0x44, 0]);
            assert_eq!(read16(iloc, 6) as usize, item_ids.len());
            assert_eq!(iloc.len(), 8 + item_ids.len() * 14);
            let mut extents = Vec::with_capacity(item_ids.len());
            for (index, item_id) in item_ids.iter().copied().enumerate() {
                let entry = 8 + index * 14;
                assert_eq!(read16(iloc, entry), item_id);
                assert_eq!(read16(iloc, entry + 2), 0);
                assert_eq!(read16(iloc, entry + 4), 1);
                extents.push((
                    item_id,
                    read32(iloc, entry + 6) as usize,
                    read32(iloc, entry + 10) as usize,
                ));
            }

            let (mdat_start, _, mdat_payload) = top
                .iter()
                .find(|(_, kind, _)| kind == b"mdat")
                .copied()
                .expect("mdat");
            let mdat_payload_start = mdat_start + 8;
            let mdat_payload_end = mdat_payload_start + mdat_payload.len();
            assert_eq!(extents[0].1, mdat_payload_start);
            for extent in &extents {
                assert!(extent.1 >= mdat_payload_start);
                assert!(extent.1 <= mdat_payload_end);
                assert!(extent.2 <= mdat_payload_end - extent.1);
            }
            for pair in extents.windows(2) {
                assert_eq!(pair[1].1, pair[0].1 + pair[0].2);
            }
            let last = extents.last().copied().expect("at least primary extent");
            assert_eq!(last.1 + last.2, mdat_payload_end);

            let iprp = child_boxes(find_child(&meta_children, b"iprp"));
            let properties = child_boxes(find_child(&iprp, b"ipco"));
            let ipma = find_child(&iprp, b"ipma");
            assert!(ipma.len() >= 8);
            assert_eq!(&ipma[..4], &[0, 0, 0, 0]);
            assert_eq!(read32(ipma, 4) as usize, item_ids.len());
            let color_count = if nclx && icc { 2 } else { 1 };
            let mut cursor = 8;
            for item_id in item_ids {
                assert_eq!(read16(ipma, cursor), item_id);
                let association_count = ipma[cursor + 2] as usize;
                cursor += 3;
                let expected = if item_id == 1 { 3 + color_count } else { 4 };
                assert_eq!(association_count, expected);
                assert!(cursor + association_count <= ipma.len());
                for association in &ipma[cursor..cursor + association_count] {
                    assert_ne!(association & 0x80, 0);
                    let property_index = usize::from(association & 0x7f);
                    assert!(property_index > 0 && property_index <= properties.len());
                }
                cursor += association_count;
            }
            assert_eq!(cursor, ipma.len(), "ipma parser did not reach EOF");
        }
    }
}

#[test]
fn color_information_type_helpers_remain_compatible() {
    let icc = ColorInformation {
        color_type: *b"prof",
        payload: vec![1, 2, 3],
    };
    assert_eq!(icc.icc_profile(), Some(&[1, 2, 3][..]));
}
