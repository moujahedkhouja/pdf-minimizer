# pdf-minimizer

Command-line PDF compressor written in Rust. It works on any PDF, but the tuning is aimed at scanned documents, which usually carry oversized images and poorly compressed streams.

There are two modes:

- Lossless (default): re-deflates Flate streams with zlib-ng, or with Zopfli if you pass `--zopfli`, and transcodes existing JPEGs at the DCT-coefficient level through mozjpeg. Pixels stay identical to the input.
- Lossy (`--aggressive`): additionally downsamples large images, re-encodes them with mozjpeg using an automatic quality search, optimizes PNGs with oxipng, and can convert text-only pages to 1-bit CCITT G4 with `--bilevel`. Each lossy candidate has to pass an SSIM similarity check against the original, and it is only kept when it comes out smaller than the original.

## Why This Tool Was Created

I created this tool because I have a very old scanner and the accompanying software isn't the best. I scan directly from the device to my USB stick as PDFs, but unfortunately, the files are much too large and not suitable for sending via email. For a while, I used online tools, but I didn't know what happens to my data and wanted a local, free tool. That's why I wrote this tool and thought maybe someone else could benefit from it too.

## Building

```sh
cargo build --release
```

The binary lands in `target/release/pdf-minimizer`. Requires Rust 1.70 or newer.

## Usage

Compress a single file. The output is written next to the input as `scan_compressed.pdf`:

```sh
pdf-minimizer scan.pdf
```

For typical scans, the preset is the easiest starting point. It downsamples to 120 DPI, caps JPEG quality at 75, and uses Zopfli:

```sh
pdf-minimizer --recommended scan.pdf
```

Tune the lossy pipeline yourself:

```sh
pdf-minimizer --aggressive --downsample-dpi 150 --image-quality 70 --bilevel scan.pdf
```

Process a batch into a separate directory, four files at a time:

```sh
pdf-minimizer --output-dir out --jobs 4 *.pdf
```

Check what a run would save without writing anything:

```sh
pdf-minimizer --dry-run scan.pdf
```

## Options

| Flag | Default | Description |
| --- | --- | --- |
| `-o, --output <FILE>` | | Output path (single input only) |
| `--output-dir <DIR>` | | Output directory for batch mode |
| `--suffix <S>` | `_compressed` | Filename suffix added before the extension |
| `--recommended` | off | Scan preset: 120 DPI, JPEG quality 75, Zopfli |
| `--aggressive` | off | Enable lossy image compression |
| `--max-pixels <N>` | 1500 | Downsample images whose largest dimension exceeds N pixels |
| `--downsample-dpi <DPI>` | | Downsample images above this DPI, assuming an A4 page |
| `--image-quality <1-100>` | 75 | JPEG quality ceiling; lower passing qualities are tried automatically |
| `--smoothing <0-100>` | 0 | JPEG pre-smoothing to suppress scanner grain |
| `--force-jpeg` | off | Convert all images to JPEG regardless of color count |
| `--bilevel` | off | Re-encode text-only pages as 1-bit CCITT G4, gated by an SSIM check |
| `--strip-metadata` | off | Remove author, title, creation date, and creator info |
| `--zopfli` | off | Re-deflate Flate streams with Zopfli (slower, slightly smaller) |
| `--max-decompressed-mib <N>` | 512 | Cap on the decoded size of any single PDF stream |
| `-j, --jobs <N>` | 1 | Number of files processed in parallel |
| `--dry-run` | off | Report estimated savings without writing output |
| `--force` | off | Overwrite existing output files |

The flags under `--aggressive` (`--max-pixels`, `--downsample-dpi`, `--smoothing`, `--force-jpeg`, `--bilevel`) have no effect without it.

## Behavior notes

- Output is written to a temporary file and renamed into place, so an interrupted run never leaves a half-written PDF.
- The decompression cap (`--max-decompressed-mib`) exists to guard against decompression bombs in untrusted input.
- Parallelism defaults to one file at a time because decoded images can use a lot of memory. Raise `--jobs` if you have RAM to spare.

## Exit codes

`0` when every input succeeded, `1` when some inputs failed, `2` when all of them failed.

## License

Licensed under either of the [Apache License 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
