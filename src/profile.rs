#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Frame,
    PartitionRdo,
    PartitionMeasure,
    BlockMeasure,
    Snapshot,
    Restore,
    BlockEncode,
    ModeSearch,
    CdfRollback,
    RgbaToYuv,
    Rav1eEncode,
    Container,
    ModeCost,
    CflSearch,
    SkipCheck,
    Transform,
    CoeffCoding,
    Rav1eValidation,
    Rav1eConfig,
    Rav1eContext,
    Rav1eFrame,
    Rav1eCopy,
    Rav1eSubmit,
    Rav1ePackets,
    Rav1eBase,
    Rav1eComplete,
}

#[cfg(feature = "profile")]
mod enabled {
    use super::Kind;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static NANOSECONDS: [AtomicU64; 26] = [const { AtomicU64::new(0) }; 26];
    static CALLS: [AtomicU64; 26] = [const { AtomicU64::new(0) }; 26];

    fn index(kind: Kind) -> usize {
        match kind {
            Kind::Frame => 0,
            Kind::PartitionRdo => 1,
            Kind::PartitionMeasure => 2,
            Kind::BlockMeasure => 3,
            Kind::Snapshot => 4,
            Kind::Restore => 5,
            Kind::BlockEncode => 6,
            Kind::ModeSearch => 7,
            Kind::CdfRollback => 8,
            Kind::RgbaToYuv => 9,
            Kind::Rav1eEncode => 10,
            Kind::Container => 11,
            Kind::ModeCost => 12,
            Kind::CflSearch => 13,
            Kind::SkipCheck => 14,
            Kind::Transform => 15,
            Kind::CoeffCoding => 16,
            Kind::Rav1eValidation => 17,
            Kind::Rav1eConfig => 18,
            Kind::Rav1eContext => 19,
            Kind::Rav1eFrame => 20,
            Kind::Rav1eCopy => 21,
            Kind::Rav1eSubmit => 22,
            Kind::Rav1ePackets => 23,
            Kind::Rav1eBase => 24,
            Kind::Rav1eComplete => 25,
        }
    }

    pub(crate) struct Guard {
        kind: Kind,
        started: Instant,
    }

    impl Guard {
        #[inline(always)]
        pub(crate) fn new(kind: Kind) -> Self {
            Self {
                kind,
                started: Instant::now(),
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let index = index(self.kind);
            NANOSECONDS[index]
                .fetch_add(self.started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            CALLS[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn report() {
        let names = [
            "frame",
            "partition_rdo",
            "partition_measure",
            "block_measure",
            "snapshot",
            "restore",
            "block_encode",
            "mode_search",
            "cdf_rollback",
            "rgba_to_yuv",
            "rav1e_encode",
            "container",
            "mode_cost",
            "cfl_search",
            "skip_check",
            "transform",
            "coeff_coding",
            "rav1e_validation",
            "rav1e_config",
            "rav1e_context",
            "rav1e_frame",
            "rav1e_copy_pad",
            "rav1e_submit_flush",
            "rav1e_receive_packets",
            "rav1e_base",
            "rav1e_complete",
        ];
        eprintln!("avifenc profile");
        for (index, name) in names.into_iter().enumerate() {
            let calls = CALLS[index].swap(0, Ordering::Relaxed);
            let nanoseconds = NANOSECONDS[index].swap(0, Ordering::Relaxed);
            if calls > 0 {
                eprintln!(
                    "  {name}: calls={calls} ms={:.3}",
                    nanoseconds as f64 / 1_000_000.0
                );
            }
        }
    }
}

#[cfg(not(feature = "profile"))]
mod enabled {
    use super::Kind;

    pub(crate) struct Guard;

    impl Guard {
        #[inline(always)]
        pub(crate) fn new(_kind: Kind) -> Self {
            Self
        }
    }

    pub(crate) fn report() {}
}

pub(crate) use enabled::Guard;
pub(crate) use enabled::report;
