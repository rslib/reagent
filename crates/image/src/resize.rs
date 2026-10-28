//! Box-filter resizing: one pass over the source rows, split over a few
//! threads.
//!
//! The whole image is scaled by two coverage tables, one per axis. The source
//! rows of a target row are added, weighted, into one `u32` accumulator row,
//! and the completed row is divided and written out: every source row is read
//! front to back, with no column walks and no per-pixel allocation.

use std::num::NonZeroUsize;

use crate::Pixels;

/// Fixed point scale of the coverage weight of one axis: the weights of one
/// target position sum to exactly this. The two axes' weights multiply, so one
/// target sample adds up to `AXIS_SCALE * AXIS_SCALE * 255`, which still fits
/// in a `u32`.
const AXIS_SCALE: u64 = 1 << 12;
/// Target pixels one thread carries: a smaller image stays on one thread and
/// spawns nothing.
const PIXELS_PER_THREAD: u64 = 1_000_000;
/// Most threads one resize uses.
const MAX_THREADS: usize = 8;
/// The widest value an 8-bit sample takes, for the clip after dividing.
const SAMPLE_MAX: u32 = 255;

/// Keeps `width` and `height` within `max_side`, shrinking the longer side
/// and keeping the aspect ratio. Each side stays at least 1.
pub fn fit(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_side == 0 {
        return (width.max(1), height.max(1));
    }
    let (long, short) = if width >= height {
        (width, height)
    } else {
        (height, width)
    };
    if long <= max_side {
        return (width, height);
    }
    let rounded = (u64::from(short) * u64::from(max_side) + u64::from(long) / 2) / u64::from(long);
    let short_out = rounded.clamp(1, u64::from(max_side)) as u32;
    if width >= height {
        (max_side, short_out)
    } else {
        (short_out, max_side)
    }
}

/// Resizes `pixels` to `width` by `height` with a box filter: each target
/// pixel is the mean of the source pixels it covers, weighted by coverage.
pub fn resize(pixels: Pixels, width: u32, height: u32) -> Pixels {
    let target_pixels = u64::from(width) * u64::from(height);
    resize_with_threads(pixels, width, height, threads_for(target_pixels))
}

/// How many threads a resize of `target_pixels` pixels uses: one per
/// [`PIXELS_PER_THREAD`] target pixels, capped by the machine and
/// [`MAX_THREADS`], and at least one.
fn threads_for(target_pixels: u64) -> usize {
    let by_size = (target_pixels / PIXELS_PER_THREAD).clamp(1, MAX_THREADS as u64) as usize;
    let available = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    by_size.min(available).max(1)
}

/// [`resize`] with an explicit thread count, so a test or a measurement can
/// compare one thread with many.
pub(crate) fn resize_with_threads(
    pixels: Pixels,
    width: u32,
    height: u32,
    threads: usize,
) -> Pixels {
    if width == 0 || height == 0 || (width == pixels.width && height == pixels.height) {
        return pixels;
    }
    let channels = pixels.channels.count();
    let out_stride = width as usize * channels;
    let Some(len) = out_stride.checked_mul(height as usize) else {
        return pixels;
    };
    let columns = Axis::new(pixels.width as usize, width as usize);
    let rows = Axis::new(pixels.height as usize, height as usize);
    let source = Source {
        data: &pixels.data,
        stride: pixels.stride(),
        channels,
        columns: &columns,
    };
    let mut data = vec![0u8; len];
    let threads = threads.clamp(1, height as usize);
    let rows_per_band = (height as usize).div_ceil(threads);
    if threads == 1 || rows_per_band >= height as usize {
        fill(&mut data, 0, out_stride, &rows, &source);
    } else {
        // A band of target rows reads only the source rows it covers, so the
        // bands share nothing but the source.
        let band_len = rows_per_band * out_stride;
        std::thread::scope(|scope| {
            for (index, band) in data.chunks_mut(band_len).enumerate() {
                let first_row = index * rows_per_band;
                let source = &source;
                let rows = &rows;
                scope.spawn(move || fill(band, first_row, out_stride, rows, source));
            }
        });
    }
    Pixels {
        width,
        height,
        channels: pixels.channels,
        data,
    }
}

/// The source pixels and the column coverage every band reads.
struct Source<'a> {
    data: &'a [u8],
    stride: usize,
    channels: usize,
    columns: &'a Axis,
}

