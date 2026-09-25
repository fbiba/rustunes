use super::{Dropped, complete_status, unserved};

use anyhow::{Context, Result, ensure};
use idevice::{
    IdeviceError,
    afc::{
        AfcClient,
        opcode::{AfcFopenMode, AfcOpcode},
    },
};
use std::{
    future::Future,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::{Instant, timeout_at};

/// Bytes per AFC read or write request, Apple's own size. Requests are
/// lock-step, so each one costs a round trip; over Wi-Fi that dominates.
pub const CHUNK: usize = 1 << 20;
/// The longest one request may wait for its reply. Wi-Fi retransmissions have
/// held a healthy connection silent for nine seconds.
pub const OPERATION: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
pub struct CopyLimits {
    pub maximum_size: u64,
    pub maximum_reads: usize,
}

#[derive(Debug)]
pub struct Failure {
    pub error: anyhow::Error,
    pub may_close: bool,
}

impl Failure {
    fn local(error: anyhow::Error) -> Self {
        Self {
            error,
            may_close: true,
        }
    }
}

/// A dropped connection stays recognisable to `dropped_by_phone`, so a
/// read-only capture may start again on a new connection.
fn failed(error: &IdeviceError, message: String) -> anyhow::Error {
    if unserved(error) {
        anyhow::Error::new(Dropped).context(message)
    } else {
        anyhow::anyhow!(message)
    }
}

pub async fn bounded<T>(
    deadline: Instant,
    name: &str,
    operation: impl Future<Output = std::result::Result<T, IdeviceError>>,
) -> std::result::Result<T, Failure> {
    if Instant::now() >= deadline {
        return Err(Failure {
            error: anyhow::anyhow!("{name}: session deadline expired"),
            may_close: false,
        });
    }
    match timeout_at(deadline.min(Instant::now() + OPERATION), operation).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            let may_close = complete_status(&error);
            Err(Failure {
                error: failed(&error, format!("{name}: {error:?}")),
                may_close,
            })
        }
        Err(_) => Err(Failure {
            error: anyhow::anyhow!("{name}: timed out; session discarded"),
            may_close: false,
        }),
    }
}

pub async fn bounded_controlled<T>(
    deadline: Instant,
    name: &str,
    operation: impl Future<Output = std::result::Result<T, IdeviceError>>,
    cancelled: Arc<AtomicBool>,
) -> std::result::Result<T, Failure> {
    let stopped = || cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline;
    if stopped() {
        return Err(Failure {
            error: anyhow::anyhow!("controlled copy cancelled/deadline expired"),
            may_close: false,
        });
    }
    let cancel = async {
        while !cancelled.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let result = tokio::select! {
        value = timeout_at(deadline.min(Instant::now() + OPERATION), operation) => value,
        _ = cancel => return Err(Failure { error: anyhow::anyhow!("controlled copy cancelled"),
            may_close: false }),
    };
    if stopped() {
        return Err(Failure {
            error: anyhow::anyhow!("controlled copy cancelled/deadline expired"),
            may_close: false,
        });
    }
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            let may_close = complete_status(&error);
            // Classify without rendering an upstream body/string. Exact AFC
            // status remains in the retained attributes/status observations.
            let class = if may_close {
                "complete AFC status"
            } else {
                "uncertain transport"
            };
            Err(Failure {
                error: failed(&error, format!("{name}: {class}")),
                may_close,
            })
        }
        Err(_) => Err(Failure {
            error: anyhow::anyhow!("controlled copy operation timeout"),
            may_close: false,
        }),
    }
}

pub type OpResult = std::result::Result<(), Failure>;

/// Open `path` read-only and return its handle. With `cancelled` the request
/// is a controlled operation and renders no upstream error text. The error
/// keeps the original value, so `dropped_by_phone` still recognises a dropped
/// connection. A reply that is not a plain open acknowledgement leaves the
/// handle unknown: discard the connection.
pub async fn open_readonly(
    afc: &mut AfcClient,
    path: &str,
    deadline: Instant,
    cancelled: Option<Arc<AtomicBool>>,
    name: &str,
) -> Result<u64> {
    let mut header = (AfcFopenMode::RdOnly as u64).to_le_bytes().to_vec();
    header.extend_from_slice(path.as_bytes());
    let operation = afc.send_op(AfcOpcode::FileOpen, header, Vec::new());
    let opened = match cancelled {
        Some(flag) => bounded_controlled(deadline, name, operation, flag).await,
        None => bounded(deadline, name, operation).await,
    }
    .map_err(|failure| failure.error)?;
    ensure!(
        opened.header.operation == AfcOpcode::FileOpenRes
            && opened.header_payload.len() == 8
            && opened.payload.is_empty(),
        "{name}: invalid open acknowledgement; connection discarded"
    );
    Ok(u64::from_le_bytes(opened.header_payload[..].try_into()?))
}

