//! Initiating side of native HyperConnect file sharing. Clipboard offers are
//! deliberately not involved: the receiver accepts and saves a share task.
use crate::{
    file_transport as ft,
    wire::{self, Field},
};
use std::{
    io::Write,
    net::{SocketAddr, TcpStream, UdpSocket},
    time::{Duration, Instant},
};

struct Output(UdpSocket);
impl Write for Output {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.send(data)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn number(input: &[u8], id: u32) -> anyhow::Result<u64> {
    let fields = wire::fields(input).ok_or_else(|| anyhow::anyhow!("Malformed share response"))?;
    let mut values = fields
        .iter()
        .filter(|f| matches!(f, Field::Integer(n, _) | Field::Bytes(n, _) if *n == id));
    let value = match values.next() {
        Some(Field::Integer(_, v)) => *v,
        None => 0,
        _ => anyhow::bail!("Invalid share integer"),
    };
    anyhow::ensure!(values.next().is_none(), "Duplicate share integer");
    Ok(value)
}
fn bytes(input: &[u8], id: u32) -> anyhow::Result<&[u8]> {
    wire::bytes_field(input, id).ok_or_else(|| anyhow::anyhow!("Missing share response field {id}"))
}
fn accept_response(response: &[u8], task: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(number(response, 1)? == 1, "Wrong share request kind");
    let fields =
        wire::fields(response).ok_or_else(|| anyhow::anyhow!("Malformed share acceptance"))?;
    let mut ids = fields
        .iter()
        .filter(|f| matches!(f, Field::Integer(2, _) | Field::Bytes(2, _)));
    // Android acknowledges the single request on this authenticated channel
    // with just its kind. The task echo is optional; an explicit different task
    // must still never authorize transfer.
    match ids.next() {
        None => (),
        Some(Field::Bytes(_, value)) if value.is_empty() || *value == task => (),
        _ => anyhow::bail!("Wrong share task"),
    }
    anyhow::ensure!(ids.next().is_none(), "Duplicate share task");
    anyhow::ensure!(number(response, 3)? == 0, "Phone declined file sharing");
    Ok(())
}
fn node<'a>(nodes: &[ft::Node<'a>], tag: u16, kind: u16) -> anyhow::Result<&'a [u8]> {
    let mut values = nodes.iter().filter(|n| n.tag == tag);
    let n = values
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing share transport field"))?;
    anyhow::ensure!(
        n.kind == kind && values.next().is_none(),
        "Invalid share transport field"
    );
    Ok(n.value)
}

