use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;
struct Counter;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
fn record(size: usize) {
    if ACTIVE.load(Relaxed) {
        COUNT.fetch_add(1, Relaxed);
        BYTES.fetch_add(size as u64, Relaxed);
    }
}
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counter = Counter;
fn hash(values: impl IntoIterator<Item = u8>) -> u64 {
    values.into_iter().fold(14695981039346656037, |h, v| {
        (h ^ u64::from(v)).wrapping_mul(1099511628211)
    })
}
fn measure<T>(iterations: usize, mut run: impl FnMut() -> T, checksum: impl Fn(&T) -> u64) {
    assert!(iterations >= 3);
    let expected = checksum(&run()); // warm-up, outside measurement
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let result = std::hint::black_box(run());
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(checksum(&result), expected);
        drop(result); // output destruction and checksum excluded from timings
    }
    COUNT.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
    let result = run();
    ACTIVE.store(false, Relaxed);
    let allocations = COUNT.load(Relaxed);
    let bytes = BYTES.load(Relaxed);
    assert_eq!(checksum(&result), expected);
    times.sort_by(f64::total_cmp);
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .map(str::to_owned)
        });
    println!(
        "iterations={iterations} median_ms={:.6} min_ms={:.6} max_ms={:.6} allocations={allocations} requested_bytes={bytes} checksum={expected:016x} peak_rss={}",
        times[iterations / 2],
        times[0],
        times[iterations - 1],
        rss.as_deref().unwrap_or("unavailable")
    );
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let width: usize = args[1].parse().unwrap();
    let height: usize = args[2].parse().unwrap();
    let quality: u8 = args[3].parse().unwrap();
    let alpha = args[4] == "alpha";
    let iterations = args.get(5).map_or(7, |s| s.parse().unwrap());
    let bit_depth: u8 = args.get(6).map_or(8, |s| s.parse().unwrap());
    let speed: u8 = args.get(7).map_or(10, |s| s.parse().unwrap());
    assert!(matches!(bit_depth, 8 | 10 | 12));
    let generated = (0..width * height)
        .flat_map(|i| {
            let x = i % width;
            let y = i / width;
            [
                ((x * 7 + y * 3) % 256) as u8,
                ((x * 2 + y * 11) % 256) as u8,
                ((x ^ y) % 256) as u8,
                if alpha {
                    ((x * 5 + y * 7) % 256) as u8
                } else {
                    255
                },
            ]
        })
        .collect();
    let input = std::env::var("AVIF_PERF_INPUT").ok();
    let rgba = input
        .as_ref()
        .map_or_else(|| generated, |path| std::fs::read(path).unwrap());
    assert_eq!(rgba.len(), width * height * 4);
    let image = avifenc_rust::ImageBuffer {
        width,
        height,
        rgba,
    };
    let options = avifenc_rust::EncoderOptions {
        quality,
        quality_alpha: quality,
        speed,
        pixel_information: (bit_depth != 8).then(|| avifenc_rust::PixelInformation {
            bits_per_channel: vec![bit_depth; 3],
            extended_channels: None,
        }),
        jobs: 0,
        ..Default::default()
    };
    println!(
        "dimensions={width}x{height} bit_depth={bit_depth} quality={quality} quality_alpha={quality} speed={speed} jobs=0 alpha={alpha} input={}",
        input.as_deref().unwrap_or("procedural")
    );
    if bit_depth == 8 {
        measure(
            iterations,
            || avifenc_rust::encode_bytes(&image, &options).unwrap(),
            |b| hash(b.iter().copied()),
        );
    } else {
        let image16 = avifenc_rust::Rgba16ImageBuffer {
            width,
            height,
            rgba: image.rgba.iter().map(|&v| u16::from(v) * 257).collect(),
        };
        measure(
            iterations,
            || avifenc_rust::encode_rgba16_bytes(&image16, &options).unwrap(),
            |b| hash(b.iter().copied()),
        );
    }
}
