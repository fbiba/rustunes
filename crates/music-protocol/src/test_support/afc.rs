//! A phone's AFC side over an offline duplex, serving an in-memory tree:
//! enough of afcd for whole write, rename, listing and read sequences. A
//! test can set what a stat reports, cut read replies short, count the
//! requests a caller made and change the tree between two requests.
use crate::protocol::transfer::{Attributes, CaptureControl, Transfer};
use anyhow::Result;
use idevice::{
    Idevice,
    afc::{
        MAGIC,
        opcode::AfcOpcode,
        packet::{AfcPacket, AfcPacketHeader},
    },
};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Directory,
    File(Vec<u8>),
}

/// A request the phone is about to serve: its position among every request
/// it was sent, its operation, and the path it names (for a read, write,
/// resize or close, the path its handle was opened on).
pub struct Request {
    pub index: usize,
    pub operation: AfcOpcode,
    pub path: Option<String>,
}

type Hook = Box<dyn FnMut(&Request, &mut FakePhone) + Send>;

pub struct FakePhone {
    pub tree: BTreeMap<String, Node>,
    /// Every request's opcode, in order.
    pub log: Vec<AfcOpcode>,
    /// The most bytes one Read reply carries. The phone may answer a read
    /// with fewer bytes than were asked for.
    pub read_limit: usize,
    /// Per path, stat keys that replace the reported ones (`Some`) or are
    /// left out (`None`).
    attributes: BTreeMap<String, BTreeMap<String, Option<String>>>,
    /// Each open handle's path and how far it has been read.
    handles: BTreeMap<u64, (String, usize)>,
    next: u64,
    hook: Option<Hook>,
}

impl FakePhone {
    pub fn new(directories: &[&str]) -> Self {
        Self {
            tree: directories
                .iter()
                .map(|path| ((*path).to_owned(), Node::Directory))
                .collect(),
            log: Vec::new(),
            read_limit: usize::MAX,
            attributes: BTreeMap::new(),
            handles: BTreeMap::new(),
            next: 1,
            hook: None,
        }
    }

    /// Put a file with `bytes` at `path`, replacing whatever was there.
    pub fn file(&mut self, path: &str, bytes: &[u8]) {
        self.tree
            .insert(path.to_owned(), Node::File(bytes.to_vec()));
    }

    /// Report `value` for `key` in every later stat of `path`, or leave the
    /// key out with `None`. A file's default stat is `S_IFREG`, its size and
    /// `st_mtime` 2; a directory's is `S_IFDIR`, size 64 and `st_mtime` 1.
    pub fn set_attribute(&mut self, path: &str, key: &str, value: Option<&str>) {
        self.attributes
            .entry(path.to_owned())
            .or_default()
            .insert(key.to_owned(), value.map(str::to_owned));
    }

    /// What a stat of `path` reports now, or None when nothing is there.
    pub fn stat(&self, path: &str) -> Option<Attributes> {
        let defaults = match self.tree.get(path)? {
            Node::Directory => [
                ("st_ifmt", "S_IFDIR".to_owned()),
                ("st_size", "64".to_owned()),
                ("st_mtime", "1".to_owned()),
            ],
            Node::File(bytes) => [
                ("st_ifmt", "S_IFREG".to_owned()),
                ("st_size", bytes.len().to_string()),
                ("st_mtime", "2".to_owned()),
            ],
        };
        let mut attributes: Attributes = defaults
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        for (key, value) in self.attributes.get(path).into_iter().flatten() {
            match value {
                Some(value) => attributes.insert(key.clone(), value.clone()),
                None => attributes.remove(key),
            };
        }
        Some(attributes)
    }

    /// Call `hook` before each request is served, with the request and this
    /// phone, so a test can add, change or remove files between requests.
    pub fn before_each(&mut self, hook: impl FnMut(&Request, &mut FakePhone) + Send + 'static) {
        self.hook = Some(Box::new(hook));
    }

    /// A connection to this phone, as `Transfer::from_test_duplex` makes it,
    /// with the phone serving it until the connection is dropped; the task
    /// then returns the phone.
    pub fn connect(
        self,
        trace: &Path,
        control: &CaptureControl,
        operations: usize,
    ) -> Result<(Transfer, tokio::task::JoinHandle<FakePhone>)> {
        let (endpoint, peer) = tokio::io::duplex(64 * 1024);
        let transfer = Transfer::from_test_duplex(endpoint, trace, control, operations)?;
        let server = tokio::spawn(self.serve(Idevice::new(Box::new(peer), "offline-afc-peer")));
        Ok((transfer, server))
    }

    fn parent_is_directory(&self, path: &str) -> bool {
        match path.rsplit_once('/') {
            None => true,
            Some((parent, _)) => self.tree.get(parent) == Some(&Node::Directory),
        }
    }

    fn handle(header: &[u8]) -> u64 {
        u64::from_le_bytes(header[..8].try_into().unwrap())
    }

    /// The path `request` names, before it is served.
    fn named_path(&self, request: &AfcPacket) -> Option<String> {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        let header = &request.header_payload;
        match request.header.operation {
            AfcOpcode::GetFileInfo
            | AfcOpcode::ReadDir
            | AfcOpcode::RemovePath
            | AfcOpcode::MakeDir => Some(text(header)),
            AfcOpcode::FileOpen => Some(text(&header[8..])),
            AfcOpcode::RenamePath => header.split(|b| *b == 0).next().map(text),
            AfcOpcode::Read | AfcOpcode::Write | AfcOpcode::FileSetSize | AfcOpcode::FileClose => {
                self.handles
                    .get(&Self::handle(header))
                    .map(|(path, _)| path.clone())
            }
            _ => None,
        }
    }

