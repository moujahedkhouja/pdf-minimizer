use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use pdf_minimizer::compressor::{compress_file, CompressOptions};
use pdf_minimizer::stats::{BatchStats, FileStats};
use rayon::prelude::*;
use std::process;
use std::sync::Mutex;

#[derive(Parser, Debug)]
#[command(name = "pdf-minimizer", version, about = "Compress PDF files")]
pub struct Cli {
    /// Input PDF file(s)
    #[arg(required = true)]
    pub input: Vec<String>,

    /// Output path (single file only)
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<String>,

    /// Output directory for batch mode
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<String>,

    /// Filename suffix added before extension
    #[arg(long, default_value = "_compressed")]
    pub suffix: String,

    /// Enable lossy compression (image downsampling)
    #[arg(long, conflicts_with = "recommended")]
    pub aggressive: bool,

    /// Use the recommended scan preset: 120 DPI, JPEG quality 75, and Zopfli
    #[arg(
        long,
        conflicts_with_all = [
            "aggressive",
            "max_pixels",
            "image_quality",
            "smoothing",
            "downsample_dpi",
            "force_jpeg",
            "bilevel",
            "zopfli"
        ]
    )]
    pub recommended: bool,

    /// Downsample images whose largest dimension exceeds N pixels (only meaningful with --aggressive)
    #[arg(long, default_value_t = 1500)]
    pub max_pixels: u32,

    /// JPEG quality ceiling, 1–100; lower passing qualities are tried automatically
    #[arg(long, default_value_t = 75, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub image_quality: u8,

    /// JPEG pre-smoothing factor 1–100 to suppress scanner grain; 0 disables (only meaningful with --aggressive)
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub smoothing: u8,

    /// Downsample images exceeding this DPI, assuming a standard A4 page (only meaningful with --aggressive)
    #[arg(long, value_name = "DPI")]
    pub downsample_dpi: Option<u16>,

    /// Force all images to JPEG regardless of color count (only meaningful with --aggressive)
    #[arg(long)]
    pub force_jpeg: bool,

    /// Re-encode text-only pages as 1-bit CCITT G4 — drops paper texture; gated on a
    /// blurred-mask SSIM check (only meaningful with --aggressive)
    #[arg(long)]
    pub bilevel: bool,

    /// Remove author, title, creation date, creator info
    #[arg(long)]
    pub strip_metadata: bool,

    /// Report estimated savings without writing output
    #[arg(long)]
    pub dry_run: bool,

    /// Overwrite existing output files
    #[arg(long)]
    pub force: bool,

    /// Re-deflate Flate streams with Zopfli (slower, slightly denser output)
    #[arg(long)]
    pub zopfli: bool,

    /// Maximum decoded size of any single PDF stream, in MiB
    #[arg(long, default_value_t = 512)]
    pub max_decompressed_mib: usize,

    /// Parallel files (defaults to 1 to bound decoded-image memory)
    #[arg(short, long, value_name = "N")]
    pub jobs: Option<usize>,
}

pub fn validate_args(cli: &Cli) -> Result<(), String> {
    if cli.output.is_some() && cli.output_dir.is_some() {
        return Err("--output and --output-dir are mutually exclusive".into());
    }
    if cli.output.is_some() && cli.input.len() > 1 {
        return Err("--output requires exactly one input file".into());
    }
    if !(1..=16_384).contains(&cli.max_decompressed_mib) {
        return Err("--max-decompressed-mib must be between 1 and 16384".into());
    }
    if cli.jobs == Some(0) {
        return Err("--jobs must be at least 1".into());
    }
    Ok(())
}

pub fn resolve_output_path(input: &str, cli: &Cli) -> std::path::PathBuf {
    if let Some(ref out) = cli.output {
        return std::path::PathBuf::from(out);
    }
    let input_path = std::path::Path::new(input);
    let stem = input_path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = input_path.extension().unwrap_or_default().to_string_lossy();
    let filename = if ext.is_empty() {
        format!("{}{}", stem, cli.suffix)
    } else {
        format!("{}{}.{}", stem, cli.suffix, ext)
    };
    match &cli.output_dir {
        Some(dir) => std::path::Path::new(dir).join(filename),
        None => input_path.with_file_name(filename),
    }
}

pub fn compute_exit_code(successes: usize, errors: usize) -> i32 {
    if errors == 0 {
        0
    } else if successes > 0 {
        1
    } else {
        2
    }
}

fn compress_options(cli: &Cli) -> CompressOptions {
    CompressOptions {
        aggressive: cli.aggressive || cli.recommended,
        max_pixels: cli.max_pixels,
        image_quality: if cli.recommended {
            75
        } else {
            cli.image_quality
        },
        smoothing: cli.smoothing,
        downsample_dpi: if cli.recommended {
            Some(120)
        } else {
            cli.downsample_dpi
        },
        force_jpeg: cli.force_jpeg || cli.recommended,
        bilevel: cli.bilevel,
        strip_metadata: cli.strip_metadata,
        dry_run: cli.dry_run,
        force: cli.force,
        zopfli: cli.zopfli || cli.recommended,
        max_decompressed_bytes: cli.max_decompressed_mib.saturating_mul(1024 * 1024),
    }
}

