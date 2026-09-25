//! ATC framing using the existing idevice service transport.
//! Framing reference: shinkuan/AirCard-Linux, MIT, services/atc.rs at
//! 7686fd21e3e598d2217c5e6da88229cd1bca4e07. Music sync semantics are unverified.

use std::{borrow::Cow, path::Path};

use anyhow::{Context, Result, bail};
use idevice::{Idevice, IdeviceError, IdeviceService, ReadWrite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub struct Atc {
    stream: Box<dyn ReadWrite>,
    received: ReceivedBuffer,
    // Set before an operation; only its complete success permits reuse.
    unusable: bool,
    output: Option<(
        super::trace::OutputAllowance,
        super::transfer::CaptureControl,
    )>,
}

// Kept with the transport, rather than inside a cancellable receive future.
// Dropping that future cannot release the reservation or lose consumed bytes.
// Dropping the transport retains its actual fragment without additional wire I/O.
#[derive(Default)]
struct ReceivedBuffer {
    bytes: Vec<u8>,
    retention: Option<(super::trace::OutputReservation, std::path::PathBuf)>,
}
impl std::ops::Deref for ReceivedBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}
impl std::ops::DerefMut for ReceivedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}
impl Drop for ReceivedBuffer {
    fn drop(&mut self) {
        if let Some((reservation, path)) = self.retention.take() {
            if !self.bytes.is_empty() {
                // A dropped future cannot report this I/O result. The owner
                // still must treat cancellation as incomplete, never success.
                let _ = reservation.write(&path, &self.bytes);
            }
        }
    }
}

impl IdeviceService for Atc {
    fn service_name() -> Cow<'static, str> {
        "com.apple.atc".into()
    }

    async fn from_stream(idevice: Idevice) -> Result<Self, IdeviceError> {
        Ok(Self {
            stream: idevice
                .get_socket()
                .ok_or(IdeviceError::NoEstablishedConnection)?,
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        })
    }
}

