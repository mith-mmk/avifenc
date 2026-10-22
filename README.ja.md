# avifenc-rust

Pure Rust AVIF エンコーダーです。

公開済み版は `avifenc-rust 0.0.7` です。このブランチは共有バッファ対応の
未公開候補 `0.1.0` を準備しています。依存指定と公開順は
[SHARED_BUFFER.md](SHARED_BUFFER.md)、API の説明は [README.md](README.md)
を参照してください。

## 任意の SIMD 対応

このブランチでは `--features asm` で rav1e の既存 native SIMD を有効にできます。
既定は従来どおり Pure Rust です。x86_64 では NASM 2.15 以上と native toolchain
が必要です（Linux x86_64、NASM 2.16.03 で確認）。画質・探索 preset は変えません。

実測した6種類では、符号化バイト列の一致を保ち、処理時間が約41～52%短くなりました。
一方、実行ファイルは2.41→4.16 MBに増え、ピークRSSも増加しています。
再現手順、8/10/12-bit比較、対象CPUと制約は [PERFORMANCE.md](PERFORMANCE.md)
を参照してください。`multithread` と併用できますが、実測値はSIMD単独の比較です。
