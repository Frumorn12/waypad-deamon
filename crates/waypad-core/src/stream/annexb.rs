//! Splitting an H.264 Annex-B byte stream into whole access units.
//!
//! Any backend that gets its encoded video as a byte stream — a GStreamer pipe
//! on Linux, an ffmpeg process, a Media Foundation sample that carries several
//! NAL units — feeds it through here to get the pictures back out. The rules
//! encoded below are the ones that decide whether an Android decoder shows a
//! picture or a black rectangle, so they are covered by tests rather than trust.

use tracing::warn;

/// Largest Annex-B payload kept while looking for the next access unit. A 1080p
/// IDR stays far below this, so hitting the cap means the producer is emitting
/// something that is not a byte stream and the reader resynchronises.
const MAX_ANNEX_B_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub key_frame: bool,
    /// SPS/PPS copied out of the access unit, sent ahead of it as a config
    /// frame. They stay inline in `data` as well so a decoder that ignores
    /// config frames still finds them before the IDR.
    pub parameter_sets: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
struct NalRef {
    /// Offset of the leading byte of the start code, not of the NAL header.
    start: usize,
    kind: u8,
    /// `first_mb_in_slice == 0`, only meaningful for slice NAL units.
    first_slice: bool,
}

impl NalRef {
    fn is_slice(self) -> bool {
        matches!(self.kind, 1..=5)
    }

    fn starts_access_unit(self, has_slice: bool) -> bool {
        match self.kind {
            // Access unit delimiter: always opens a picture.
            9 => true,
            // Parameter sets and SEI belong to the picture that follows them.
            6 | 7 | 8 | 13 | 14 | 15 => has_slice,
            // A slice opens a new picture only when it restarts the macroblock
            // scan, which keeps multi-slice frames (x264 sliced threads) whole.
            1..=5 => has_slice && self.first_slice,
            _ => false,
        }
    }
}

/// Splits the encoder's Annex-B byte stream into whole access units. Reads from
/// the producer pipe land on arbitrary boundaries, so NAL units are only cut
/// once the start code of the following one has actually been seen.
pub struct AnnexBStreamReader {
    buffer: Vec<u8>,
    nals: Vec<NalRef>,
    scan_from: usize,
}

impl Default for AnnexBStreamReader {
    fn default() -> Self {
        Self::new()
    }
}

impl AnnexBStreamReader {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            nals: Vec::new(),
            scan_from: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<AccessUnit> {
        self.buffer.extend_from_slice(chunk);
        self.scan();
        let mut units = Vec::new();
        while let Some(boundary) = self.next_boundary() {
            units.push(self.take_access_unit(boundary));
        }
        if self.buffer.len() > MAX_ANNEX_B_BUFFER {
            warn!(
                bytes = self.buffer.len(),
                "annex-b buffer overflow; resynchronising on the next start code"
            );
            self.buffer.clear();
            self.nals.clear();
            self.scan_from = 0;
        }
        units
    }