pub(crate) struct Sender {
    pub request: crate::share_queue::Request,
    pub nonce: u32,
    pub key: [u8; 32],
    user_key: [u8; 32],
    client_id: u32,
    pub peer: Option<u8>,
    started: Instant,
    activity: Instant,
    reliable: Option<kcp::Kcp<Output>>,
    socket: Option<UdpSocket>,
    address: Option<SocketAddr>,
    stream: Option<TcpStream>,
    data_key: Option<[u8; 16]>,
    task: String,
    accepted: bool,
    announced: bool,
    streams: Vec<(u32, usize, u64, bool, bool)>,
    saved_streams: Vec<u32>,
    finish_sent: bool,
    pending: Vec<u8>,
    pending_stream: u32,
    pending_offset: usize,
    assembler: ft::BytesAssembler,
    complete: bool,
}
impl Sender {
    pub fn new(request: crate::share_queue::Request) -> anyhow::Result<Self> {
        let now = Instant::now();
        crate::share_queue::validate_selected(request.target)?;
        Ok(Self {
            request,
            nonce: u32::from_ne_bytes(crate::private::random()?) & 0x7fff_ffff,
            key: crate::private::random()?,
            user_key: crate::private::random()?,
            client_id: 1,
            peer: None,
            started: now,
            activity: now,
            reliable: None,
            socket: None,
            address: None,
            stream: None,
            data_key: None,
            task: format!("{:016x}", u64::from_ne_bytes(crate::private::random()?)),
            accepted: false,
            announced: false,
            streams: Vec::new(),
            saved_streams: Vec::new(),
            finish_sent: false,
            pending: Vec::new(),
            pending_stream: 0,
            pending_offset: 0,
            assembler: ft::BytesAssembler::default(),
            complete: false,
        })
    }
    pub fn logical_request(&self) -> Vec<u8> {
        use base64::Engine;
        let mut config = wire::integer(1, self.client_id.into());
        config.extend(wire::message(4, &self.key));
        config.extend(wire::message(5, &self.user_key));
        // UserInfo field 1 identifies the OS (Darwin = 5), not the UDP
        // transport type used later in RequestOfPeerPort.
        let mut user = wire::integer(1, 5);
        user.extend(wire::message(2, crate::networking::MAC_PACKAGE.as_bytes()));
        // The phone's Express listener requires its link-request envelope in
        // UserInfo.userData before it will accept the underlying SDK channel.
        let link = ft::union(1, 0x100, &ft::tlv(5, 0, b"").unwrap()).unwrap();
        user.extend(wire::message(
            4,
            base64::engine::general_purpose::STANDARD
                .encode(link)
                .as_bytes(),
        ));
        user.extend(wire::message(10, &config));
        // SDK channels require the receiver's confirmation callback to run.
        // Without this flag the phone rejects the connection before Express
        // can parse the link request or present the file-sharing prompt.
        let mut body = wire::integer(1, 1);
        body.extend(wire::message(
            2,
            crate::file_channel::PHONE_SHARE_SERVICE.as_bytes(),
        ));
        body.extend(wire::message(3, &user));
        body.extend(wire::integer(4, 48));
        body.extend(wire::integer(5, 10000));
        body.extend(wire::integer(6, 128));
        body.extend(wire::integer(7, 1));
        let mut out = wire::integer(1, 1);
        out.extend(wire::message(2, &body));
        out
    }
    pub fn port_request(&self) -> anyhow::Result<Vec<u8>> {
        let mut body = wire::integer(1, self.client_id.into());
        body.extend(wire::integer(2, 1));
        body.extend(wire::integer(3, self.nonce.into()));
        body.extend(wire::message(4, &self.key));
        body.extend(wire::message(5, &self.user_key));
        crate::file_channel::control_packet(2, 0, &body)
    }
    pub fn connect(
        &mut self,
        control: &crate::file_channel::Control<'_>,
        peer: SocketAddr,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.reliable.is_none() && control.kind == 3,
            "Unexpected outgoing port response"
        );
        anyhow::ensure!(
            number(control.body, 1)? == u64::from(self.client_id),
            "Wrong share client identity"
        );
        let server = u32::try_from(number(control.body, 2)?)?;
        let port = u16::try_from(number(control.body, 3)?)?;
        anyhow::ensure!(server != 0 && port != 0, "Invalid share peer port");
        let address = SocketAddr::new(peer.ip(), port);
        let socket = UdpSocket::bind(if peer.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        socket.connect(address)?;
        socket.set_nonblocking(true)?;
        let mut reliable = kcp::Kcp::new(0x12345678, Output(socket.try_clone()?));
        reliable.set_nodelay(true, 20, 2, true);
        reliable.set_wndsize(32, 128);
        reliable.set_mtu(1400)?;
        let mut info = ft::tlv(3, 0, &server.to_be_bytes())?;
        info.extend(ft::tlv(3, 1, &1u32.to_be_bytes())?);
        info.extend(ft::tlv(3, 2, &0xff00u32.to_be_bytes())?);
        reliable.send(&ft::union(0, 0x100, &info)?)?;
        self.socket = Some(socket);
        self.reliable = Some(reliable);
        self.address = Some(address);
        self.activity = Instant::now();
        eprintln!("Mi Clip Share: peer port accepted; starting encrypted transfer channel");
        Ok(())
    }
    fn send(&mut self, message: &[u8]) -> anyhow::Result<()> {
        self.reliable
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Share transport unavailable"))?
            .send(&ft::seal(&self.key, message)?)?;
        Ok(())
    }
    fn manifest(&self) -> anyhow::Result<Vec<u8>> {
        let mut body = wire::integer(1, 1);
        body.extend(wire::message(2, crate::networking::DEVICE_NAME.as_bytes()));
        body.extend(wire::message(3, self.task.as_bytes()));
        for (i, source) in self.request.offer.files.iter().enumerate() {
            let mut file = wire::message(1, source.name.as_bytes());
            file.extend(wire::integer(2, source.size));
            file.extend(wire::message(3, format!("file_id_{i}").as_bytes()));
            file.extend(wire::message(5, source.mime.as_bytes()));
            body.extend(wire::message(5, &file));
        }
        let mut root = wire::integer(1, 1);
        root.extend(wire::message(2, &body));
        crate::file_protocol::request_with_limit(
            &root,
            crate::file_protocol::MAX_OUTGOING_SHARE_SIZE,
        )?;
        Ok(root)
    }
    fn announce_streams(&mut self) -> anyhow::Result<()> {
        for index in 0..self.request.offer.files.len() {
            let id = index as u32 + 1;
            let file = self.request.offer.check(index)?;
            let mut body = ft::tlv(3, 0, &0u32.to_be_bytes())?;
            body.extend(ft::tlv(3, 1, &id.to_be_bytes())?);
            body.extend(ft::tlv(4, 2, &(file.size as i64).to_be_bytes())?);
            body.extend(ft::tlv(5, 3, file.mime.as_bytes())?);
            body.extend(ft::tlv(5, 4, file.name.as_bytes())?);
            body.extend(ft::tlv(5, 5, format!("file_id_{index}").as_bytes())?);
            let express = ft::union(3, 0x100, &body)?;
            self.send(&ft::sdk_bytes(&express)?)?;
            self.streams.push((id, index, 0, false, false));
        }
        Ok(())
    }
    fn receive(&mut self, packet: &[u8]) -> anyhow::Result<()> {
        if ft::frame(packet).is_ok_and(|p| p.tag == 4) {
            // SDK server-info is plaintext; no file data is authorized by it.
            return Ok(());
        }
        let plaintext = ft::open(&self.key, packet)?;
        let frame = ft::frame(&plaintext)?;
        anyhow::ensure!(
            (frame.kind, frame.tag) == (0x100, 1),
            "Unsupported share event"
        );
        let fields = ft::nodes(frame.value)?;
        let express = node(&fields, 0, 5)?;
        let frame = ft::frame(express)?;
        match (frame.kind, frame.tag) {
            (0x100, 1) => {
                anyhow::ensure!(self.stream.is_none(), "Duplicate share socket offer");
                let fields = ft::nodes(frame.value)?;
                let port = u16::try_from(u32::from_be_bytes(node(&fields, 3, 3)?.try_into()?))?;
                let key: [u8; 16] = node(&fields, 4, 5)?.try_into()?;
                anyhow::ensure!(port != 0, "Invalid share data port");
                let address = SocketAddr::new(self.address.unwrap().ip(), port);
                let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
                stream.set_nonblocking(true)?;
                self.stream = Some(stream);
                self.data_key = Some(key);
                self.send(&ft::byte_message(0, &self.manifest()?)?)?;
                self.announced = true;
                eprintln!("Mi Clip Share: file manifest sent; waiting for phone acceptance");
                self.request.status("waiting", 0)?;
            }
            (0x100, 2) => {
                if let Some(message) = self.assembler.push(express)? {
                    if std::env::var_os("MICLIP_DIAGNOSTICS").is_some() {
                        let schema = |data: &[u8]| {
                            wire::fields(data)
                                .unwrap_or_default()
                                .iter()
                                .map(|field| match field {
                                    Field::Integer(id, value) => format!("{id}=integer({value})"),
                                    Field::Bytes(id, value) => {
                                        format!("{id}=bytes({})", value.len())
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        eprintln!(
                            "Mi Clip Share: control response schema: {}",
                            schema(&message)
                        );
                        if let Some(body) = wire::bytes_field(&message, 2) {
                            eprintln!("Mi Clip Share: response body schema: {}", schema(body));
                        }
                    }
                    match number(&message, 1)? {
                        2 => {
                            anyhow::ensure!(
                                self.announced && !self.accepted,
                                "Unexpected share acceptance"
                            );
                            let response = bytes(&message, 2)?;
                            accept_response(response, self.task.as_bytes())?;
                            self.accepted = true;
                            eprintln!("Mi Clip Share: phone accepted file manifest");
                            self.announce_streams()?;
                        }
                        8 => {
                            anyhow::ensure!(
                                self.accepted
                                    && number(bytes(&message, 2)?, 1)? == 1
                                    && self.streams.iter().all(|s| s.4)
                                    && self.pending.is_empty(),
                                "Premature share completion"
                            );
                            self.request.complete()?;
                            self.complete = true;
                        }
                        _ => anyhow::bail!("Phone cancelled or rejected file sharing"),
                    }
                }
            }
            (0x100, 4) => {
                let fields = ft::nodes(frame.value)?;
                let id = u32::from_be_bytes(node(&fields, 1, 3)?.try_into()?);
                anyhow::ensure!(
                    u32::from_be_bytes(node(&fields, 2, 3)?.try_into()?) == 2,
                    "Unsupported share transfer mode"
                );
                let stream = self
                    .streams
                    .iter_mut()
                    .find(|s| s.0 == id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown share stream"))?;
                anyhow::ensure!(!stream.3, "Duplicate share stream request");
                stream.3 = true;
            }
            (0x100, 5) => {
                let fields = ft::nodes(frame.value)?;
                let id = u32::from_be_bytes(node(&fields, 1, 3)?.try_into()?);
                let result = i32::from_be_bytes(node(&fields, 2, 3)?.try_into()?);
                // This is MiContPacketTransferState: NONE=0, SUCCESS=1.
                // It is not the manifest's RejectReason (where 0 accepts).
                anyhow::ensure!(result == 1, "Phone failed to save share stream ({result})");
                let stream = self
                    .streams
                    .iter()
                    .find(|s| s.0 == id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown share stream completion"))?;
                anyhow::ensure!(
                    stream.3 && stream.4 && (self.pending.is_empty() || self.pending_stream != id),
                    "Premature share stream completion"
                );
                // Stream receipt precedes the task's command 8. Keep waiting
                // for that overall confirmation before completing the request.
                if !self.saved_streams.contains(&id) {
                    self.saved_streams.push(id);
                }
                eprintln!("Mi Clip Share: phone confirmed a saved file stream");
            }
            _ => {
                let schema = ft::nodes(frame.value)?
                    .iter()
                    .map(|n| format!("{}:{}({})", n.tag, n.kind, n.value.len()))
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "Unsupported share response {:#x}/{}: {schema}",
                    frame.kind,
                    frame.tag
                );
            }
        }
        Ok(())
    }
    pub fn poll(&mut self) -> anyhow::Result<bool> {
        anyhow::ensure!(self.request.exists(), "Share cancelled");
        anyhow::ensure!(
            self.activity.elapsed() < Duration::from_secs(60)
                && (self.accepted || self.started.elapsed() < Duration::from_secs(600)),
            "Share timed out"
        );
        if let Some(socket) = self.socket.as_ref() {
            let mut buffer = [0; 65536];
            for _ in 0..32 {
                match socket.recv(&mut buffer) {
                    Ok(count) => {
                        ft::validate_datagram(&buffer[..count])?;
                        self.reliable.as_mut().unwrap().input(&buffer[..count])?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if let Some(reliable) = self.reliable.as_mut() {
            reliable.update(self.started.elapsed().as_millis() as u32)?;
            reliable.flush_ack()?;
        }
        for _ in 0..8 {
            let Some(reliable) = self.reliable.as_mut() else {
                break;
            };
            let Ok(size) = reliable.peeksize() else {
                break;
            };
            anyhow::ensure!(size <= ft::MAX_PACKET, "Oversized share event");
            let mut packet = vec![0; size];
            reliable.recv(&mut packet)?;
            self.activity = Instant::now();
            self.receive(&packet)?;
        }
        if self.complete {
            return Ok(true);
        }
        // Keep the existing record size and send a batch between control polls.
        for _ in 0..64 {
            if self.pending.is_empty() {
                let Some(s) = self.streams.iter_mut().find(|s| s.3 && !s.4) else {
                    break;
                };
                let bytes = self.request.offer.read(s.1, s.2)?;
                self.pending =
                    crate::file_stream::seal(self.data_key.as_ref().unwrap(), s.0, s.2, &bytes)?;
                s.2 += bytes.len() as u64;
                s.4 = bytes.is_empty();
                self.pending_stream = s.0;
                self.pending_offset = 0;
            }
            match self
                .stream
                .as_mut()
                .unwrap()
                .write(&self.pending[self.pending_offset..])
            {
                Ok(0) => anyhow::bail!("Share socket closed"),
                Ok(count) => {
                    self.pending_offset += count;
                    self.activity = Instant::now();
                    if self.pending_offset == self.pending.len() {
                        self.pending.clear();
                        self.request
                            .status("sending", self.streams.iter().map(|s| s.2).sum())?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        if self.accepted
            && !self.finish_sent
            && !self.streams.is_empty()
            && self.saved_streams.len() == self.streams.len()
            && self.streams.iter().all(|s| s.4)
            && self.pending.is_empty()
        {
            // Native FileSendComplete (7) is sent by the sender after every
            // successful stream receipt. Android then returns response 8 and
            // leaves its "receiving" state.
            let mut finish = wire::integer(1, 7);
            finish.extend(wire::message(2, &wire::integer(1, 1)));
            self.send(&ft::byte_message(0, &finish)?)?;
            self.finish_sent = true;
            eprintln!("Mi Clip Share: all file streams saved; task finish sent");
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn android_acceptance_can_omit_task_echo_but_cannot_change_or_reject_it() {
        // Actual Android acceptance body: request kind only, default reason 0.
        let android = [0x08, 0x01];
        accept_response(&android, b"task-one").unwrap();
        let mut echoed = android.to_vec();
        echoed.extend(wire::message(2, b"task-one"));
        accept_response(&echoed, b"task-one").unwrap();
        assert!(accept_response(&echoed, b"task-two").is_err());
        echoed.extend(wire::message(2, b"task-one"));
        assert!(accept_response(&echoed, b"task-one").is_err());
        let mut rejected = android.to_vec();
        rejected.extend(wire::integer(3, 1));
        assert!(accept_response(&rejected, b"task-one").is_err());
        assert!(accept_response(&[0x08, 0x02], b"task-one").is_err());
    }

    fn setup(sample_size: u64) -> (std::path::PathBuf, Sender) {
        let root = std::env::temp_dir().join(format!(
            "clip-share-test-{:016x}",
            u64::from_ne_bytes(crate::private::random().unwrap())
        ));
        let directory = root.join("outbox/test.ready");
        std::fs::create_dir_all(directory.join("files")).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut sample = std::fs::File::create(directory.join("files/sample.txt")).unwrap();
        sample.write_all(&vec![0x42; 70_001]).unwrap();
        sample.set_len(sample_size).unwrap();
        std::fs::write(directory.join("files/empty.bin"), b"").unwrap();
        std::fs::write(
            directory.join("request.json"),
            br#"{"files":["sample.txt","empty.bin"],"target":"127.0.0.1:1"}"#,
        )
        .unwrap();
        let request = crate::share_queue::Request::next(&root.join("outbox"))
            .unwrap()
            .unwrap();
        (root, Sender::new(request).unwrap())
    }
    #[test]
    fn direct_share_accepts_an_18_gib_file_and_reads_its_last_byte() {
        let size = 18 * 1024 * 1024 * 1024;
        let (root, sender) = setup(size);
        let packet = sender.manifest().unwrap();
        let manifest = crate::file_protocol::request_with_limit(
            &packet,
            crate::file_protocol::MAX_OUTGOING_SHARE_SIZE,
        )
        .unwrap();
        assert_eq!(manifest.files[0].size, size);
        assert_eq!(sender.request.offer.read(0, size - 1).unwrap(), [0]);
        assert!(sender.request.offer.read(0, size).unwrap().is_empty());
        assert!(crate::file_protocol::request(&packet).is_err());
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_share_request_contains_express_link_and_wifi_medium() {
        use base64::Engine;
        let (root, sender) = setup(70_001);
        let request = sender.logical_request();
        let body = bytes(&request, 2).unwrap();
        assert_eq!(number(body, 1).unwrap(), 1);
        assert_eq!(number(body, 4).unwrap(), 48);
        assert_eq!(number(body, 6).unwrap(), 128);
        let user = bytes(body, 3).unwrap();
        assert_eq!(number(user, 1).unwrap(), 5);
        assert_eq!(
            bytes(user, 2).unwrap(),
            crate::networking::MAC_PACKAGE.as_bytes()
        );
        let link = base64::engine::general_purpose::STANDARD
            .decode(bytes(user, 4).unwrap())
            .unwrap();
        // Express's TlvExpressLink selects request branch 1, with an empty
        // user-data string in field 0. An ordinary SDK channel omits this.
        assert_eq!(
            link,
            [
                1, 1, 255, 255, 0, 0, 0, 18, 0, 1, 1, 0, 0, 1, 0, 0, 0, 8, 0, 5, 0, 0, 0, 0, 0, 0
            ]
        );
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_share_saves_multiple_files_and_waits_for_receiver_confirmation() {
        let (root, mut sender) = setup(70_001);
        let local = "127.0.0.1".parse().unwrap();
        let mut receiver = crate::file_channel::FileReceiver::open(
            local,
            local,
            None,
            sender.key,
            Some(&root.join("downloads")),
        )
        .unwrap();
        let port_request = sender.port_request().unwrap();
        let request = crate::file_channel::port_request(
            &crate::file_channel::control(&port_request).unwrap(),
        )
        .unwrap();
        crate::file_channel::PortBinding::from_user_info(
            bytes(bytes(&sender.logical_request(), 2).unwrap(), 3).unwrap(),
        )
        .unwrap()
        .verify(&request)
        .unwrap();
        let response =
            crate::file_channel::port_response(&request, 1, receiver.port().unwrap()).unwrap();
        sender
            .connect(
                &crate::file_channel::control(&response).unwrap(),
                SocketAddr::new(local, 1),
            )
            .unwrap();
        assert!(!sender.poll().unwrap());
        assert!(!root.join("downloads/sample.txt").exists());
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            receiver.poll().unwrap();
            if sender.poll().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(sender.complete, "share did not receive completion");
        assert_eq!(
            std::fs::read(root.join("downloads/sample.txt")).unwrap(),
            vec![0x42; 70_001]
        );
        assert_eq!(
            std::fs::metadata(root.join("downloads/empty.bin"))
                .unwrap()
                .len(),
            0
        );
        let state: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("outbox/test.ready/status.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["state"], "completed");
        drop(sender);
        drop(receiver);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cancellation_and_premature_success_do_not_report_completion() {
        let (root, mut sender) = setup(70_001);
        let mut message = wire::integer(1, 8);
        message.extend(wire::message(2, &wire::integer(1, 1)));
        let complete = ft::byte_message(0, &message).unwrap();
        assert!(
            sender
                .receive(&ft::seal(&sender.key, &complete).unwrap())
                .is_err()
        );
        assert!(!sender.complete);
        std::fs::remove_dir_all(root.join("outbox/test.ready")).unwrap();
        assert!(sender.poll().is_err());
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn saved_stream_receipt_waits_for_task_completion_and_rejects_failure() {
        let (root, mut sender) = setup(70_001);
        let receipt = |code: i32, id: u32| {
            let mut body = ft::tlv(3, 0, &0u32.to_be_bytes()).unwrap();
            body.extend(ft::tlv(3, 1, &id.to_be_bytes()).unwrap());
            body.extend(ft::tlv(3, 2, &code.to_be_bytes()).unwrap());
            let express = ft::union(5, 0x100, &body).unwrap();
            ft::seal(&sender.key, &ft::sdk_bytes(&express).unwrap()).unwrap()
        };
        let okay = receipt(1, 1);
        let failure = receipt(3, 1);
        let unknown = receipt(1, 2);
        assert!(sender.receive(&okay).is_err());
        sender.accepted = true;
        sender.streams.push((1, 0, 70001, true, true));
        sender.receive(&okay).unwrap();
        assert!(!sender.complete);
        assert!(sender.receive(&failure).is_err());
        assert!(sender.receive(&unknown).is_err());
        drop(sender);
        std::fs::remove_dir_all(root).unwrap();
    }
}