/// One axis of the box filter: for every target position, the run of
/// source positions it covers and the weight of each.
///
/// A target position covers one contiguous run of source positions, so the
/// table keeps the first position and one `u16` weight per covered source
/// position: about 2 bytes per source position, below the 3 or 4 bytes of a
/// pixel, so the tables never outgrow the image they scale.
struct Axis {
    /// The first source position of every target position.
    first: Vec<u32>,
    /// Where the weights of every target position start in `weights`, and
    /// one past the end.
    offset: Vec<usize>,
    /// The weights, target by target, in source order.
    weights: Vec<u16>,
    /// The weight total of every target position: always [`AXIS_SCALE`].
    totals: Vec<u32>,
}

impl Axis {
    /// Builds the coverage of `target` positions over `source` positions.
    fn new(source: usize, target: usize) -> Axis {
        let mut axis = Axis {
            first: Vec::with_capacity(target),
            offset: Vec::with_capacity(target.saturating_add(1)),
            weights: Vec::with_capacity(source.saturating_add(target)),
            totals: Vec::with_capacity(target),
        };
        axis.offset.push(0);
        if source == 0 || target == 0 {
            return axis;
        }
        let scale = source as f64 / target as f64;
        for index in 0..target {
            let from = index as f64 * scale;
            let to = ((index + 1) as f64 * scale).min(source as f64);
            let first = (from.floor() as usize).min(source - 1);
            let last = (to.ceil() as usize).clamp(first + 1, source);
            let span = (to - from).max(f64::MIN_POSITIVE);
            let run = axis.weights.len();
            // The weights are the rounded running sum of the ideal coverage,
            // so an error never piles up on the last source position.
            let mut ideal = 0.0f64;
            let mut assigned = 0u64;
            for at in first..last {
                let low = from.max(at as f64);
                let high = to.min((at + 1) as f64);
                ideal += ((high - low).max(0.0) / span) * AXIS_SCALE as f64;
                let upto = if at + 1 == last {
                    AXIS_SCALE
                } else {
                    ideal.round() as u64
                };
                let weight = upto.saturating_sub(assigned).min(AXIS_SCALE);
                assigned += weight;
                axis.weights.push(weight as u16);
            }
            if assigned == 0 {
                axis.weights.truncate(run);
                axis.weights.push(AXIS_SCALE as u16);
                assigned = AXIS_SCALE;
            }
            axis.first.push(first as u32);
            axis.offset.push(axis.weights.len());
            axis.totals.push(assigned as u32);
        }
        axis
    }

    /// The first source position of `target` and the weights of its run.
    fn covers(&self, target: usize) -> (usize, &[u16]) {
        let (Some(&first), Some(&start), Some(&end)) = (
            self.first.get(target),
            self.offset.get(target),
            self.offset.get(target + 1),
        ) else {
            return (0, &[]);
        };
        (first as usize, self.weights.get(start..end).unwrap_or(&[]))
    }

    /// The weight total of `target`.
    fn total(&self, target: usize) -> u32 {
        self.totals.get(target).copied().unwrap_or(0)
    }
}

/// Fills the target rows of one band, `first_row` and on.
fn fill(out: &mut [u8], first_row: usize, out_stride: usize, rows: &Axis, source: &Source) {
    let channels = source.channels;
    // One accumulator row of weighted samples, reused by every target row.
    let mut accumulator = vec![0u32; out_stride];
    for (offset, target) in out.chunks_mut(out_stride).enumerate() {
        let row_total = rows.total(first_row + offset);
        if row_total == 0 {
            continue;
        }
        accumulator.fill(0);
        let (first_source_row, row_weights) = rows.covers(first_row + offset);
        for (step, &row_weight) in row_weights.iter().enumerate() {
            let Some(row) = source_row(source.data, source.stride, first_source_row + step) else {
                continue;
            };
            let row_weight = u32::from(row_weight);
            // Each target column reads its own run of source pixels, which
            // follow the previous run in memory: one forward walk of the row.
            for (sums, target_column) in accumulator.chunks_exact_mut(channels).zip(0..) {
                let (first_column, column_weights) = source.columns.covers(target_column);
                let Some(run) = row.get(first_column * channels..) else {
                    continue;
                };
                for (sample, &column_weight) in run.chunks_exact(channels).zip(column_weights) {
                    let weight = row_weight * u32::from(column_weight);
                    for (sum, value) in sums.iter_mut().zip(sample) {
                        *sum = sum.saturating_add(u32::from(*value) * weight);
                    }
                }
            }
        }
        for (target_column, pixel) in target.chunks_mut(channels).enumerate() {
            let Some(total) = row_total.checked_mul(source.columns.total(target_column)) else {
                continue;
            };
            if total == 0 {
                continue;
            }
            let half = total / 2;
            let base = target_column * channels;
            for (channel, value) in pixel.iter_mut().enumerate() {
                let sum = accumulator.get(base + channel).copied().unwrap_or(0);
                *value = (sum.saturating_add(half) / total).min(SAMPLE_MAX) as u8;
            }
        }
    }
}

