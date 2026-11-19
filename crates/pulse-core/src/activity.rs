//! Screen activity: how much of the screen changed from one second to the
//! next. The client scores frames while it records, and a reviewer's server
//! can decode the uploaded video and score it again from the same luma
//! (brightness) plane, so the dead-time cut never has to trust the client.

/// Thumbnail size: 160×90 cells, about one cell per 8×8 pixels at 720p.
pub const THUMBNAIL_WIDTH: usize = 160;
pub const THUMBNAIL_HEIGHT: usize = 90;

/// How far a cell's average brightness (0–255) must move to count as changed.
/// Large enough to ignore video compression noise, small enough to catch a
/// line of text appearing in a cell.
const CELL_THRESHOLD: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail(Vec<u8>);

impl Thumbnail {
    /// Averages a luma plane into a fixed grid of cells.
    pub fn from_luma(luma: &[u8], width: usize, height: usize, stride: usize) -> Thumbnail {
        let mut sums = vec![0u32; THUMBNAIL_WIDTH * THUMBNAIL_HEIGHT];
        let mut counts = vec![0u32; THUMBNAIL_WIDTH * THUMBNAIL_HEIGHT];
        let columns: Vec<usize> = (0..width).map(|x| x * THUMBNAIL_WIDTH / width).collect();
        for y in 0..height {
            let row = y * THUMBNAIL_HEIGHT / height * THUMBNAIL_WIDTH;
            for (x, value) in luma[y * stride..y * stride + width].iter().enumerate() {
                sums[row + columns[x]] += u32::from(*value);
                counts[row + columns[x]] += 1;
            }
        }
        Thumbnail(
            sums.iter()
                .zip(&counts)
                .map(|(sum, count)| (sum / (*count).max(1)) as u8)
                .collect(),
        )
    }

    /// Changed cells per thousand, from 0 (identical) to 1000 (all changed).
    pub fn change_from(&self, previous: &Thumbnail) -> u16 {
        let changed = self
            .0
            .iter()
            .zip(&previous.0)
            .filter(|(now, before)| now.abs_diff(**before) > CELL_THRESHOLD)
            .count();
        (changed * 1000 / self.0.len()) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(width: usize, height: usize, value: impl Fn(usize, usize) -> u8) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| value(x, y))
            .collect()
    }

    #[test]
    fn identical_frames_score_zero_and_noise_is_ignored() {
        let base = plane(1280, 720, |x, y| ((x / 7 + y / 3) % 200) as u8);
        let noisy: Vec<u8> = base
            .iter()
            .enumerate()
            .map(|(i, v)| v.saturating_add((i % 5) as u8))
            .collect();
        let before = Thumbnail::from_luma(&base, 1280, 720, 1280);

        assert_eq!(
            Thumbnail::from_luma(&base, 1280, 720, 1280).change_from(&before),
            0
        );
        assert_eq!(
            Thumbnail::from_luma(&noisy, 1280, 720, 1280).change_from(&before),
            0
        );
    }

    #[test]
    fn a_new_line_of_text_changes_one_row_of_cells() {
        let blank = plane(1280, 720, |_, _| 30);
        // a white text line 8 px tall across the left half, at y 400..408 (one cell row)
        let typed = plane(1280, 720, |x, y| {
            if (400..408).contains(&y) && x < 640 {
                230
            } else {
                30
            }
        });

        let score = Thumbnail::from_luma(&typed, 1280, 720, 1280)
            .change_from(&Thumbnail::from_luma(&blank, 1280, 720, 1280));

        // 80 of 14400 cells
        assert_eq!(score, 5);
    }

    #[test]
    fn stride_padding_is_ignored_and_any_size_maps_onto_the_grid() {
        let width = 1366;
        let stride = 1376;
        let left_dark = plane(stride, 768, |x, _| {
            if x >= width {
                255
            } else if x < 683 {
                0
            } else {
                200
            }
        });
        // same visible pixels as left_dark, different padding
        let all_dark = plane(
            stride,
            768,
            |x, _| if x < 683 || x >= width { 0 } else { 200 },
        );
        let swapped = plane(stride, 768, |x, _| {
            if x >= width {
                0
            } else if x < 683 {
                200
            } else {
                0
            }
        });

        let base = Thumbnail::from_luma(&left_dark, width, 768, stride);
        assert_eq!(
            Thumbnail::from_luma(&all_dark, width, 768, stride).change_from(&base),
            0
        );
        assert_eq!(
            Thumbnail::from_luma(&swapped, width, 768, stride).change_from(&base),
            1000
        );
    }
}
