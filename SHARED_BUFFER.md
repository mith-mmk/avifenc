# Shared-buffer encode integration

`avifenc_rust::buffer` reexports the initial `avif-image-buffer` package (0.1.0 candidate, not yet published)
from avif-rust Git commit `5c73c7f26c276098ddd761fb20b956302a462001`.
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


Packaging follow-up: this branch prepares avifenc-rust 0.1.0, since 0.0.7
is already published without the shared API. The shared dependency includes
`version = "0.1.0"` alongside the immutable Git revision. Cargo removes Git
coordinates from the packaged manifest and resolves that version from crates.io.
Do not mix a registry decoder and Git encoder without a consistent top-level
shared-package patch: the two sources have distinct Rust type identities.

The required publication order is avif-image-buffer 0.1.0, avifenc-rust 0.1.0,
then avif-rust 0.1.0. No package was uploaded, no credentials were created and
no release/tag was published. A real registry package verification of this crate
requires the shared package to be published first; Git tests do not establish
that registry publication has happened.

Local packaging verification passed using the already packaged shared 0.1.0
archive through a command-scoped crates.io path patch. The resulting encoder
archive was built by Cargo after normalization. This is a local artifact check,
not a successful crates.io dependency resolution or publication. The normal
registry package command still rejects the unpublished shared dependency.

## Backend validation before sample copies

Shared preflight and native encode use the same backend validation helpers.
The rav1e configuration builder applies advanced values, types and cross-option
constraints and calls its pure configuration validator before any context or
frame is created. The lossless path uses its existing level table for width,
height and picture size. Primary and alpha backend choices are checked
independently; the lossless level limit is not imposed on lossy-only images.
Invalid settings now fail before adapter sample copies. Unit tests count sample
copies and backend entries to verify this ordering, in addition to testing
advanced-option failures, valid tunes and backend dimension boundaries.
