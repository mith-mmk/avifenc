# Shared-buffer encode integration

`avifenc_rust::buffer` reexports the unpublished `avif-image-buffer` package
from avif-rust Git commit `9ed4ad242582c3375c8d8ee1365ef7d6ddc5b53a`.
This selects the buffer package only, not the decoder crate. The encoder must
not depend on avif-rust: optional avif encode delegation points in this direction.
No package/repository publication is included.

`ImageBuffer`, `Rgba16ImageBuffer` and `FrameBuffers` keep their public fields,
signatures and old encode entry points. Additive `view_ex` methods validate
their current fields and borrow with explicit source color/depth/native roles.
RGBA16 remains full-range16-bit, regardless of sample maxima.

`preflight_ex(view, EncodeOptionsEx)` checks storage, native roles/subsampling,
depth, alpha association, source/output CICP/ICC and backend controls before
compact sample allocation or backend entry. New coded depth is explicit8/10/12.
Native depth/range/chroma/alpha conversions are not inferred. Packed precision
uses Exact or explicitly selected Nearest tie rules; metadata permissions are
per CICP/ICC field and perform no ICC/color transform. Premultiplied alpha rejects.

`encode_ex` delegates to `encode_native_bytes` and returns complete bytes plus
a report. Preflight reports have `encoded=false`; only complete encode success
sets it true. The adapter currently copies logical sample values once into
compact native vectors; padding is excluded. This is not a whole-codec zero-copy
claim. AVIF document context and preservation decisions belong to the caller's
AVIF-specific planning layer, not this pixel API.

The native writer's alpha `auxl` direction is corrected from primary->alpha to
alpha->primary. This intentionally changes those two item-reference bytes in
legacy encoded outputs too; old public APIs and sample conversion policies are
unchanged. Container regression expectations now require the standard direction.

Verification checkpoint (Rust/Cargo1.91.0, Linux, offline after Git source fetch):

```sh
cargo test --locked --offline --release --lib --tests
cargo clippy --locked --offline --all-targets
cargo fmt --all -- --check
```

The native/legacy/FFmpeg/shared targets report90 passed,0 failed. Five new shared
test groups include30 native combinations (8/10/12 x400/GBR/444/422/420 x alpha
absent/present), byte equality with native encoding, explicit packed16->12 policy,
unchanged inputs, metadata permissions and preflight failures. Ordinary Clippy
passes with three inherited `collapsible_else_if` warnings; `-Dwarnings` fails
on those old blocks. Full cross-crate preservation, independent round-trip pixels,
WASM and isolated performance are verified at the subsequent avif integration
checkpoint, not claimed by these encoder-only pass counts.

WML2 PR13, JPEG2000, release/version numbers and codec defaults are unchanged.
