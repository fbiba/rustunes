//! Bounded AFC copy, status classification, and file observations.

pub mod copy;

/// An `AfcClient` over an in-memory stream and the phone's end of it, so the
/// primitives' exact requests are checked offline. No device is involved.
#[cfg(test)]
mod duplex {
    use idevice::{
        Idevice,
        afc::{
            AfcClient, MAGIC,
            opcode::AfcOpcode,
            packet::{AfcPacket, AfcPacketHeader},
        },
    };

    pub(super) fn pair() -> (AfcClient, Idevice) {
        let (client, peer) = tokio::io::duplex(super::copy::CHUNK * 2);
        (
            AfcClient::new(Idevice::new(Box::new(client), "offline-only")),
            Idevice::new(Box::new(peer), "offline-peer"),
        )
    }

    pub(super) async fn reply(
        peer: &mut Idevice,
        request: &AfcPacket,
        operation: AfcOpcode,
        header_payload: Vec<u8>,
        payload: Vec<u8>,
    ) {
        let header_payload_len = AfcPacketHeader::LEN + header_payload.len() as u64;
        let packet = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: header_payload_len + payload.len() as u64,
                header_payload_len,
                packet_num: request.header.packet_num,
                operation,
            },
            header_payload,
            payload,
        };
        peer.send_raw(&packet.serialize()).await.unwrap();
    }

    /// The reply for an AFC status code (0 acknowledges).
    pub(super) async fn status(peer: &mut Idevice, request: &AfcPacket, code: u64) {
        reply(
            peer,
            request,
            AfcOpcode::Status,
            code.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await;
    }
}

use idevice::{
    IdeviceError, IdeviceService,
    afc::{AfcClient, errors::AfcError},
    provider::IdeviceProvider,
    usbmuxd::errors::UsbmuxdError,
};
use std::time::Duration;

const CONNECT_ATTEMPTS: usize = 3;

/// A new AFC connection that the phone refused, or closed before answering
/// anything. Soon after another connection hashed files (GetFileHash, which
/// leaks inside afcd and which this crate never sends), the phone did this to
/// about one new connection in nine while another AFC connection was open.
/// Nothing was served on it, so it may be discarded and opened again. Over
/// Wi-Fi the same refusal arrives as a refused or reset TCP connection instead
/// of a usbmuxd result.
fn unserved(error: &IdeviceError) -> bool {
    match error {
        IdeviceError::Usbmuxd(UsbmuxdError::ConnectionRefused) => true,
        IdeviceError::Socket(error) => matches!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
        ),
        _ => false,
    }
}

/// Whether `error` carries (as a type, never as text) the phone refusing an
/// AFC connection or dropping one without a reply. The phone also drops
/// established connections this way (observed), so a read-only
/// capture that failed like this may start again on a new connection.
pub fn dropped_by_phone(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<Dropped>() || cause.downcast_ref::<IdeviceError>().is_some_and(unserved)
    })
}

/// A connection dropped during a bounded copy operation, kept by type
/// without the upstream error text those operations never render.
#[derive(Debug)]
struct Dropped;
impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("connection dropped by the phone")
    }
}
impl std::error::Error for Dropped {}

/// Open an AFC connection that has answered one device-information request
/// (which reads no file), reopening an unserved one. The returned client
/// numbers its packets from zero again, so evidence that records AFC packet
/// numbers from zero is unchanged; the phone accepts the restarted numbering
/// (checked).
pub async fn connect_checked(provider: &dyn IdeviceProvider) -> Result<AfcClient, IdeviceError> {
    let mut attempt = 1;
    loop {
        let result = async {
            let mut afc = AfcClient::connect(provider).await?;
            afc.get_device_info_raw().await?;
            Ok(AfcClient::new(afc.idevice))
        }
        .await;
        match result {
            Err(error) if attempt < CONNECT_ATTEMPTS && unserved(&error) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            other => return other,
        }
    }
}