    fn scan(&mut self) {
        // A start code plus the two bytes needed to classify the NAL span five
        // bytes, so scanning resumes far enough back to complete a pattern that
        // was still truncated on the previous read.
        let mut index = self.scan_from;
        while index + 5 <= self.buffer.len() {
            if self.buffer[index] != 0 || self.buffer[index + 1] != 0 || self.buffer[index + 2] != 1
            {
                index += 1;
                continue;
            }
            // Four-byte start codes are three-byte ones with a leading zero;
            // that zero can never be payload because emulation prevention
            // forbids `00 00 00` inside a NAL.
            let start = if index > 0 && self.buffer[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            self.nals.push(NalRef {
                start,
                kind: self.buffer[index + 3] & 0x1f,
                first_slice: self.buffer[index + 4] & 0x80 != 0,
            });
            index += 3;
        }
        self.scan_from = self.buffer.len().saturating_sub(4);
    }

    fn next_boundary(&self) -> Option<usize> {
        let mut has_slice = false;
        for (index, nal) in self.nals.iter().enumerate() {
            if index > 0 && nal.starts_access_unit(has_slice) {
                return Some(index);
            }
            if nal.is_slice() {
                has_slice = true;
            }
        }
        None
    }

    pub fn has_pending_picture(&self) -> bool {
        self.nals.iter().any(|nal| nal.is_slice())
    }

    /// Releases the buffered access unit without waiting for the start code of
    /// the next one, which would otherwise cost a full frame interval of
    /// latency. Only safe once the producer pipe has gone idle: the encoder
    /// writes one access unit per pipe write, so an idle pipe means the picture
    /// is complete.
    pub fn flush_pending(&mut self) -> Option<AccessUnit> {
        if !self.has_pending_picture() {
            return None;
        }
        let end = self.buffer.len();
        Some(self.take_access_unit_before(self.nals.len(), end))
    }

    fn take_access_unit(&mut self, boundary: usize) -> AccessUnit {
        let end = self.nals[boundary].start;
        self.take_access_unit_before(boundary, end)
    }

    fn take_access_unit_before(&mut self, boundary: usize, end: usize) -> AccessUnit {
        let begin = self.nals[0].start;
        let key_frame = self.nals[..boundary].iter().any(|nal| nal.kind == 5);
        let mut parameter_sets = Vec::new();
        for (index, nal) in self.nals[..boundary].iter().enumerate() {
            if !matches!(nal.kind, 7 | 8) {
                continue;
            }
            let stop = self.nals.get(index + 1).map_or(end, |next| next.start);
            parameter_sets.extend_from_slice(&self.buffer[nal.start..stop]);
        }
        let data = self.buffer[begin..end].to_vec();
        self.buffer.drain(..end);
        self.nals.drain(..boundary);
        for nal in &mut self.nals {
            nal.start -= end;
        }
        self.scan_from = self.scan_from.saturating_sub(end);
        AccessUnit {
            data,
            key_frame,
            parameter_sets: (!parameter_sets.is_empty()).then_some(parameter_sets),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a NAL with a four-byte start code. `header` is the NAL header
    /// byte, `first` the byte after it (its top bit carries
    /// `first_mb_in_slice == 0` for slices).
    fn nal(header: u8, first: u8, payload: &[u8]) -> Vec<u8> {
        let mut unit = vec![0, 0, 0, 1, header, first];
        unit.extend_from_slice(payload);
        unit
    }

    fn short_nal(header: u8, first: u8, payload: &[u8]) -> Vec<u8> {
        let mut unit = vec![0, 0, 1, header, first];
        unit.extend_from_slice(payload);
        unit
    }

    fn aud() -> Vec<u8> {
        nal(0x09, 0x10, &[])
    }

    fn sps() -> Vec<u8> {
        nal(0x67, 0x64, &[0x00, 0x28])
    }

    fn pps() -> Vec<u8> {
        nal(0x68, 0xeb, &[0xe3, 0xcb])
    }

    fn idr() -> Vec<u8> {
        nal(0x65, 0x88, &[1, 2, 3, 4])
    }

    fn slice() -> Vec<u8> {
        nal(0x41, 0x9a, &[5, 6, 7, 8])
    }

    #[test]
    fn splits_annex_b_access_units() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&pps());
        stream.extend_from_slice(&idr());
        let keyframe_len = stream.len();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());

        let units = reader.push(&stream);
        // The trailing access unit stays buffered until the next one starts.
        assert_eq!(units.len(), 1);
        assert!(units[0].key_frame);
        assert_eq!(units[0].data.len(), keyframe_len);
        let mut parameter_sets = sps();
        parameter_sets.extend_from_slice(&pps());
        assert_eq!(
            units[0].parameter_sets.as_deref(),
            Some(&parameter_sets[..])
        );

        let mut tail = aud();
        tail.extend_from_slice(&slice());
        let units = reader.push(&tail);
        assert_eq!(units.len(), 1);
        assert!(!units[0].key_frame);
        assert!(units[0].parameter_sets.is_none());
    }