fn print_file_result(input: &str, output: &str, stats: &FileStats, opts: &CompressOptions) {
    let mode = if opts.aggressive {
        "lossless + lossy"
    } else {
        "lossless"
    };
    let last_line = if opts.dry_run {
        "  [dry-run, not saved]".to_string()
    } else {
        format!("  Saved to:    {}", output)
    };
    println!(
        "{}\n  Original:    {}\n  Compressed:  {}\n  Reduction:   {:.1}%  (-{})\n  Mode:        {}\n{}\n",
        input,
        FileStats::format_bytes(stats.original_bytes),
        FileStats::format_bytes(stats.compressed_bytes),
        stats.reduction_percent(),
        FileStats::format_bytes(stats.saved_bytes()),
        mode,
        last_line
    );
}

fn main() {
    let cli = Cli::parse();

    if let Err(e) = validate_args(&cli) {
        eprintln!("Error: {}", e);
        process::exit(2);
    }

    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(cli.jobs.unwrap_or(1))
        .build_global()
    {
        eprintln!("Error: could not initialize worker pool: {}", e);
        process::exit(2);
    }

    let opts = compress_options(&cli);

    let multi = MultiProgress::new();
    let style = ProgressStyle::with_template("{spinner:.green} {msg}").unwrap();

    let batch = Mutex::new(BatchStats::default());

    cli.input.par_iter().for_each(|input| {
        let output_path = resolve_output_path(input, &cli);
        let output_str = output_path.to_string_lossy().into_owned();
        let pb = multi.add(ProgressBar::new_spinner());
        pb.set_style(style.clone());
        pb.set_message(format!("Processing {}", input));
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        match compress_file(input, &output_str, &opts) {
            Ok(file_stats) => {
                pb.finish_and_clear();
                print_file_result(input, &output_str, &file_stats, &opts);
                let mut b = batch.lock().unwrap();
                b.add(file_stats);
            }
            Err(e) => {
                pb.finish_and_clear();
                eprintln!("Error processing {}: {}", input, e);
                batch.lock().unwrap().add_error();
            }
        }
    });

    let b = batch.lock().unwrap();
    if cli.input.len() > 1 {
        b.print_summary();
    }
    let successes = b.total_files;
    let errors = b.error_count;
    drop(b);
    process::exit(compute_exit_code(successes, errors));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cli(args: &[&str]) -> Result<Cli, clap::Error> {
        use clap::Parser;
        Cli::try_parse_from(std::iter::once("pdf-minimizer").chain(args.iter().copied()))
    }

    #[test]
    fn rejects_output_with_output_dir() {
        let cli = make_cli(&["a.pdf", "-o", "out.pdf", "--output-dir", "/tmp"]).unwrap();
        assert!(validate_args(&cli).is_err());
    }

    #[test]
    fn rejects_output_with_multiple_inputs() {
        let cli = make_cli(&["a.pdf", "b.pdf", "-o", "out.pdf"]).unwrap();
        assert!(validate_args(&cli).is_err());
    }

    #[test]
    fn accepts_valid_single_file() {
        let cli = make_cli(&["a.pdf", "-o", "out.pdf"]).unwrap();
        assert!(validate_args(&cli).is_ok());
    }

    #[test]
    fn accepts_valid_batch() {
        let cli = make_cli(&["a.pdf", "b.pdf", "--output-dir", "/tmp"]).unwrap();
        assert!(validate_args(&cli).is_ok());
    }

    #[test]
    fn recommended_conflicts_with_custom_image_tuning() {
        assert!(make_cli(&["a.pdf", "--recommended", "--image-quality", "90"]).is_err());
        assert!(make_cli(&["a.pdf", "--recommended", "--aggressive"]).is_err());
    }

    #[test]
    fn accepts_recommended_preset() {
        let cli = make_cli(&["a.pdf", "--recommended"]).unwrap();
        assert!(cli.recommended);
        assert!(validate_args(&cli).is_ok());

        let opts = compress_options(&cli);
        assert!(opts.aggressive);
        assert_eq!(opts.downsample_dpi, Some(120));
        assert_eq!(opts.image_quality, 75);
        assert!(opts.force_jpeg);
        assert!(opts.zopfli);
    }

    #[test]
    fn rejects_invalid_decompression_limit() {
        let cli = make_cli(&["a.pdf", "--max-decompressed-mib", "0"]).unwrap();
        assert!(validate_args(&cli).is_err());
    }

    #[test]
    fn rejects_zero_parallel_jobs() {
        let cli = make_cli(&["a.pdf", "--jobs", "0"]).unwrap();
        assert!(validate_args(&cli).is_err());
    }

    #[test]
    fn resolve_output_uses_suffix_by_default() {
        let cli = make_cli(&["input.pdf"]).unwrap();
        let out = resolve_output_path("input.pdf", &cli);
        assert_eq!(out.file_name().unwrap(), "input_compressed.pdf");
    }

    #[test]
    fn resolve_output_uses_output_dir() {
        let cli = make_cli(&["input.pdf", "--output-dir", "/out"]).unwrap();
        let out = resolve_output_path("input.pdf", &cli);
        assert_eq!(out, std::path::PathBuf::from("/out/input_compressed.pdf"));
    }

    #[test]
    fn resolve_output_no_extension_no_trailing_dot() {
        let cli = make_cli(&["myfile"]).unwrap();
        let out = resolve_output_path("myfile", &cli);
        assert_eq!(out.file_name().unwrap(), "myfile_compressed");
    }

    #[test]
    fn exit_code_all_success() {
        assert_eq!(compute_exit_code(3, 0), 0);
    }

    #[test]
    fn exit_code_partial_failure() {
        assert_eq!(compute_exit_code(2, 1), 1);
    }

    #[test]
    fn exit_code_total_failure() {
        assert_eq!(compute_exit_code(0, 3), 2);
    }
}
