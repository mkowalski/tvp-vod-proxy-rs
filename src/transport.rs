//! Bounded payload-activity inspection of this proxy's fixed-PID FFmpeg output.
use std::io;

pub(crate) const VIDEO_PID: u16 = 256;
pub(crate) const AUDIO_PID: u16 = 257;

#[derive(Default)]
pub(crate) struct Activity {
    pub video: bool,
    pub audio: bool,
}
#[derive(Default)]
pub(crate) struct Tracker {
    pending: Vec<u8>,
}

impl Tracker {
    pub(crate) fn feed(&mut self, mut bytes: &[u8]) -> io::Result<Activity> {
        let mut seen = Activity::default();
        if !self.pending.is_empty() {
            let n = (188 - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.pending.len() < 188 {
                return Ok(seen);
            }
            Self::packet(&self.pending, &mut seen)?;
            self.pending.clear();
        }
        let (packets, remainder) = bytes.as_chunks::<188>();
        for packet in packets {
            Self::packet(packet, &mut seen)?;
        }
        self.pending.extend_from_slice(remainder);
        Ok(seen)
    }
    fn packet(packet: &[u8], seen: &mut Activity) -> io::Result<()> {
        if packet[0] != 0x47 || packet[1] & 0x80 != 0 || packet[3] & 0x30 == 0 {
            return Err(io::Error::other("invalid transport framing"));
        }
        if packet[3] & 0x10 == 0 {
            return Ok(());
        }
        let offset = if packet[3] & 0x20 != 0 {
            5 + packet[4] as usize
        } else {
            4
        };
        if offset > 188 {
            return Err(io::Error::other("invalid transport adaptation"));
        }
        if offset == 188 {
            return Ok(());
        }
        let pid = ((packet[1] & 0x1f) as u16) << 8 | packet[2] as u16;
        seen.video |= pid == VIDEO_PID;
        seen.audio |= pid == AUDIO_PID;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_payload_and_adaptation_only_packets() {
        let mut video = [0u8; 188];
        video[0] = 0x47;
        video[1] = 1;
        video[3] = 0x10;
        let mut audio = video;
        audio[2] = 1;
        let mut tracker = Tracker::default();
        assert!(!tracker.feed(&video[..100]).unwrap().video);
        assert!(tracker.feed(&video[100..]).unwrap().video);
        assert!(tracker.feed(&audio).unwrap().audio);
        audio[3] = 0x20;
        audio[4] = 183;
        assert!(!tracker.feed(&audio).unwrap().audio);
        assert!(tracker.pending.len() < 188);
    }
}