impl Atc {
    /// Attach only at the crate's fresh transport construction seam. The normal
    /// Session owns this same allowance through handshake and terminal receive.
    pub(crate) fn with_output(
        mut self,
        output: super::trace::OutputAllowance,
        control: &super::transfer::CaptureControl,
    ) -> Result<Self> {
        control.check()?;
        anyhow::ensure!(
            !self.unusable && self.received.is_empty() && self.output.is_none(),
            "output allowance requires an intact unbound ATC transport"
        );
        self.output = Some((output, control.clone()));
        Ok(self)
    }
    pub(crate) fn ensure_output_control(
        &self,
        control: &super::transfer::CaptureControl,
    ) -> Result<()> {
        if let Some((_, original)) = &self.output {
            anyhow::ensure!(
                original.deadline == control.deadline
                    && std::sync::Arc::ptr_eq(&original.cancelled, &control.cancelled),
                "bounded ATC continuation must retain its original control"
            );
        }
        Ok(())
    }
    pub(crate) fn reserve_received(&mut self, partial_path: &Path) -> Result<Option<usize>> {
        if let Some((output, control)) = &self.output {
            control.check()?;
            anyhow::ensure!(
                !self.unusable && self.received.retention.is_none(),
                "bounded ATC receive already interrupted"
            );
            let hold = output.reserve_file(4, (MAX_FRAME + 4) as u64)?;
            let capacity = hold.capacity() as usize;
            self.received.retention = Some((hold, partial_path.to_owned()));
            Ok(Some(capacity))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn retain_received(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some((reservation, _)) = self.received.retention.take() {
            reservation.write(path, bytes)
        } else {
            self.retain_raw(path, bytes)
        }
    }
    pub(crate) fn retain_partial(&mut self, path: &Path) -> Result<()> {
        if let Some((reservation, _)) = self.received.retention.take() {
            reservation.write(path, &self.received.bytes)
        } else {
            self.retain_raw(path, &self.received.bytes)
        }
    }
    pub(crate) fn discard_bounded(&mut self) {
        if self.output.is_some() {
            self.unusable = true;
        }
    }
    pub(crate) fn retain_raw(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        match &self.output {
            Some((output, _)) => output.write(path, bytes),
            None => super::trace::private_write(path, bytes),
        }
    }
    pub(crate) fn retain_xml(&self, path: &Path, value: &plist::Value) -> Result<()> {
        if let Some((output, control)) = &self.output {
            let mut file = output.create_controlled(path, control)?;
            value.to_writer_xml(&mut file)?;
            file.finish()
        } else {
            let mut xml = Vec::new();
            value.to_writer_xml(&mut xml)?;
            super::trace::private_write(path, &xml)
        }
    }
    /// Consume an intact live transport for pending-only simultaneous observation
    /// and completion. No conversion back to Atc or ordinary Session exists.
    /// The existing aggregate ByteBudget remains around this same stream.
    pub(crate) fn into_pending_parts(
        self,
        control: &super::transfer::CaptureControl,
    ) -> Result<(
        tokio::io::ReadHalf<Box<dyn ReadWrite>>,
        tokio::io::WriteHalf<Box<dyn ReadWrite>>,
        Option<super::trace::OutputAllowance>,
    )> {
        self.ensure_output_control(control)?;
        if self.unusable || !self.received.is_empty() || self.received.retention.is_some() {
            bail!("pending transport split after a failed or partial operation");
        }
        let (read, write) = tokio::io::split(self.stream);
        Ok((read, write, self.output.map(|(allowance, _)| allowance)))
    }

    /// Save the intended outgoing frame before sending it. This is not a device
    /// acknowledgement. Use the caller's new trace directory and sequence.
    /// Any error or cancellation permanently refuses reuse of this connection.
    pub async fn send_plist(
        &mut self,
        value: &plist::Value,
        trace_dir: &Path,
        sequence: usize,
    ) -> Result<()> {
        if self.unusable {
            bail!("ATC connection cannot be reused after a failed or cancelled operation");
        }
        self.unusable = true;
        if let Some((output, control)) = self.output.clone() {
            self.send_with_output(value, trace_dir, sequence, &output, &control)
                .await?;
            self.unusable = false;
            return Ok(());
        }
        let mut payload = Vec::new();
        plist::to_writer_binary(&mut payload, value).context("encoding outgoing ATC plist")?;
        let length = payload.len();
        if length == 0 || length > MAX_FRAME {
            bail!("invalid little-endian ATC payload length: {length}");
        }
        let mut frame = (length as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&payload);
        super::trace::private_write(
            &trace_dir.join(format!("{sequence:03}-outgoing.atc")),
            &frame,
        )?;
        let mut xml = Vec::new();
        plist::to_writer_xml(&mut xml, value).context("serializing outgoing ATC trace plist")?;
        super::trace::private_write(
            &trace_dir.join(format!("{sequence:03}-outgoing.plist")),
            &xml,
        )?;
        self.stream
            .write_all(&frame)
            .await
            .context("sending ATC frame; intended outgoing trace retained")?;
        self.stream
            .flush()
            .await
            .context("flushing ATC stream; intended outgoing trace retained")?;
        self.unusable = false;
        Ok(())
    }

    async fn send_with_output(
        &mut self,
        value: &plist::Value,
        trace_dir: &Path,
        sequence: usize,
        output: &super::trace::OutputAllowance,
        control: &super::transfer::CaptureControl,
    ) -> Result<()> {
        let frame = retain_outgoing_raw(value, trace_dir, sequence, output, control)?;
        self.retain_xml(
            &trace_dir.join(format!("{sequence:03}-outgoing.plist")),
            value,
        )?;
        control
            .run(
                std::time::Duration::from_secs(35),
                "bounded ATC send deadline",
                async {
                    self.stream
                        .write_all(&frame)
                        .await
                        .context("sending ATC frame; intended outgoing trace retained")?;
                    self.stream
                        .flush()
                        .await
                        .context("flushing ATC stream; intended outgoing trace retained")?;
                    Ok(())
                },
            )
            .await
    }

    /// Bytes received so far remain available on EOF, error, or cancellation.
    pub fn partial_frame(&self) -> &[u8] {
        &self.received
    }

    pub async fn read_frame(&mut self) -> Result<Vec<u8>> {
        self.read_frame_with_limit(MAX_FRAME + 4).await
    }

    pub(crate) async fn read_frame_with_limit(&mut self, evidence_limit: usize) -> Result<Vec<u8>> {
        if self.unusable {
            bail!("ATC connection cannot be reused after a failed or cancelled operation");
        }
        self.unusable = true;
        self.received.clear();
        if let Some((_, control)) = self.output.clone() {
            control
                .run(
                    std::time::Duration::from_secs(35),
                    "bounded ATC receive deadline",
                    self.receive_frame(evidence_limit),
                )
                .await?;
        } else {
            self.receive_frame(evidence_limit).await?;
        }
        self.unusable = false;
        Ok(std::mem::take(&mut self.received.bytes))
    }

    async fn receive_frame(&mut self, evidence_limit: usize) -> Result<()> {
        self.receive_until(4).await?;
        let header: [u8; 4] = self.received[..4].try_into()?;
        let length = u32::from_le_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME {
            bail!("invalid little-endian ATC payload length: {length}");
        }
        anyhow::ensure!(
            4 + length <= evidence_limit,
            "ATC frame exceeds reserved evidence capacity; header retained"
        );
        self.receive_until(4 + length).await
    }

    async fn receive_until(&mut self, length: usize) -> Result<()> {
        let mut buffer = [0u8; 8192];
        while self.received.len() < length {
            let remaining = (length - self.received.len()).min(buffer.len());
            let count = self.stream.read(&mut buffer[..remaining]).await?;
            if count == 0 {
                bail!(
                    "ATC stream closed with {} of {length} bytes received",
                    self.received.len()
                );
            }
            self.received.extend_from_slice(&buffer[..count]);
        }
        Ok(())
    }
}

/// Bounded outgoing raw preparation shared by ordinary and genuine pending
/// writers. The bytes are durable before a caller can expose them to transport.
pub(crate) fn retain_outgoing_raw(
    value: &plist::Value,
    trace_dir: &Path,
    sequence: usize,
    output: &super::trace::OutputAllowance,
    control: &super::transfer::CaptureControl,
) -> Result<Vec<u8>> {
    use std::io::Write;
    control.check()?;
    let mut raw = output.create(&trace_dir.join(format!("{sequence:03}-outgoing.atc")))?;
    let mut buffer = output.buffer(MAX_FRAME + 4, control);
    buffer.write_all(&[0; 4])?;
    let encoded =
        plist::to_writer_binary(&mut buffer, value).context("encoding bounded outgoing ATC plist");
    let length = buffer.bytes.len() - 4;
    if encoded.is_ok() {
        anyhow::ensure!(
            length > 0 && length <= MAX_FRAME,
            "invalid bounded ATC payload length"
        );
        buffer.bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
    }
    let frame = encoded.as_ref().ok().map(|_| buffer.bytes.clone());
    // A failed serializer may have emitted a prefix. Keep those charged
    // bytes at the exclusive path, with a zero length on incomplete output.
    raw.write_charged(buffer)?;
    raw.finish()?;
    encoded?;
    Ok(frame.expect("successful bounded encoding"))
}

#[cfg(test)]
mod tests {
    use super::*;
    mod bounded;
    use plist::Value;
    use std::{
        fs,
        future::{Future, poll_fn},
        os::unix::fs::{DirBuilderExt, PermissionsExt},
        path::PathBuf,
        task::Poll,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::io::duplex;

    fn new_trace_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "music-protocol-atc-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn outgoing_frame_matches_private_directional_trace() {
        let trace_dir = new_trace_dir("outgoing");
        let (client, mut device) = duplex(1024);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert("Session".into(), 1.into());
        dictionary.insert("Command".into(), "Ping".into());
        let value = Value::Dictionary(dictionary);
        atc.send_plist(&value, &trace_dir, 7).await.unwrap();
        drop(atc);
        let mut frame = Vec::new();
        device.read_to_end(&mut frame).await.unwrap();
        assert_eq!(
            u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize,
            frame.len() - 4
        );
        assert!(frame[4..].starts_with(b"bplist00"));
        assert_eq!(plist::from_bytes::<Value>(&frame[4..]).unwrap(), value);
        let raw_path = trace_dir.join("007-outgoing.atc");
        let xml_path = trace_dir.join("007-outgoing.plist");
        assert_eq!(fs::read(&raw_path).unwrap(), frame);
        assert_eq!(
            plist::from_bytes::<Value>(&fs::read(&xml_path).unwrap()).unwrap(),
            value
        );
        for path in [raw_path, xml_path] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(&trace_dir).unwrap().count(), 2);
        fs::remove_dir_all(trace_dir).unwrap();
    }

    #[tokio::test]
    async fn oversized_outgoing_plist_sends_nothing() {
        let trace_dir = new_trace_dir("oversized");
        let (client, mut device) = duplex(16);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        // The plist envelope makes this payload exceed the exact 16 MiB bound.
        let value = Value::Data(vec![0; MAX_FRAME]);
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            atc.send_plist(&value, &trace_dir, 0),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid little-endian ATC payload length")
        );
        drop(atc);
        let mut sent = Vec::new();
        device.read_to_end(&mut sent).await.unwrap();
        assert!(sent.is_empty());
        assert_eq!(fs::read_dir(&trace_dir).unwrap().count(), 0);
        fs::remove_dir_all(trace_dir).unwrap();
    }

