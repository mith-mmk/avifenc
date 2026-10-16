use avifenc_rust::{buffer::*, *};

fn options(bits: u8, model: NativeModel) -> EncodeOptionsEx {
    let matrix = if model == NativeModel::IdentityGbr {
        0
    } else {
        6
    };
    EncodeOptionsEx {
        native: NativeEncodeOptions {
            bit_depth: bits,
            subsampling: match model {
                NativeModel::Monochrome => NativeSubsampling::Cs400,
                NativeModel::Yuv420 => NativeSubsampling::Cs420,
                NativeModel::Yuv422 => NativeSubsampling::Cs422,
                _ => NativeSubsampling::Cs444,
            },
            encoder: EncoderOptions {
                lossless: true,
                color_information: None,
                ..Default::default()
            },
            color_information: NativeColorInformation::from_nclx(NclxColorInformation {
                color_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients: matrix,
                full_range_flag: true,
            }),
        },
        precision: PrecisionPolicy::Exact,
        color: ColorPolicyEx::Preserve,
    }
}
fn color(model: NativeModel) -> ColorMetadataRef<'static> {
    ColorMetadataRef {
        icc: None,
        cicp: Some(Cicp {
            primaries: 1,
            transfer: 13,
            matrix: if model == NativeModel::IdentityGbr {
                0
            } else {
                6
            },
        }),
    }
}
fn native(bits: u8, model: NativeModel, alpha: bool) -> FrameBuffers {
    let (w, h) = (5usize, 3usize);
    let mut planes = Vec::new();
    for role in 0..4usize {
        if (role == 3 && !alpha) || ((role == 1 || role == 2) && model == NativeModel::Monochrome) {
            continue;
        }
        let (sx, sy) = if role == 1 || role == 2 {
            model.chroma_subsampling().unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        let (pw, ph) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
        let max = sample_max(bits).unwrap();
        planes.push(PlaneBuffer {
            layout: PlaneLayout {
                plane: role as u8,
                width: pw,
                height: ph,
                subsampling_x: sx as u8,
                subsampling_y: sy as u8,
                sample_count: pw * ph,
            },
            samples: (0..pw * ph)
                .map(|i| {
                    if role == 3 && i % 3 == 0 {
                        0
                    } else {
                        ((i * 37 + role * 113) as u32 % (max + 1)) as u16
                    }
                })
                .collect(),
        });
    }
    FrameBuffers {
        width: w,
        height: h,
        planes,
    }
}
fn descriptor(model: NativeModel, alpha: bool) -> NativeDescriptor {
    NativeDescriptor {
        width: 5,
        height: 3,
        model,
        alpha: if alpha {
            AlphaMode::Straight
        } else {
            AlphaMode::Absent
        },
        range: Some(ColorRange::Full),
        chroma_location: None,
    }
}

#[test]
fn native_shared_route_matches_legacy_native_bytes_for_all_depths_and_roles() {
    for bits in [8, 10, 12] {
        for model in [
            NativeModel::Monochrome,
            NativeModel::IdentityGbr,
            NativeModel::Yuv444,
            NativeModel::Yuv422,
            NativeModel::Yuv420,
        ] {
            for alpha in [false, true] {
                let buffers = native(bits, model, alpha);
                let original = buffers.clone();
                let opts = options(bits, model);
                let view = buffers
                    .view_ex(descriptor(model, alpha), bits, color(model))
                    .unwrap();
                let preview = preflight_ex(view, &opts).unwrap();
                assert!(!preview.encoded);
                assert!(preview.conversions.is_empty());
                let out = encode_ex(view, &opts).unwrap();
                assert!(out.report.encoded);
                assert_eq!(out.report.input_depth, bits);
                assert_eq!(out.report.output_depth, bits);
                assert_eq!(
                    out.report.adapter_sample_copies,
                    buffers.planes.iter().map(|p| p.samples.len()).sum()
                );
                assert_eq!(
                    out.bytes,
                    encode_native_bytes(&buffers, &opts.native).unwrap()
                );
                assert_eq!(buffers, original);
                if alpha {
                    assert!(
                        out.bytes
                            .windows(14)
                            .any(|b| b == b"\0\0\0\x0eauxl\0\x02\0\x01\0\x01")
                    );
                }
            }
        }
    }
}

#[test]
fn packed_storage16_requires_exact_or_explicit_nearest_and_never_changes_input() {
    let image = Rgba16ImageBuffer {
        width: 1,
        height: 1,
        rgba: vec![1, 32768, 65535, 0],
    };
    let raw = image.clone();
    let view = image.view_ex(color(NativeModel::IdentityGbr)).unwrap();
    let mut opts = options(12, NativeModel::IdentityGbr);
    assert!(matches!(
        preflight_ex(view, &opts),
        Err(EncodeErrorEx::Buffer(BufferError::PrecisionLoss { .. }))
    ));
    opts.precision = PrecisionPolicy::Nearest {
        ties: TieRule::AwayFromZero,
    };
    let result = encode_ex(view, &opts).unwrap();
    assert_eq!(image, raw);
    assert_eq!(
        result.report.conversions,
        [ConversionEx::PackedPrecision {
            from: 16,
            to: 12,
            policy: opts.precision
        }]
    );
    opts.native.bit_depth = 16;
    assert!(matches!(
        preflight_ex(view, &opts),
        Err(EncodeErrorEx::Unsupported(_))
    ));
}

#[test]
fn premultiplied_alpha_native_precision_and_color_changes_are_not_implicit() {
    let source = native(10, NativeModel::Yuv420, true);
    let image = source
        .view_ex(
            descriptor(NativeModel::Yuv420, true),
            10,
            color(NativeModel::Yuv420),
        )
        .unwrap();
    let mut opts = options(12, NativeModel::Yuv420);
    assert!(matches!(
        preflight_ex(image, &opts),
        Err(EncodeErrorEx::ConversionRequired(_))
    ));
    opts.native.bit_depth = 10;
    let ImageViewEx::NativePlanar(mut p) = image else {
        panic!()
    };
    p.descriptor.alpha = AlphaMode::Premultiplied(AlphaDomain::Unknown);
    assert!(matches!(
        preflight_ex(ImageViewEx::NativePlanar(p), &opts),
        Err(EncodeErrorEx::ConversionRequired(_))
    ));
    p.descriptor.alpha = AlphaMode::Straight;
    p.descriptor.chroma_location = Some(1);
    assert!(matches!(
        preflight_ex(ImageViewEx::NativePlanar(p), &opts),
        Err(EncodeErrorEx::Unsupported(_))
    ));
    opts.native.subsampling = NativeSubsampling::Cs444;
    assert!(matches!(
        preflight_ex(image, &opts),
        Err(EncodeErrorEx::ConversionRequired(_))
    ));
}

#[test]
fn metadata_changes_need_individual_permissions_and_are_reported() {
    let image = ImageBuffer {
        width: 1,
        height: 1,
        rgba: vec![1, 2, 3, 0],
    };
    let icc = [1, 2, 3];
    let mut source = color(NativeModel::IdentityGbr);
    source.icc = Some(&icc);
    let view = image.view_ex(source).unwrap();
    let mut opts = options(8, NativeModel::IdentityGbr);
    assert!(matches!(
        preflight_ex(view, &opts),
        Err(EncodeErrorEx::ColorConflict(_))
    ));
    opts.color = ColorPolicyEx::Explicit {
        allow_cicp_change: true,
        allow_icc_change: false,
    };
    assert!(preflight_ex(view, &opts).is_err());
    opts.color = ColorPolicyEx::Explicit {
        allow_cicp_change: false,
        allow_icc_change: true,
    };
    let report = encode_ex(view, &opts).unwrap().report;
    assert_eq!(report.conversions, [ConversionEx::IccSignalling]);
    assert!(!report.output_icc);
    opts.color = ColorPolicyEx::Preserve;
    opts.native.color_information.icc_profile = Some(icc.to_vec());
    opts.native.color_information.icc_color_type = Some(*b"rICC");
    let result = encode_ex(view, &opts).unwrap();
    assert!(result.report.output_icc);
    assert!(result.report.conversions.is_empty());
}

#[test]
fn short_layout_and_backend_controls_fail_during_preflight() {
    let image = ImageBuffer {
        width: 1,
        height: 1,
        rgba: vec![1, 2, 3, 4],
    };
    let view = image.view_ex(color(NativeModel::IdentityGbr)).unwrap();
    let mut opts = options(8, NativeModel::IdentityGbr);
    opts.native.encoder.jobs = 1;
    assert!(matches!(
        preflight_ex(view, &opts),
        Err(EncodeErrorEx::Codec(EncoderError::Unsupported(_)))
    ));
    let ImageViewEx::Packed(mut p) = view else {
        panic!()
    };
    p.plane.layout.row_stride_bytes = 3;
    assert!(matches!(
        preflight_ex(ImageViewEx::Packed(p), &opts),
        Err(EncodeErrorEx::Buffer(_))
    ));
}
