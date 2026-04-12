/// Size stats for a single processed file.
#[derive(Debug, Clone)]
pub struct FileStats {
    pub original_bytes: u64,
    pub compressed_bytes: u64,
}

impl FileStats {
    pub fn reduction_percent(&self) -> f64 {
        if self.original_bytes == 0 {
            return 0.0;
        }
        let saved = self.original_bytes.saturating_sub(self.compressed_bytes);
        (saved as f64 / self.original_bytes as f64) * 100.0
    }

    pub fn saved_bytes(&self) -> u64 {
        self.original_bytes.saturating_sub(self.compressed_bytes)
    }

    pub fn format_bytes(bytes: u64) -> String {
        if bytes >= 1_000_000 {
            format!("{:.1} MB", bytes as f64 / 1_000_000.0)
        } else if bytes >= 1_000 {
            format!("{:.1} KB", bytes as f64 / 1_000.0)
        } else {
            format!("{} B", bytes)
        }
    }
}

/// Aggregated stats across a batch run.
#[derive(Debug, Default)]
pub struct BatchStats {
    pub total_files: usize,
    pub error_count: usize,
    reductions: Vec<f64>,
    saved: u64,
}

impl BatchStats {
    pub fn add(&mut self, stats: FileStats) {
        self.total_files += 1;
        self.reductions.push(stats.reduction_percent());
        self.saved += stats.saved_bytes();
    }

    pub fn add_error(&mut self) {
        self.error_count += 1;
    }

    pub fn total_saved_bytes(&self) -> u64 {
        self.saved
    }

    pub fn avg_reduction_percent(&self) -> f64 {
        if self.reductions.is_empty() {
            return 0.0;
        }
        self.reductions.iter().sum::<f64>() / self.reductions.len() as f64
    }

    pub fn print_summary(&self) {
        let saved = FileStats::format_bytes(self.saved);
        let avg = self.avg_reduction_percent();
        let errors = if self.error_count > 0 {
            format!("  |  {} error{}", self.error_count, if self.error_count == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        println!(
            "\nProcessed {} file{}  |  Total saved: {} ({:.1}% avg reduction){}",
            self.total_files,
            if self.total_files == 1 { "" } else { "s" },
            saved,
            avg,
            errors
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stats_reduction_percent() {
        let s = FileStats { original_bytes: 1000, compressed_bytes: 600 };
        assert!((s.reduction_percent() - 40.0).abs() < 0.01);
    }

    #[test]
    fn file_stats_saved_bytes() {
        let s = FileStats { original_bytes: 1000, compressed_bytes: 600 };
        assert_eq!(s.saved_bytes(), 400);
    }

    #[test]
    fn batch_stats_aggregates() {
        let mut b = BatchStats::default();
        b.add(FileStats { original_bytes: 1000, compressed_bytes: 600 });
        b.add(FileStats { original_bytes: 2000, compressed_bytes: 1800 });
        assert_eq!(b.total_files, 2);
        assert_eq!(b.total_saved_bytes(), 600);
    }

    #[test]
    fn batch_stats_avg_reduction() {
        let mut b = BatchStats::default();
        b.add(FileStats { original_bytes: 1000, compressed_bytes: 500 });
        b.add(FileStats { original_bytes: 1000, compressed_bytes: 750 });
        // avg of 50% and 25% = 37.5%
        assert!((b.avg_reduction_percent() - 37.5).abs() < 0.01);
    }
}