    #[tokio::test]
    async fn existing_outgoing_trace_prevents_transmission_and_overwrite() {
        let trace_dir = new_trace_dir("collision");
        let raw_path = trace_dir.join("003-outgoing.atc");
        crate::protocol::trace::private_write(&raw_path, b"previous attempt").unwrap();
        let (client, mut device) = duplex(1024);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        assert!(
            atc.send_plist(&Value::Boolean(true), &trace_dir, 3)
                .await
                .is_err()
        );
        drop(atc);
        let mut sent = Vec::new();
        device.read_to_end(&mut sent).await.unwrap();
        assert!(sent.is_empty());
        assert_eq!(fs::read(raw_path).unwrap(), b"previous attempt");
        fs::remove_dir_all(trace_dir).unwrap();
    }

    #[tokio::test]
    async fn transport_failure_retains_intended_outgoing_trace() {
        let trace_dir = new_trace_dir("failed-write");
        let (client, device) = duplex(1024);
        drop(device);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        let value = Value::Boolean(false);
        let error = atc.send_plist(&value, &trace_dir, 2).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("intended outgoing trace retained")
        );
        let frame = fs::read(trace_dir.join("002-outgoing.atc")).unwrap();
        assert_eq!(plist::from_bytes::<Value>(&frame[4..]).unwrap(), value);
        let xml = fs::read(trace_dir.join("002-outgoing.plist")).unwrap();
        assert_eq!(plist::from_bytes::<Value>(&xml).unwrap(), value);
        let retry = atc.send_plist(&value, &trace_dir, 3).await.unwrap_err();
        assert!(retry.to_string().contains("cannot be reused"));
        assert!(
            atc.read_frame()
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be reused")
        );
        assert_eq!(fs::read_dir(&trace_dir).unwrap().count(), 2);
        fs::remove_dir_all(trace_dir).unwrap();
    }

    #[tokio::test]
    async fn receives_little_endian_binary_plist_without_sending() {
        let (client, mut device) = duplex(1024);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        let mut dict = plist::Dictionary::new();
        dict.insert("Command".into(), "SyncAllowed".into());
        let value = Value::Dictionary(dict);
        let mut payload = Vec::new();
        plist::to_writer_binary(&mut payload, &value).unwrap();
        device
            .write_all(&(payload.len() as u32).to_le_bytes())
            .await
            .unwrap();
        device.write_all(&payload).await.unwrap();
        let frame = atc.read_frame().await.unwrap();
        assert_eq!(plist::from_bytes::<Value>(&frame[4..]).unwrap(), value);
        assert_eq!(&frame[4..], payload);
        drop(atc);
        let mut reply = Vec::new();
        device.read_to_end(&mut reply).await.unwrap();
        assert!(reply.is_empty(), "passive probe must not send ATC messages");
    }

    #[tokio::test]
    async fn cancelled_partial_header_or_payload_preserves_bytes_and_refuses_reuse() {
        let partial_frames = [
            8u32.to_le_bytes()[..2].to_vec(),
            [8u32.to_le_bytes().as_slice(), b"bp"].concat(),
        ];
        for partial in partial_frames {
            let trace_dir = new_trace_dir("cancelled-read");
            let (client, mut device) = duplex(64);
            let mut atc = Atc {
                stream: Box::new(client),
                received: ReceivedBuffer::default(),
                unusable: false,
                output: None,
            };
            device.write_all(&partial).await.unwrap();
            {
                let mut read = std::pin::pin!(atc.read_frame());
                poll_fn(|context| {
                    assert!(read.as_mut().poll(context).is_pending());
                    Poll::Ready(())
                })
                .await;
                // Drop the pending operation, as a timeout/select would do.
            }
            assert_eq!(atc.partial_frame(), partial);
            // A fresh valid frame must not let reuse hide the lost boundary.
            device.write_all(&4u32.to_le_bytes()).await.unwrap();
            device.write_all(b"next").await.unwrap();
            let retry = atc.read_frame().await.unwrap_err();
            assert!(retry.to_string().contains("cannot be reused"));
            assert_eq!(atc.partial_frame(), partial);
            let send = atc
                .send_plist(&Value::Boolean(true), &trace_dir, 0)
                .await
                .unwrap_err();
            assert!(send.to_string().contains("cannot be reused"));
            assert_eq!(fs::read_dir(&trace_dir).unwrap().count(), 0);
            drop(atc);
            let mut sent = Vec::new();
            device.read_to_end(&mut sent).await.unwrap();
            assert!(sent.is_empty());
            fs::remove_dir_all(trace_dir).unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_partial_write_preserves_intended_trace_and_refuses_reuse() {
        let trace_dir = new_trace_dir("cancelled-write");
        let (client, mut device) = duplex(8);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        // Queue a valid incoming frame so an unguarded retry would complete.
        device.write_all(&4u32.to_le_bytes()).await.unwrap();
        device.write_all(b"next").await.unwrap();
        let value = Value::String("cancelled transport test".into());
        {
            let mut send = std::pin::pin!(atc.send_plist(&value, &trace_dir, 0));
            poll_fn(|context| {
                assert!(send.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        let intended = fs::read(trace_dir.join("000-outgoing.atc")).unwrap();
        assert_eq!(plist::from_bytes::<Value>(&intended[4..]).unwrap(), value);
        let retry = atc.send_plist(&value, &trace_dir, 1).await.unwrap_err();
        assert!(retry.to_string().contains("cannot be reused"));
        assert!(
            atc.read_frame()
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be reused")
        );
        assert_eq!(fs::read_dir(&trace_dir).unwrap().count(), 2);
        drop(atc);
        let mut sent = Vec::new();
        device.read_to_end(&mut sent).await.unwrap();
        assert_eq!(sent, intended[..8]);
        fs::remove_dir_all(trace_dir).unwrap();
    }

    #[tokio::test]
    async fn completed_frames_and_flushed_sends_allow_sequential_reuse() {
        let trace_dir = new_trace_dir("sequential");
        let (client, mut device) = duplex(1024);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        for flag in [true, false] {
            let mut payload = Vec::new();
            plist::to_writer_binary(&mut payload, &Value::Boolean(flag)).unwrap();
            device
                .write_all(&(payload.len() as u32).to_le_bytes())
                .await
                .unwrap();
            device.write_all(&payload).await.unwrap();
        }
        for (sequence, flag) in [true, false].into_iter().enumerate() {
            let expected = Value::Boolean(flag);
            let frame = atc.read_frame().await.unwrap();
            assert_eq!(plist::from_bytes::<Value>(&frame[4..]).unwrap(), expected);
            assert!(atc.partial_frame().is_empty());
            atc.send_plist(&expected, &trace_dir, sequence)
                .await
                .unwrap();
            let mut header = [0u8; 4];
            device.read_exact(&mut header).await.unwrap();
            let mut payload = vec![0u8; u32::from_le_bytes(header) as usize];
            device.read_exact(&mut payload).await.unwrap();
            assert_eq!(plist::from_bytes::<Value>(&payload).unwrap(), expected);
        }
        assert_eq!(fs::read_dir(&trace_dir).unwrap().count(), 4);
        fs::remove_dir_all(trace_dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_unbounded_frame_before_allocating_body() {
        for length in [0u32, (MAX_FRAME + 1) as u32, u32::MAX] {
            let (client, mut device) = duplex(16);
            let mut atc = Atc {
                stream: Box::new(client),
                received: ReceivedBuffer::default(),
                unusable: false,
                output: None,
            };
            device.write_all(&length.to_le_bytes()).await.unwrap();
            let error = atc.read_frame().await.err().unwrap();
            assert!(error.to_string().contains("invalid little-endian"));
            assert_eq!(atc.partial_frame(), length.to_le_bytes());
            drop(device);
            assert!(
                atc.read_frame()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("cannot be reused")
            );
            assert_eq!(atc.partial_frame(), length.to_le_bytes());
        }
    }

    #[tokio::test]
    async fn rejects_truncated_payload() {
        let (client, mut device) = duplex(16);
        let mut atc = Atc {
            stream: Box::new(client),
            received: ReceivedBuffer::default(),
            unusable: false,
            output: None,
        };
        device.write_all(&8u32.to_le_bytes()).await.unwrap();
        device.write_all(b"bplist").await.unwrap();
        drop(device);
        assert!(atc.read_frame().await.is_err());
        assert_eq!(
            atc.partial_frame(),
            [8u32.to_le_bytes().as_slice(), b"bplist"].concat()
        );
        assert!(
            atc.read_frame()
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be reused")
        );
        assert_eq!(
            atc.partial_frame(),
            [8u32.to_le_bytes().as_slice(), b"bplist"].concat()
        );
    }
}