    /// Serve requests until the host closes its end.
    pub async fn serve(mut self, mut peer: Idevice) -> Self {
        while let Ok(request) = AfcPacket::read(&mut peer).await {
            let operation = request.header.operation;
            if let Some(mut hook) = self.hook.take() {
                let about = Request {
                    index: self.log.len(),
                    operation,
                    path: self.named_path(&request),
                };
                hook(&about, &mut self);
                self.hook = Some(hook);
            }
            self.log.push(operation);
            let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
            let (code, reply) = match operation {
                AfcOpcode::GetFileInfo => {
                    let path = text(&request.header_payload);
                    match self.stat(&path) {
                        None => (8, None),
                        Some(attributes) => {
                            let mut payload = Vec::new();
                            for (key, value) in attributes {
                                for part in [key, value] {
                                    payload.extend_from_slice(part.as_bytes());
                                    payload.push(0);
                                }
                            }
                            (0, Some(payload))
                        }
                    }
                }
                AfcOpcode::FileOpen => {
                    let mode = Self::handle(&request.header_payload);
                    let path = text(&request.header_payload[8..]);
                    let exists = matches!(self.tree.get(&path), Some(Node::File(_)));
                    if mode == 1 && !exists || !self.parent_is_directory(&path) {
                        (8, None)
                    } else {
                        if !exists {
                            self.tree.insert(path.clone(), Node::File(Vec::new()));
                        }
                        let handle = self.next;
                        self.next += 1;
                        self.handles.insert(handle, (path, 0));
                        reply_packet(
                            &mut peer,
                            &request,
                            AfcOpcode::FileOpenRes,
                            handle.to_le_bytes().to_vec(),
                            Vec::new(),
                        )
                        .await;
                        continue;
                    }
                }
                AfcOpcode::Write => {
                    let path = self.handles[&Self::handle(&request.header_payload)]
                        .0
                        .clone();
                    if let Some(Node::File(bytes)) = self.tree.get_mut(&path) {
                        bytes.extend_from_slice(&request.payload);
                    }
                    (0, None)
                }
                AfcOpcode::FileSetSize => {
                    let path = self.handles[&Self::handle(&request.header_payload)]
                        .0
                        .clone();
                    let size =
                        u64::from_le_bytes(request.header_payload[8..16].try_into().unwrap());
                    if let Some(Node::File(bytes)) = self.tree.get_mut(&path) {
                        bytes.resize(size as usize, 0);
                    }
                    (0, None)
                }
                AfcOpcode::Read => {
                    let want =
                        u64::from_le_bytes(request.header_payload[8..16].try_into().unwrap());
                    let (path, offset) = self
                        .handles
                        .get_mut(&Self::handle(&request.header_payload))
                        .unwrap();
                    let Some(Node::File(bytes)) = self.tree.get(path.as_str()) else {
                        panic!("read of {path}");
                    };
                    // From where the handle stands, as a file reads: a file
                    // changed between two reads gives its new bytes.
                    let start = (*offset).min(bytes.len());
                    let end = start
                        .saturating_add(want as usize)
                        .min(start.saturating_add(self.read_limit))
                        .min(bytes.len());
                    *offset = end;
                    let bytes = bytes[start..end].to_vec();
                    reply_packet(&mut peer, &request, AfcOpcode::Data, Vec::new(), bytes).await;
                    continue;
                }
                AfcOpcode::FileClose => {
                    self.handles.remove(&Self::handle(&request.header_payload));
                    (0, None)
                }
                AfcOpcode::RenamePath => {
                    let parts: Vec<_> = request.header_payload.split(|b| *b == 0).collect();
                    let (from, to) = (text(parts[0]), text(parts[1]));
                    match self.tree.remove(&from) {
                        Some(node) => {
                            self.tree.insert(to, node);
                            (0, None)
                        }
                        None => (8, None),
                    }
                }
                AfcOpcode::MakeDir => {
                    let path = text(&request.header_payload);
                    if self.parent_is_directory(&path) {
                        self.tree.entry(path).or_insert(Node::Directory);
                        (0, None)
                    } else {
                        (8, None)
                    }
                }
                AfcOpcode::ReadDir => {
                    let path = text(&request.header_payload);
                    let prefix = format!("{path}/");
                    let mut names = b".\0..\0".to_vec();
                    for name in self.tree.keys().filter_map(|key| key.strip_prefix(&prefix)) {
                        if !name.contains('/') {
                            names.extend_from_slice(name.as_bytes());
                            names.push(0);
                        }
                    }
                    (0, Some(names))
                }
                AfcOpcode::RemovePath => {
                    let path = text(&request.header_payload);
                    (
                        if self.tree.remove(&path).is_some() {
                            0
                        } else {
                            8
                        },
                        None,
                    )
                }
                other => panic!("fake phone does not serve {other:?}"),
            };
            match reply {
                Some(payload) => {
                    reply_packet(&mut peer, &request, AfcOpcode::Data, Vec::new(), payload).await
                }
                None => {
                    reply_packet(
                        &mut peer,
                        &request,
                        AfcOpcode::Status,
                        u64::to_le_bytes(code).to_vec(),
                        Vec::new(),
                    )
                    .await
                }
            }
        }
        self
    }
}

async fn reply_packet(
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