/// idevice also wraps header/parser failures in `Afc(_)`. Those do not prove
/// a complete response and must never permit connection reuse or FD cleanup I/O.
pub fn complete_status(error: &IdeviceError) -> bool {
    // Explicit allowlist: new upstream parser variants must default to discard.
    matches!(
        error,
        IdeviceError::Afc(
            AfcError::UnknownError
                | AfcError::OpHeaderInvalid
                | AfcError::NoResources
                | AfcError::ReadError
                | AfcError::WriteError
                | AfcError::UnknownPacketType
                | AfcError::InvalidArg
                | AfcError::ObjectNotFound
                | AfcError::ObjectIsDir
                | AfcError::PermDenied
                | AfcError::ServiceNotConnected
                | AfcError::OpTimeout
                | AfcError::TooMuchData
                | AfcError::EndOfData
                | AfcError::OpNotSupported
                | AfcError::ObjectExists
                | AfcError::ObjectBusy
                | AfcError::NoSpaceLeft
                | AfcError::OpWouldBlock
                | AfcError::IoError
                | AfcError::OpInterrupted
                | AfcError::OpInProgress
                | AfcError::InternalError
                | AfcError::MuxError
                | AfcError::NoMem
                | AfcError::NotEnoughData
                | AfcError::DirNotEmpty
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_connections_are_recognised_by_type_through_context_only() {
        let typed = |kind| {
            anyhow::anyhow!(IdeviceError::Socket(std::io::Error::new(kind, "socket")))
                .context("AFC operation failed; journal retained")
        };
        assert!(dropped_by_phone(&typed(std::io::ErrorKind::BrokenPipe)));
        assert!(dropped_by_phone(&typed(std::io::ErrorKind::UnexpectedEof)));
        assert!(dropped_by_phone(&typed(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(dropped_by_phone(&typed(
            std::io::ErrorKind::ConnectionReset
        )));
        assert!(dropped_by_phone(
            &anyhow::Error::from(IdeviceError::Usbmuxd(UsbmuxdError::ConnectionRefused))
                .context("outer")
                .context("outermost")
        ));
        assert!(!dropped_by_phone(&typed(std::io::ErrorKind::TimedOut)));
        assert!(!dropped_by_phone(&anyhow::anyhow!(
            "device socket io failed: Broken pipe (os error 32)"
        )));
        assert!(!dropped_by_phone(
            &anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
                .context("local file")
        ));
    }

    #[tokio::test]
    async fn bounded_operations_keep_a_dropped_connection_recognisable() {
        let failing = |kind| async move {
            Err::<(), _>(IdeviceError::Socket(std::io::Error::new(
                kind,
                "upstream text",
            )))
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let cancelled = || std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let eof = std::io::ErrorKind::UnexpectedEof;
        let failure =
            copy::bounded_controlled(deadline, "read attributes", failing(eof), cancelled())
                .await
                .unwrap_err();
        assert!(dropped_by_phone(&failure.error));
        let shown = format!("{:#}", failure.error);
        assert!(shown.starts_with("read attributes: uncertain transport"));
        assert!(!shown.contains("upstream text"));
        let failure = copy::bounded(deadline, "read", failing(eof))
            .await
            .unwrap_err();
        assert!(dropped_by_phone(&failure.error));
        let timed_out = std::io::ErrorKind::TimedOut;
        let failure = copy::bounded_controlled(deadline, "read", failing(timed_out), cancelled())
            .await
            .unwrap_err();
        assert!(!dropped_by_phone(&failure.error));
    }

    #[test]
    fn only_refused_or_unanswered_connections_are_reopened() {
        assert!(unserved(&IdeviceError::Usbmuxd(
            UsbmuxdError::ConnectionRefused
        )));
        assert!(unserved(&IdeviceError::Socket(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "early eof"
        ))));
        assert!(unserved(&IdeviceError::Socket(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broken pipe"
        ))));
        assert!(!unserved(&IdeviceError::Socket(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timeout"
        ))));
        assert!(!unserved(&IdeviceError::Afc(AfcError::ObjectNotFound)));
        assert!(!unserved(&IdeviceError::Afc(AfcError::PermDenied)));
    }
}