/// One whole source row, or `None` when it runs past the end of the buffer.
fn source_row(data: &[u8], stride: usize, row: usize) -> Option<&[u8]> {
    if stride == 0 {
        return None;
    }
    let start = row.checked_mul(stride)?;
    data.get(start..start.checked_add(stride)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Channels;

    fn solid(width: u32, height: u32, color: [u8; 3]) -> Pixels {
        let data = color
            .iter()
            .copied()
            .cycle()
            .take(width as usize * height as usize * 3)
            .collect();
        Pixels::new(width, height, Channels::Rgb, data).expect("solid image")
    }

    #[test]
    fn axis_tables_take_about_one_weight_per_source_position() {
        const SOURCE: usize = 20_000_000;
        const TARGET: usize = 2000;
        let axis = Axis::new(SOURCE, TARGET);
        assert!(axis.weights.len() <= SOURCE + TARGET);
        assert_eq!(axis.first.len(), TARGET);
        assert_eq!(axis.offset.len(), TARGET + 1);
        for target in 0..TARGET {
            let (_, weights) = axis.covers(target);
            let sum: u32 = weights.iter().map(|weight| u32::from(*weight)).sum();
            assert_eq!(sum, AXIS_SCALE as u32);
        }
    }

    #[test]
    fn fit_shrinks_the_longer_side() {
        assert_eq!(fit(4032, 3024, 2000), (2000, 1500));
        assert_eq!(fit(3024, 4032, 2000), (1500, 2000));
        assert_eq!(fit(1, 10_000, 2000), (1, 2000));
        assert_eq!(fit(10_000, 1, 2000), (2000, 1));
        assert_eq!(fit(2000, 1000, 2000), (2000, 1000));
        assert_eq!(fit(20, 10, 2000), (20, 10));
        assert_eq!(fit(20, 0, 2000), (20, 1));
    }

    #[test]
    fn a_4x4_to_2x2_is_the_exact_mean() {
        let mut data = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                data.extend_from_slice(&[x * 16, y * 16, x * 16 + y * 16]);
            }
        }
        let pixels = Pixels::new(4, 4, Channels::Rgb, data).expect("4x4");
        let out = resize(pixels, 2, 2);
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(out.data, [8, 8, 16, 40, 8, 48, 8, 40, 48, 40, 40, 80]);
    }

    #[test]
    fn a_solid_color_stays_solid_at_odd_ratios() {
        let out = resize(solid(7, 5, [10, 200, 30]), 3, 2);
        assert_eq!((out.width, out.height), (3, 2));
        let (pixels, _) = out.data.as_chunks::<3>();
        for pixel in pixels {
            assert_eq!(*pixel, [10, 200, 30]);
        }
    }

    #[test]
    fn threads_give_the_same_bytes_as_one_thread() {
        let mut data = Vec::new();
        for y in 0..40u8 {
            for x in 0..37u8 {
                data.extend_from_slice(&[x.wrapping_mul(7), y.wrapping_mul(5), x ^ y]);
            }
        }
        let pixels = Pixels::new(37, 40, Channels::Rgba, {
            let mut rgba = Vec::new();
            let (pixels, _) = data.as_chunks::<3>();
            for pixel in pixels {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            rgba
        })
        .expect("image");
        let one = resize_with_threads(pixels.clone(), 13, 9, 1);
        let many = resize_with_threads(pixels, 13, 9, 4);
        assert_eq!(one.data, many.data);
        assert_eq!((one.width, one.height), (13, 9));
    }

    #[test]
    fn resizing_to_the_same_size_returns_the_same_pixels() {
        let pixels = solid(3, 2, [1, 2, 3]);
        let out = resize(pixels.clone(), 3, 2);
        assert_eq!(out, pixels);
    }

    #[test]
    fn the_thread_count_follows_the_target_size() {
        let available = std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        assert_eq!(threads_for(0), 1);
        assert_eq!(threads_for(PIXELS_PER_THREAD - 1), 1);
        assert_eq!(
            threads_for(PIXELS_PER_THREAD * 3),
            3.min(MAX_THREADS).min(available).max(1)
        );
        assert_eq!(threads_for(u64::MAX / 2), MAX_THREADS.min(available).max(1));
    }
}
