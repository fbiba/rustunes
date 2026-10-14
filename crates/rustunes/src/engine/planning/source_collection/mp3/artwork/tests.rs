use super::*;
use music_protocol::protocol::trace::private_write;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mp3-artwork-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        crate::engine::execution::native_insert::create_trace(&p).unwrap();
        Self(p)
    }
    fn jpeg(&self, width: u16, height: u16) -> Vec<u8> {
        let raw = self.0.join(format!("{width}-{height}.rgb"));
        private_write(
            &raw,
            &[40, 90, 170].repeat(usize::from(width) * usize::from(height)),
        )
        .unwrap();
        let o = Command::new("ffmpeg")
            .env_remove("FFREPORT")
            .args([
                "-nostdin",
                "-hide_banner",
                "-v",
                "error",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgb24",
                "-video_size",
                &format!("{width}x{height}"),
                "-i",
                "/proc/self/fd/0",
                "-frames:v",
                "1",
                "-threads",
                "1",
                "-c:v",
                "mjpeg",
                "-pix_fmt",
                "yuvj444p",
                "-f",
                "image2pipe",
                "pipe:1",
            ])
            .stdin(Stdio::from(File::open(raw).unwrap()))
            .output()
            .unwrap();
        assert!(o.status.success() && o.stderr.is_empty());
        assert!(o.stdout.len() < 65536);
        o.stdout
    }
    fn source(&self, images: &[Vec<u8>]) -> (PathBuf, Source) {
        let mut frames = Vec::new();
        for image in images {
            let data = [b"\x00image/jpeg\x00\x03\x00".as_slice(), image].concat();
            frames.extend_from_slice(b"APIC");
            frames.extend_from_slice(&(data.len() as u32).to_be_bytes());
            frames.extend_from_slice(&[0, 0]);
            frames.extend(data);
        }
        let n = frames.len();
        let size = [
            ((n >> 21) & 127) as u8,
            ((n >> 14) & 127) as u8,
            ((n >> 7) & 127) as u8,
            (n & 127) as u8,
        ];
        let audio = music_protocol::test_support::mpeg::audio();
        let file = self.0.join(format!("source-{}.mp3", images.len()));
        fs::write(
            &file,
            [b"ID3\x03\x00\x00".as_slice(), &size, &frames, &audio].concat(),
        )
        .unwrap();
        let source = music_protocol::media::mpeg::read(&file).unwrap();
        (file, source)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
#[test]
fn an_insert_batch_sends_the_one_front_cover_with_its_payload_checksum() {
    let t = Temp::new();
    let jpeg = t.jpeg(20, 10);
    let (file, source) = t.source(std::slice::from_ref(&jpeg));
    let cover = batch_cover(&file, &source).unwrap().unwrap();
    assert_eq!(cover.bytes, jpeg);
    assert_eq!(cover.checksum, jpeg.len() as u32 + 1);
    assert_eq!(cover.sha256, <[u8; 32]>::from(Sha256::digest(&jpeg)));
    let (file, source) = t.source(&[]);
    assert!(batch_cover(&file, &source).unwrap().is_none());
    // Two front covers: neither is sent.
    let (file, source) = t.source(&[jpeg.clone(), jpeg]);
    assert!(batch_cover(&file, &source).unwrap().is_none());
}