pub async fn transfer(
    afc: &mut AfcClient,
    handle: u64,
    output: &mut impl Write,
    size: u64,
    limits: CopyLimits,
    deadline: Instant,
) -> (OpResult, Option<OpResult>) {
    transfer_options(afc, handle, output, size, limits, deadline, None).await
}

pub async fn transfer_controlled(
    afc: &mut AfcClient,
    handle: u64,
    output: &mut impl Write,
    size: u64,
    limits: CopyLimits,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> (OpResult, Option<OpResult>) {
    transfer_options(afc, handle, output, size, limits, deadline, Some(cancelled)).await
}

/// `open_readonly` then `transfer`, with the copy and the close outcomes apart
/// because evidence records them separately. An open that fails is the error;
/// nothing was copied.
#[allow(clippy::too_many_arguments)]
pub async fn read_file(
    afc: &mut AfcClient,
    path: &str,
    output: &mut impl Write,
    size: u64,
    limits: CopyLimits,
    deadline: Instant,
    cancelled: Option<Arc<AtomicBool>>,
    name: &str,
) -> Result<(OpResult, Option<OpResult>)> {
    let handle = open_readonly(afc, path, deadline, cancelled.clone(), name).await?;
    Ok(transfer_options(afc, handle, output, size, limits, deadline, cancelled).await)
}

async fn transfer_options(
    afc: &mut AfcClient,
    handle: u64,
    output: &mut impl Write,
    size: u64,
    limits: CopyLimits,
    deadline: Instant,
    cancelled: Option<Arc<AtomicBool>>,
) -> (OpResult, Option<OpResult>) {
    let copied = copy_chunks(
        afc,
        handle,
        output,
        size,
        limits,
        deadline,
        cancelled.clone(),
    )
    .await;
    if copied.as_ref().is_err_and(|failure| !failure.may_close) {
        return (copied, None);
    }
    let operation = afc.send_op(
        AfcOpcode::FileClose,
        handle.to_le_bytes().to_vec(),
        Vec::new(),
    );
    let close = match cancelled {
        Some(flag) => bounded_controlled(deadline, "close", operation, flag).await,
        None => bounded(deadline, "close", operation).await,
    }
    .and_then(|reply| {
        if reply.header.operation == AfcOpcode::Status
            && reply.header_payload == [0; 8]
            && reply.payload.is_empty()
        {
            Ok(())
        } else {
            Err(Failure {
                error: anyhow::anyhow!("unexpected close reply; acknowledgement unproved"),
                may_close: false,
            })
        }
    });
    (copied, Some(close))
}

async fn copy_chunks(
    afc: &mut AfcClient,
    handle: u64,
    output: &mut impl Write,
    size: u64,
    limits: CopyLimits,
    deadline: Instant,
    cancelled: Option<Arc<AtomicBool>>,
) -> OpResult {
    if size > limits.maximum_size {
        return Err(Failure::local(anyhow::anyhow!(
            "source exceeds configured copy limit"
        )));
    }
    let mut remaining = size;
    let mut reads = 0;
    while remaining > 0 {
        if reads == limits.maximum_reads {
            return Err(Failure::local(anyhow::anyhow!(
                "read-operation limit reached"
            )));
        }
        let want = remaining.min(CHUNK as u64) as usize;
        let args = [handle.to_le_bytes(), (want as u64).to_le_bytes()].concat();
        reads += 1;
        let operation = afc.send_op(AfcOpcode::Read, args, Vec::new());
        let reply = match &cancelled {
            Some(flag) => bounded_controlled(deadline, "read chunk", operation, flag.clone()).await,
            None => bounded(deadline, "read chunk", operation).await,
        }?;
        if reply.header.operation != AfcOpcode::Data || !reply.header_payload.is_empty() {
            return Err(Failure {
                error: anyhow::anyhow!("unexpected read response; session discarded"),
                may_close: false,
            });
        }
        append_chunk(output, &reply.payload, want).map_err(Failure::local)?;
        remaining -= reply.payload.len() as u64;
    }
    Ok(())
}

pub fn append_chunk(output: &mut impl Write, bytes: &[u8], requested: usize) -> Result<()> {
    ensure!(
        !bytes.is_empty(),
        "premature EOF; partial raw bytes retained"
    );
    ensure!(
        bytes.len() <= requested,
        "reply exceeds requested chunk; overrun was not appended"
    );
    output
        .write_all(bytes)
        .context("writing raw bytes; partial output retained")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::afc::{
        dropped_by_phone,
        duplex::{pair, reply, status},
    };
    use idevice::{Idevice, afc::packet::AfcPacket};

    const PATH: &str = "iTunes_Control/iTunes/File.sqlitedb";
    const HANDLE: u64 = 42;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    fn limits() -> CopyLimits {
        CopyLimits {
            maximum_size: 1 << 20,
            maximum_reads: 8,
        }
    }
    /// `Some` flag for the controlled variant, `None` for the plain one.
    fn mode(controlled: bool) -> Option<Arc<AtomicBool>> {
        controlled.then(|| Arc::new(AtomicBool::new(false)))
    }

    async fn open_request(peer: &mut Idevice) -> AfcPacket {
        let request = AfcPacket::read(peer).await.unwrap();
        assert_eq!(request.header.operation, AfcOpcode::FileOpen);
        // The mode word is 1 (read-only), then the path.
        assert_eq!(
            request.header_payload,
            [1u64.to_le_bytes().as_slice(), PATH.as_bytes()].concat()
        );
        assert!(request.payload.is_empty());
        request
    }
    async fn accepts_open(peer: &mut Idevice) {
        let request = open_request(peer).await;
        reply(
            peer,
            &request,
            AfcOpcode::FileOpenRes,
            HANDLE.to_le_bytes().to_vec(),
            Vec::new(),
        )
        .await;
    }
    async fn serves_read(peer: &mut Idevice, wanted: u64, bytes: &[u8]) {
        let request = AfcPacket::read(peer).await.unwrap();
        assert_eq!(request.header.operation, AfcOpcode::Read);
        assert_eq!(
            request.header_payload,
            [HANDLE.to_le_bytes(), wanted.to_le_bytes()].concat()
        );
        reply(peer, &request, AfcOpcode::Data, Vec::new(), bytes.to_vec()).await;
    }
    async fn accepts_close(peer: &mut Idevice) {
        let request = AfcPacket::read(peer).await.unwrap();
        assert_eq!(request.header.operation, AfcOpcode::FileClose);
        assert_eq!(request.header_payload, HANDLE.to_le_bytes());
        status(peer, &request, 0).await;
    }

    #[tokio::test]
    async fn open_readonly_sends_one_read_only_open_and_returns_the_handle() {
        for controlled in [false, true] {
            let (mut afc, mut peer) = pair();
            let server = tokio::spawn(async move {
                accepts_open(&mut peer).await;
                assert!(peer.read_raw(1).await.is_err());
            });
            let handle = open_readonly(&mut afc, PATH, soon(), mode(controlled), "open test")
                .await
                .unwrap();
            assert_eq!(handle, HANDLE);
            drop(afc);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn open_readonly_refuses_a_reply_that_is_not_a_plain_acknowledgement() {
        let handle = HANDLE.to_le_bytes().to_vec();
        for (operation, header, payload) in [
            (AfcOpcode::Data, handle.clone(), Vec::new()),
            (AfcOpcode::FileOpenRes, vec![0; 7], Vec::new()),
            (AfcOpcode::FileOpenRes, vec![0; 9], Vec::new()),
            (AfcOpcode::FileOpenRes, handle.clone(), vec![1]),
        ] {
            let (mut afc, mut peer) = pair();
            let server = tokio::spawn(async move {
                let request = open_request(&mut peer).await;
                reply(&mut peer, &request, operation, header, payload).await;
            });
            let error = open_readonly(&mut afc, PATH, soon(), None, "open test")
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "open test: invalid open acknowledgement; connection discarded"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn open_readonly_failures_render_upstream_text_only_when_not_controlled() {
        for controlled in [false, true] {
            let (mut afc, mut peer) = pair();
            let server = tokio::spawn(async move {
                let request = open_request(&mut peer).await;
                status(&mut peer, &request, 8).await;
            });
            let error = open_readonly(&mut afc, PATH, soon(), mode(controlled), "open test")
                .await
                .unwrap_err();
            let shown = format!("{error:#}");
            if controlled {
                assert_eq!(shown, "open test: complete AFC status");
            } else {
                assert!(shown.starts_with("open test: ") && shown.contains("ObjectNotFound"));
            }
            assert!(!dropped_by_phone(&error));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn open_readonly_keeps_a_dropped_connection_recognisable() {
        for controlled in [false, true] {
            let (mut afc, peer) = pair();
            drop(peer);
            let error = open_readonly(&mut afc, PATH, soon(), mode(controlled), "open test")
                .await
                .unwrap_err();
            assert!(dropped_by_phone(&error), "{error:#}");
        }
    }

    #[tokio::test]
    async fn open_readonly_sends_nothing_after_the_deadline_or_a_cancellation() {
        let (mut afc, mut peer) = pair();
        let error = open_readonly(&mut afc, PATH, Instant::now(), None, "open test")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "open test: session deadline expired");
        let flag = Arc::new(AtomicBool::new(true));
        let error = open_readonly(&mut afc, PATH, soon(), Some(flag), "open test")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "controlled copy cancelled/deadline expired"
        );
        drop(afc);
        assert!(peer.read_raw(1).await.is_err());
    }

    #[tokio::test]
    async fn read_file_opens_copies_and_closes_keeping_both_outcomes() {
        for controlled in [false, true] {
            let (mut afc, mut peer) = pair();
            let server = tokio::spawn(async move {
                accepts_open(&mut peer).await;
                serves_read(&mut peer, 5, b"abc").await;
                serves_read(&mut peer, 2, b"de").await;
                accepts_close(&mut peer).await;
                assert!(peer.read_raw(1).await.is_err());
            });
            let mut output = Vec::new();
            let (copied, closed) = read_file(
                &mut afc,
                PATH,
                &mut output,
                5,
                limits(),
                soon(),
                mode(controlled),
                "open test",
            )
            .await
            .unwrap();
            assert!(copied.is_ok());
            assert!(matches!(closed, Some(Ok(()))));
            assert_eq!(output, b"abcde");
            drop(afc);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn read_file_that_fails_to_open_reads_and_closes_nothing() {
        let (mut afc, mut peer) = pair();
        let server = tokio::spawn(async move {
            let request = open_request(&mut peer).await;
            status(&mut peer, &request, 8).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        let mut output = Vec::new();
        let error = read_file(
            &mut afc,
            PATH,
            &mut output,
            5,
            limits(),
            soon(),
            None,
            "open test",
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").starts_with("open test: "));
        assert!(output.is_empty());
        drop(afc);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_file_closes_after_a_local_refusal_but_not_after_an_uncertain_failure() {
        // Over the size limit nothing is read; the open handle is still closed.
        let (mut afc, mut peer) = pair();
        let server = tokio::spawn(async move {
            accepts_open(&mut peer).await;
            accepts_close(&mut peer).await;
            assert!(peer.read_raw(1).await.is_err());
        });
        let mut output = Vec::new();
        let (copied, closed) = read_file(
            &mut afc,
            PATH,
            &mut output,
            limits().maximum_size + 1,
            limits(),
            soon(),
            None,
            "open test",
        )
        .await
        .unwrap();
        assert!(copied.unwrap_err().may_close);
        assert!(matches!(closed, Some(Ok(()))));
        drop(afc);
        server.await.unwrap();

        // A read the phone never answers is uncertain: no close follows, and
        // the bytes accepted before it stay in the output.
        for controlled in [false, true] {
            let (mut afc, mut peer) = pair();
            let server = tokio::spawn(async move {
                accepts_open(&mut peer).await;
                serves_read(&mut peer, 5, b"abc").await;
                let request = AfcPacket::read(&mut peer).await.unwrap();
                assert_eq!(request.header.operation, AfcOpcode::Read);
                assert!(peer.read_raw(1).await.is_err());
            });
            let mut output = Vec::new();
            let deadline = Instant::now() + Duration::from_millis(200);
            let (copied, closed) = read_file(
                &mut afc,
                PATH,
                &mut output,
                5,
                limits(),
                deadline,
                mode(controlled),
                "open test",
            )
            .await
            .unwrap();
            assert!(!copied.unwrap_err().may_close);
            assert!(closed.is_none());
            assert_eq!(output, b"abc");
            drop(afc);
            server.await.unwrap();
        }
    }
}