    #[test]
    fn flushes_the_buffered_picture_when_the_producer_goes_idle() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = aud();
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&pps());
        stream.extend_from_slice(&idr());

        assert!(reader.push(&stream).is_empty());
        assert!(reader.has_pending_picture());
        let unit = reader.flush_pending().expect("buffered keyframe");
        assert!(unit.key_frame);
        assert_eq!(unit.data, stream);
        assert!(unit.parameter_sets.is_some());

        // Nothing is left to flush, and a parameter-set-only tail is never
        // mistaken for a picture.
        assert!(!reader.has_pending_picture());
        assert!(reader.flush_pending().is_none());
        assert!(reader.push(&sps()).is_empty());
        assert!(!reader.has_pending_picture());
        assert!(reader.flush_pending().is_none());
    }

    #[test]
    fn splits_annex_b_units_across_buffer_reads() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&pps());
        stream.extend_from_slice(&idr());
        let keyframe = stream.clone();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());
        let second = stream[keyframe.len()..].to_vec();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());

        // Every possible split point must yield the same two access units,
        // including cuts that land in the middle of a start code.
        for split in 1..stream.len() {
            let mut reader = AnnexBStreamReader::new();
            let mut units = reader.push(&stream[..split]);
            units.extend(reader.push(&stream[split..]));
            assert_eq!(units.len(), 2, "split at {split}");
            assert_eq!(units[0].data, keyframe, "split at {split}");
            assert!(units[0].key_frame, "split at {split}");
            assert_eq!(units[1].data, second, "split at {split}");
            assert!(!units[1].key_frame, "split at {split}");
        }
    }

    #[test]
    fn accepts_three_and_four_byte_start_codes() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = short_nal(0x67, 0x64, &[0x00, 0x28]);
        stream.extend_from_slice(&short_nal(0x68, 0xeb, &[0xe3, 0xcb]));
        stream.extend_from_slice(&short_nal(0x65, 0x88, &[1, 2, 3, 4]));
        let keyframe_len = stream.len();
        stream.extend_from_slice(&sps());
        stream.extend_from_slice(&idr());

        let units = reader.push(&stream);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.len(), keyframe_len);
        assert!(units[0].key_frame);
        let mut parameter_sets = short_nal(0x67, 0x64, &[0x00, 0x28]);
        parameter_sets.extend_from_slice(&short_nal(0x68, 0xeb, &[0xe3, 0xcb]));
        assert_eq!(
            units[0].parameter_sets.as_deref(),
            Some(&parameter_sets[..])
        );
    }

    #[test]
    fn keeps_multi_slice_pictures_in_one_access_unit() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = nal(0x41, 0x9a, &[1, 2]);
        // Second slice of the same picture: first_mb_in_slice != 0.
        stream.extend_from_slice(&nal(0x41, 0x0a, &[3, 4]));
        stream.extend_from_slice(&nal(0x41, 0x0a, &[5, 6]));
        let picture_len = stream.len();
        stream.extend_from_slice(&nal(0x41, 0x9a, &[7, 8]));
        stream.extend_from_slice(&nal(0x41, 0x0a, &[9, 10]));

        let units = reader.push(&stream);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.len(), picture_len);
    }

    #[test]
    fn ignores_leading_bytes_before_the_first_start_code() {
        let mut reader = AnnexBStreamReader::new();
        let mut stream = vec![0xde, 0xad, 0xbe, 0xef];
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&idr());
        let keyframe = stream[4..].to_vec();
        stream.extend_from_slice(&aud());
        stream.extend_from_slice(&slice());

        let units = reader.push(&stream);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, keyframe);
    }
}
