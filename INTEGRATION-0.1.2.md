# Encoder integration for 0.1.2

PR #3 head `0cf20a2298061d76ec02e44765553298e836090e` was reviewed and
merged as `a258c33d41372810e0c7041710bd588e18df592f` before integrating
PR #2 head `95b33814b71e816cfb06dd425dfe371fb302d9f6`. The normal merge
retains rectangular partition/palette syntax fixes, common monochrome header
normalization, opt-in SIMD and direct RGBA8/RGBA16 lossless staging.

The only source-tree conflict was `.gitattributes`; both AVIF/RGBA and OBU
binary rules are retained. `lossless.yml` now installs NASM because its
all-features builds include the newly integrated `asm` feature. Both CI
workflows remain enabled with their mandatory regression steps.

Local locked release suites pass **100 tests each** with default, asm and
all features, no failures or ignored tests. `AVIF_REQUIRE_FFMPEG=1` was set;
FFmpeg 7.1.5, libaom 3.12.1 and dav1d 1.5.1 provide independent references.
Coverage includes forced HORZ/VERT leaves at 8/10/12-bit with screen tools
on/off, exact native samples from both decoders, monochrome av1C/sequence
headers, frozen SIMD/RGBA16 byte oracles and old/new staging differential
checks. Existing oracle bytes are unchanged. Format passes.

Historical performance numbers remain tied to their original source commits;
this integration makes no new timing/RSS claim. No private input samples are
added. GitHub Actions results for the merged candidate are linked in PR #2.
Additional hardware testing is not required for this release.
