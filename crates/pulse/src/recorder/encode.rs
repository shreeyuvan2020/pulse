//! Turns captured frames into H.264 in MP4 files. This is the OpenH264
//! software encoder, which runs on every OS; hardware encoders (Media
//! Foundation, VideoToolbox, VA-API) will plug in behind the same
//! `VideoEncoder` shape.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;

use mp4::{AvcConfig, Bytes, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};
use openh264::{OpenH264API, Timestamp};
use pulse_core::activity::Thumbnail;

/// A captured screen in BGRA order, as every OS capture API returns it.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Bytes per row, which can be more than `width * 4`.
    pub stride: usize,
    pub bgra: Vec<u8>,
}

/// Fits the screen inside 1280×720 keeping its shape, never upscaling. H.264
/// needs even sizes.
pub fn target_size(width: usize, height: usize) -> (usize, usize) {
    let (width, height) = if width * 720 > height * 1280 {
        (1280.min(width), height * 1280.min(width) / width)
    } else {
        (width * 720.min(height) / height, 720.min(height))
    };
    ((width & !1).max(2), (height & !1).max(2))
}

/// Box-filters the frame down to `width`×`height` RGB, averaging every source
/// pixel so small text stays legible instead of aliasing away.
pub fn downscale(frame: &Frame, width: usize, height: usize) -> Vec<u8> {
    let span = |index: usize, target: usize, source: usize| {
        let start = index * source / target;
        start..((index + 1) * source / target).max(start + 1)
    };
    let columns: Vec<_> = (0..width).map(|x| span(x, width, frame.width)).collect();
    let mut rgb = vec![0; width * height * 3];
    for y in 0..height {
        let rows = span(y, height, frame.height);
        for (x, columns) in columns.iter().enumerate() {
            let mut sum = [0u32; 3];
            for row in rows.clone() {
                let line = &frame.bgra[row * frame.stride..];
                for pixel in line[columns.start * 4..columns.end * 4].as_chunks::<4>().0 {
                    sum[0] += u32::from(pixel[2]);
                    sum[1] += u32::from(pixel[1]);
                    sum[2] += u32::from(pixel[0]);
                }
            }
            let count = (rows.len() * columns.len()) as u32;
            let out = (y * width + x) * 3;
            for channel in 0..3 {
                rgb[out + channel] = (sum[channel] / count) as u8;
            }
        }
    }
    rgb
}

pub struct Encoded {
    /// Length-prefixed NAL units, ready to be an MP4 sample.
    pub sample: Vec<u8>,
    pub keyframe: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub thumbnail: Thumbnail,
}

pub struct VideoEncoder {
    encoder: Encoder,
}

impl VideoEncoder {
    pub fn new() -> io::Result<VideoEncoder> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .max_frame_rate(FrameRate::from_hz(1.0))
            .rate_control_mode(RateControlMode::Bitrate)
            // about 40 MB per hour: 40_000_000 * 8 / 3600
            .bitrate(BitRate::from_bps(90_000))
            .debug(false)
            .adaptive_quantization(false)
            .background_detection(false)
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(300));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(io::Error::other)?;
        Ok(VideoEncoder { encoder })
    }

    pub fn encode(
        &mut self,
        rgb: &[u8],
        width: usize,
        height: usize,
        millis: u64,
        keyframe: bool,
    ) -> io::Result<Encoded> {
        let yuv = YUVBuffer::from_rgb_source(RgbSliceU8::new(rgb, (width, height)));
        let thumbnail = Thumbnail::from_luma(yuv.y(), width, height, yuv.strides().0);
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let stream = self
            .encoder
            .encode_at(&yuv, Timestamp::from_millis(millis))
            .map_err(io::Error::other)?;
        let mut encoded = Encoded {
            sample: Vec::new(),
            keyframe: matches!(stream.frame_type(), FrameType::IDR | FrameType::I),
            sps: None,
            pps: None,
            thumbnail,
        };
        for layer in (0..stream.num_layers()).filter_map(|index| stream.layer(index)) {
            for nal in (0..layer.nal_count()).filter_map(|index| layer.nal_unit(index)) {
                let nal = strip_start_code(nal);
                match nal.first().map(|header| header & 0x1f) {
                    Some(7) => encoded.sps = Some(nal.to_vec()),
                    Some(8) => encoded.pps = Some(nal.to_vec()),
                    Some(_) => {
                        encoded.sample.extend((nal.len() as u32).to_be_bytes());
                        encoded.sample.extend(nal);
                    }
                    None => {}
                }
            }
        }
        Ok(encoded)
    }
}

fn strip_start_code(nal: &[u8]) -> &[u8] {
    nal.strip_prefix(&[0, 0, 0, 1])
        .or_else(|| nal.strip_prefix(&[0, 0, 1]))
        .unwrap_or(nal)
}

/// One MP4 file with a single H.264 track at one frame per second.
pub struct SegmentWriter {
    writer: Mp4Writer<BufWriter<File>>,
    frames: u64,
}

impl SegmentWriter {
    pub fn create(
        path: &Path,
        width: usize,
        height: usize,
        sps: Vec<u8>,
        pps: Vec<u8>,
    ) -> io::Result<SegmentWriter> {
        let config = Mp4Config {
            major_brand: "isom".parse().map_err(io::Error::other)?,
            minor_version: 512,
            compatible_brands: ["isom", "iso2", "avc1", "mp41"]
                .iter()
                .map(|brand| brand.parse())
                .collect::<Result<_, _>>()
                .map_err(io::Error::other)?,
            timescale: 1000,
        };
        let mut writer = Mp4Writer::write_start(BufWriter::new(File::create(path)?), &config)
            .map_err(io::Error::other)?;
        writer
            .add_track(&TrackConfig {
                track_type: TrackType::Video,
                timescale: 1000,
                language: "und".into(),
                media_conf: MediaConfig::AvcConfig(AvcConfig {
                    width: width as u16,
                    height: height as u16,
                    seq_param_set: sps,
                    pic_param_set: pps,
                }),
            })
            .map_err(io::Error::other)?;
        Ok(SegmentWriter { writer, frames: 0 })
    }

    pub fn write(&mut self, sample: Vec<u8>, keyframe: bool) -> io::Result<()> {
        let sample = Mp4Sample {
            start_time: self.frames * 1000,
            duration: 1000,
            rendering_offset: 0,
            is_sync: keyframe,
            bytes: Bytes::from(sample),
        };
        self.frames += 1;
        self.writer
            .write_sample(1, &sample)
            .map_err(io::Error::other)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.writer.write_end().map_err(io::Error::other)?;
        use std::io::Write;
        self.writer.into_writer().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_common_screens_inside_720p() {
        assert_eq!(target_size(1920, 1080), (1280, 720));
        assert_eq!(target_size(1366, 768), (1280, 718));
        assert_eq!(target_size(2560, 1600), (1152, 720));
        assert_eq!(target_size(1024, 768), (960, 720));
        assert_eq!(target_size(800, 601), (800, 600));
    }

    #[test]
    fn downscale_averages_blocks_and_swaps_bgra_to_rgb() {
        // 4x2 source: left half pure blue in BGRA, right half alternates red and black
        let mut bgra = Vec::new();
        for _ in 0..2 {
            bgra.extend([255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 200, 255, 0, 0, 0, 255]);
            bgra.extend([9; 8]); // stride padding
        }
        let frame = Frame {
            width: 4,
            height: 2,
            stride: 24,
            bgra,
        };

        assert_eq!(downscale(&frame, 2, 1), vec![0, 0, 255, 100, 0, 0]);
    }
}
