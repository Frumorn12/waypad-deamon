//! Splitting a stream of concatenated JPEG images into individual frames.
//!
//! Used by fallback capture paths that emit whole pictures back to back with no
//! framing of their own, so the boundaries have to be recovered from the SOI and
//! EOI markers.

pub struct JpegStreamReader {
    buffer: Vec<u8>,
}

impl Default for JpegStreamReader {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegStreamReader {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            let Some(start) = find_marker(&self.buffer, [0xff, 0xd8], 0) else {
                self.buffer.clear();
                break;
            };
            if start > 0 {
                self.buffer.drain(..start);
            }
            let Some(end) = find_marker(&self.buffer, [0xff, 0xd9], 2) else {
                break;
            };
            let frame_end = end + 2;
            frames.push(self.buffer[..frame_end].to_vec());
            self.buffer.drain(..frame_end);
        }
        frames
    }
}

fn find_marker(buffer: &[u8], marker: [u8; 2], from: usize) -> Option<usize> {
    buffer
        .windows(2)
        .enumerate()
        .skip(from)
        .find_map(|(index, window)| (window == marker).then_some(index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_concatenated_jpeg_frames() {
        let mut reader = JpegStreamReader::new();
        let frames = reader.push(&[0xff, 0xd8, 1, 2, 0xff, 0xd9, 0xff, 0xd8]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], vec![0xff, 0xd8, 1, 2, 0xff, 0xd9]);
        let frames = reader.push(&[3, 4, 0xff, 0xd9]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], vec![0xff, 0xd8, 3, 4, 0xff, 0xd9]);
    }

    #[test]
    fn finds_markers_after_offset() {
        assert_eq!(find_marker(&[0, 0xff, 0xd8], [0xff, 0xd8], 0), Some(1));
        assert_eq!(find_marker(&[0xff, 0xd8], [0xff, 0xd8], 1), None);
    }
}
