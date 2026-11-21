//! Bounded building blocks for the separately encrypted file channel.
//! These codecs do not authenticate a peer or publish received files.
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};

pub(crate) const MAX_PACKET: usize = u16::MAX as usize;
const OVERHEAD: usize = 4 + 12 + 16;

/// Validate the native event transport's KCP segments before handing them to
/// the reliability engine. The public conversation number is not a credential.
#[cfg(feature = "file-sharing")]
pub(crate) fn validate_datagram(mut packet: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !packet.is_empty() && packet.len() <= MAX_PACKET,
        "Invalid bulk datagram size"
    );
    let mut count = 0;
    while !packet.is_empty() {
        count += 1;
        anyhow::ensure!(
            count <= 64 && packet.len() >= 24,
            "Invalid reliable bulk segment"
        );
        anyhow::ensure!(
            u32::from_le_bytes(packet[..4].try_into()?) == 0x12345678,
            "Unknown reliable bulk conversation"
        );
        let command = packet[4];
        let length = usize::try_from(u32::from_le_bytes(packet[20..24].try_into()?))?;
        anyhow::ensure!(
            (81..=84).contains(&command),
            "Unsupported reliable bulk command"
        );
        anyhow::ensure!(packet[5] < 128, "Too many reliable bulk fragments");
        anyhow::ensure!(
            length <= 2048 && length <= packet.len() - 24,
            "Invalid reliable bulk segment length"
        );
        anyhow::ensure!(
            command == 81 || length == 0,
            "Unexpected reliable bulk control payload"
        );
        packet = &packet[24 + length..];
    }
    Ok(())
}

pub(crate) fn seal(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        !plaintext.is_empty() && plaintext.len() <= MAX_PACKET - OVERHEAD,
        "Invalid bulk packet size"
    );
    let nonce = crate::private::random::<12>()?;
    let mut packet = vec![0x81, 0x04];
    packet.extend(u16::try_from(plaintext.len() + OVERHEAD)?.to_be_bytes());
    packet.extend(nonce);
    packet.extend(
        Aes256Gcm::new_from_slice(key)
            .unwrap()
            .encrypt(Nonce::from_slice(&nonce), plaintext)
            .map_err(|_| anyhow::anyhow!("Bulk packet encryption failed"))?,
    );
    Ok(packet)
}

/// Decode one complete, unfragmented security-enabled packet. The caller must
/// supply the key committed by the authenticated logical connection request.
pub(crate) fn open(key: &[u8; 32], packet: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        (OVERHEAD + 1..=MAX_PACKET).contains(&packet.len()),
        "Invalid bulk packet length"
    );
    let header = match packet[..2] {
        [0x81, 0x04] => 4,
        [0x82, 0x18 | 0x58] if packet.len() >= 8 => {
            // Version two carries fragment index/count even for one fragment.
            // The observed 0x40 flag does not change the nonce offset.
            anyhow::ensure!(
                packet[4..8] == [0, 0, 0, 1],
                "Multipart bulk packet requires reassembly"
            );
            8
        }
        _ => anyhow::bail!(
            "Unsupported bulk packet header ({:02x}/{:02x}, {} bytes)",
            packet[0],
            packet[1],
            packet.len()
        ),
    };
    anyhow::ensure!(
        usize::from(u16::from_be_bytes(packet[2..4].try_into()?)) == packet.len(),
        "Incomplete bulk packet"
    );
    Aes256Gcm::new_from_slice(key)
        .unwrap()
        .decrypt(
            Nonce::from_slice(&packet[header..header + 12]),
            &packet[header + 12..],
        )
        .map_err(|_| anyhow::anyhow!("Bulk packet authentication failed"))
}

/// A borrowed TLV node; values and lengths are encoded in network byte order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Node<'a> {
    pub kind: u16,
    pub tag: u16,
    pub value: &'a [u8],
}

pub(crate) fn nodes(input: &[u8]) -> anyhow::Result<Vec<Node<'_>>> {
    anyhow::ensure!(input.len() <= MAX_PACKET, "Oversized bulk TLV record");
    let mut rest = input;
    let mut result = Vec::new();
    while !rest.is_empty() {
        anyhow::ensure!(
            result.len() < 64 && rest.len() >= 8,
            "Invalid bulk TLV list"
        );
        let kind = u16::from_be_bytes(rest[..2].try_into()?);
        let tag = u16::from_be_bytes(rest[2..4].try_into()?);
        let length = usize::try_from(u32::from_be_bytes(rest[4..8].try_into()?))?;
        let value = rest
            .get(8..)
            .and_then(|body| body.get(..length))
            .ok_or_else(|| anyhow::anyhow!("Truncated bulk TLV value"))?;
        // Unknown types cannot be treated as arbitrary scalar data. This keeps
        // malformed integer widths and unsupported containers out of dispatch.
        let valid = match kind {
            1 | 6 => length == 1,
            2 | 7 => length == 2,
            3 | 8 => length == 4,
            4 | 9 => length == 8,
            5 | 0x100 | 0x101 => true,
            _ => false,
        };
        anyhow::ensure!(valid, "Unsupported bulk TLV type or width");
        result.push(Node { kind, tag, value });
        rest = &rest[8 + length..];
    }
    Ok(result)
}

pub(crate) fn tlv(kind: u16, tag: u16, value: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(value.len() <= MAX_PACKET - 8, "Oversized bulk TLV value");
    let mut encoded = kind.to_be_bytes().to_vec();
    encoded.extend(tag.to_be_bytes());
    encoded.extend(u32::try_from(value.len())?.to_be_bytes());
    encoded.extend(value);
    nodes(&encoded)?;
    Ok(encoded)
}

/// Require exactly one selected branch of the root union. Nested schemas are
/// checked by their individual handlers; this function never recurses on input.
pub(crate) fn frame(input: &[u8]) -> anyhow::Result<Node<'_>> {
    let root = nodes(input)?;
    anyhow::ensure!(
        root.len() == 1 && root[0].kind == 0x101 && root[0].tag == u16::MAX,
        "Invalid bulk TLV root"
    );
    let selected = root[0]
        .value
        .get(..2)
        .ok_or_else(|| anyhow::anyhow!("Missing bulk union selector"))?;
    let selected = u16::from_be_bytes(selected.try_into()?);
    let mut branches = nodes(&root[0].value[2..])?;
    anyhow::ensure!(
        branches.len() == 1 && branches[0].tag == selected,
        "Invalid bulk TLV union"
    );
    Ok(branches.remove(0))
}

pub(crate) fn union(tag: u16, kind: u16, value: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut body = tag.to_be_bytes().to_vec();
    body.extend(tlv(kind, tag, value)?);
    tlv(0x101, u16::MAX, &body)
}

/// The SDK event socket begins with a plaintext client-info frame, before its
/// encrypted packet frames. This step negotiates framing, not peer trust.
pub(crate) fn negotiation_reply(input: &[u8], server_id: u32) -> anyhow::Result<Option<Vec<u8>>> {
    let client = frame(input)?;
    if (client.kind, client.tag) != (0x100, 0) {
        return Ok(None);
    }
    let fields = nodes(client.value)?;
    anyhow::ensure!(fields.len() == 3, "Invalid bulk client-info fields");
    let integer = |tag| -> anyhow::Result<u32> {
        let mut matching = fields.iter().filter(|node| node.tag == tag);
        let node = matching
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing bulk client-info field"))?;
        anyhow::ensure!(
            matching.next().is_none() && node.kind == 3,
            "Ambiguous bulk client-info field"
        );
        Ok(u32::from_be_bytes(node.value.try_into()?))
    };
    anyhow::ensure!(
        server_id != 0 && integer(0)? == server_id,
        "Wrong bulk server identity"
    );
    anyhow::ensure!(integer(1)? == 1, "Unsupported bulk event protocol version");
    let maximum = integer(2)?;
    anyhow::ensure!(
        (32..=0xff00).contains(&maximum),
        "Invalid bulk event packet limit"
    );
    let mut server = tlv(3, 0, &server_id.to_be_bytes())?;
    server.extend(tlv(3, 1, &maximum.to_be_bytes())?);
    Ok(Some(union(4, 0x100, &server)?))
}

/// Advertise the separate TCP data socket inside an encrypted SDK event packet.
/// The random challenge is an ephemeral socket credential, never a filename.
pub(crate) fn data_socket_offer(port: u16, challenge: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(port != 0, "Invalid file data socket port");
    let mut handshake = tlv(3, 0, &0u32.to_be_bytes())?;
    handshake.extend(tlv(3, 1, &1u32.to_be_bytes())?);
    handshake.extend(tlv(3, 2, &0u32.to_be_bytes())?);
    handshake.extend(tlv(3, 3, &u32::from(port).to_be_bytes())?);
    handshake.extend(tlv(5, 4, challenge)?);
    // This field is the number of data connections, not a TCP transport enum.
    // Advertise exactly the one connection that our listener accepts.
    handshake.extend(tlv(1, 5, &[1])?);
    let express = union(1, 0x100, &handshake)?;
    union(1, 0x100, &tlv(5, 0, &express)?)
}

pub(crate) fn byte_message(id: u32, bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(bytes.len() <= 61440, "Oversized Express byte response");
    let mut fields = tlv(3, 0, &id.to_be_bytes())?;
    fields.extend(tlv(5, 1, bytes)?);
    fields.extend(tlv(1, 2, &[0])?);
    let express = union(2, 0x100, &fields)?;
    union(1, 0x100, &tlv(5, 0, &express)?)
}

pub(crate) struct StreamOffer<'a> {
    pub id: u32,
    pub length: Option<u64>,
    pub name: &'a str,
    pub tag: &'a [u8],
}

/// Native StreamSnd fields: packet type, stream ID, length, content type,
/// filename, and tag. Only the stream ID belongs in the RcvBegin response.
pub(crate) fn stream_offer(input: &[u8]) -> anyhow::Result<Option<StreamOffer<'_>>> {
    let packet = frame(input)?;
    if (packet.kind, packet.tag) != (0x100, 3) {
        return Ok(None);
    }
    let fields = nodes(packet.value)?;
    anyhow::ensure!(fields.len() == 6, "Invalid stream-offer fields");
    let get = |tag, kind| -> anyhow::Result<&[u8]> {
        let mut matches = fields.iter().filter(|f| f.tag == tag);
        let field = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing stream-offer field"))?;
        anyhow::ensure!(
            field.kind == kind && matches.next().is_none(),
            "Ambiguous stream-offer field"
        );
        Ok(field.value)
    };
    let _packet_type = u32::from_be_bytes(get(0, 3)?.try_into()?);
    let id = u32::from_be_bytes(get(1, 3)?.try_into()?);
    let length = i64::from_be_bytes(get(2, 4)?.try_into()?);
    let _content_type = get(3, 5)?;
    let name = std::str::from_utf8(get(4, 5)?)?;
    let tag = get(5, 5)?;
    anyhow::ensure!(
        id <= i32::MAX as u32 && (-1..=crate::file_store::MAX_FILE_SIZE as i64).contains(&length),
        "Invalid stream identity or length"
    );
    anyhow::ensure!(
        crate::file_store::valid_name(name),
        "Unsafe stream filename"
    );
    Ok(Some(StreamOffer {
        id,
        length: (length >= 0).then_some(length as u64),
        name,
        tag,
    }))
}

/// Mode two uses the separate data socket; mode one uses the serial event path.
pub(crate) fn stream_begin(id: u32) -> anyhow::Result<Vec<u8>> {
    let mut fields = tlv(3, 0, &0u32.to_be_bytes())?;
    fields.extend(tlv(3, 1, &id.to_be_bytes())?);
    fields.extend(tlv(3, 2, &2u32.to_be_bytes())?);
    let express = union(4, 0x100, &fields)?;
    union(1, 0x100, &tlv(5, 0, &express)?)
}

/// Clipboard uses the SDK directly, without the Express layer used by Share.
pub(crate) fn sdk_bytes(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(bytes.len() <= 61440, "Oversized clipboard response");
    union(1, 0x100, &tlv(5, 0, bytes)?)
}
pub(crate) fn sdk_stream(id: u32, size: u64, name: &str, mime: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        id > 0
            && id <= i32::MAX as u32
            && size <= crate::file_store::MAX_FILE_SIZE
            && crate::file_store::valid_name(name),
        "Invalid clipboard stream"
    );
    anyhow::ensure!(
        mime.len() <= 128 && mime.bytes().all(|b| b.is_ascii_graphic()),
        "Invalid file MIME"
    );
    let mut header = tlv(4, 0, &size.to_be_bytes())?;
    header.extend(tlv(5, 1, mime.as_bytes())?);
    header.extend(tlv(5, 2, name.as_bytes())?);
    header.extend(tlv(5, 3, &[])?);
    let mut payload = 0u16.to_be_bytes().to_vec();
    payload.extend(tlv(0x100, 0, &header)?);
    let mut fields = tlv(3, 0, &id.to_be_bytes())?;
    fields.extend(tlv(3, 1, &1u32.to_be_bytes())?);
    fields.extend(tlv(0x101, 2, &payload)?);
    union(2, 0x100, &fields)
}
#[cfg(test)]
pub(crate) fn sdk_event(id: u32, event: u32) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        id > 0 && (2..=5).contains(&event),
        "Invalid clipboard stream event"
    );
    let mut fields = tlv(3, 0, &id.to_be_bytes())?;
    fields.extend(tlv(3, 1, &event.to_be_bytes())?);
    fields.extend(tlv(0x101, 2, &u16::MAX.to_be_bytes())?);
    union(2, 0x100, &fields)
}
pub(crate) fn sdk_event_value(packet: &Node<'_>) -> anyhow::Result<(u32, u32)> {
    anyhow::ensure!(
        (packet.kind, packet.tag) == (0x100, 2),
        "Unexpected clipboard frame"
    );
    let fields = nodes(packet.value)?;
    anyhow::ensure!(
        fields.len() == 3
            && fields[0].kind == 3
            && fields[0].tag == 0
            && fields[1].kind == 3
            && fields[1].tag == 1
            && fields[2].kind == 0x101
            && fields[2].tag == 2
            && fields[2].value == [255, 255],
        "Invalid clipboard stream event"
    );
    let id = u32::from_be_bytes(fields[0].value.try_into()?);
    let event = u32::from_be_bytes(fields[1].value.try_into()?);
    anyhow::ensure!(
        id > 0 && (2..=5).contains(&event),
        "Unknown clipboard stream event"
    );
    Ok((id, event))
}
pub(crate) fn sdk_data(id: u32, offset: i64, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        id > 0 && data.len() <= 16384 && offset >= 0,
        "Invalid clipboard data chunk"
    );
    let mut fields = tlv(3, 0, &id.to_be_bytes())?;
    fields.extend(tlv(4, 1, &offset.to_be_bytes())?);
    fields.extend(tlv(3, 2, &(data.len() as u32).to_be_bytes())?);
    fields.extend(tlv(5, 3, data)?);
    union(3, 0x100, &fields)
}

/// Express byte packets can exceed one SDK event record (native chunks are
/// 60 KiB). Publish the control message only after its final authenticated chunk.
#[derive(Default)]
pub(crate) struct BytesAssembler {
    id: Option<u32>,
    bytes: Vec<u8>,
}
impl BytesAssembler {
    pub fn push(&mut self, packet: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let packet = frame(packet)?;
        if (packet.kind, packet.tag) != (0x100, 2) {
            return Ok(None);
        }
        let fields = nodes(packet.value)?;
        anyhow::ensure!(fields.len() == 3, "Invalid Express byte fields");
        let get = |tag, kind| -> anyhow::Result<&[u8]> {
            let mut values = fields.iter().filter(|field| field.tag == tag);
            let field = values
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing Express byte field"))?;
            anyhow::ensure!(
                field.kind == kind && values.next().is_none(),
                "Ambiguous Express byte field"
            );
            Ok(field.value)
        };
        let id = u32::from_be_bytes(get(0, 3)?.try_into()?);
        let bytes = get(1, 5)?;
        let more = get(2, 1)?[0];
        anyhow::ensure!(
            more <= 1 && bytes.len() <= 61440,
            "Invalid Express byte chunk"
        );
        anyhow::ensure!(
            self.id.is_none_or(|current| current == id),
            "Interleaved Express byte packets"
        );
        anyhow::ensure!(
            self.bytes.len() + bytes.len() <= 4 * 1024 * 1024,
            "Oversized Express control message"
        );
        self.id = Some(id);
        self.bytes.extend(bytes);
        if more == 0 {
            self.id = None;
            Ok(Some(std::mem::take(&mut self.bytes)))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn clipboard_sdk_frames_match_native_synthetic_encoder() {
        // Independent native ChannelFrame serialization of synthetic values.
        let decode = |hex: &str| -> Vec<u8> {
            hex.as_bytes()
                .chunks_exact(2)
                .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
                .collect()
        };
        assert_eq!(
            super::sdk_stream(7, 12, "sample.heic", "image/heic").unwrap(),
            decode(
                "0101ffff000000710002010000020000006700030000000000040000000700030001000000040000000101010002000000470000010000000000003d0004000000000008000000000000000c000500010000000a696d6167652f68656963000500020000000b73616d706c652e686569630005000300000000"
            )
        );
        let event = decode(
            "0101ffff0000002c000201000002000000220003000000000004000000070003000100000004000000020101000200000002ffff",
        );
        assert_eq!(super::sdk_event(7, 2).unwrap(), event);
        assert_eq!(
            super::sdk_event_value(&super::frame(&event).unwrap()).unwrap(),
            (7, 2)
        );
        assert_eq!(
            super::sdk_data(7, 9, b"abc").unwrap(),
            decode(
                "0101ffff0000003d00030100000300000033000300000000000400000007000400010000000800000000000000090003000200000004000000030005000300000003616263"
            )
        );
        // Native ContinueSend's successful EOF carries the final byte offset
        // and an empty payload. Negative offsets mean cancellation/failure.
        assert_eq!(
            super::sdk_data(7, 12, &[]).unwrap(),
            decode(
                "0101ffff0000003a000301000003000000300003000000000004000000070004000100000008000000000000000c0003000200000004000000000005000300000000"
            )
        );
        assert!(super::sdk_data(7, -1, &[]).is_err());
        assert!(super::sdk_data(7, -1, b"abc").is_err());
        assert!(super::sdk_data(7, 0, &vec![0; 16385]).is_err());
        let mut bad = event;
        let last = bad.len() - 1;
        bad[last] = 0;
        assert!(super::sdk_event_value(&super::frame(&bad).unwrap()).is_err());
    }

    use super::*;
    #[test]
    fn data_offer_requests_one_connection_matching_our_listener() {
        let encoded = data_socket_offer(12345, &[0x42; 16]).unwrap();
        let sdk = frame(&encoded).unwrap();
        let fields = nodes(sdk.value).unwrap();
        let handshake = frame(fields[0].value).unwrap();
        assert_eq!((handshake.kind, handshake.tag), (0x100, 1));
        let fields = nodes(handshake.value).unwrap();
        assert_eq!(
            (fields[5].kind, fields[5].tag, fields[5].value),
            (1, 5, [1].as_slice())
        );
        assert_eq!(fields[3].value, 12345u32.to_be_bytes());
        assert_eq!(fields[4].value, [0x42; 16]);
    }
    #[test]
    fn stream_begin_echoes_stream_id_and_offer_rejects_unsafe_metadata() {
        let packet = |name: &[u8], length: u64| {
            let mut fields = tlv(3, 0, &32u32.to_be_bytes()).unwrap();
            fields.extend(tlv(3, 1, &7u32.to_be_bytes()).unwrap());
            fields.extend(tlv(4, 2, &length.to_be_bytes()).unwrap());
            fields.extend(tlv(5, 3, b"application/octet-stream").unwrap());
            fields.extend(tlv(5, 4, name).unwrap());
            fields.extend(tlv(5, 5, b"synthetic-file").unwrap());
            union(3, 0x100, &fields).unwrap()
        };
        let input = packet(b"example.bin", 100);
        let offer = stream_offer(&input).unwrap().unwrap();
        assert_eq!(
            (offer.id, offer.length, offer.name),
            (7, Some(100), "example.bin")
        );
        let sdk = stream_begin(offer.id).unwrap();
        let sdk = frame(&sdk).unwrap();
        let fields = nodes(sdk.value).unwrap();
        let ack = frame(fields[0].value).unwrap();
        assert_eq!((ack.kind, ack.tag), (0x100, 4));
        let ack = nodes(ack.value).unwrap();
        assert_eq!(ack.len(), 3);
        assert_eq!(ack[0].value, 0u32.to_be_bytes());
        assert_eq!(ack[1].value, 7u32.to_be_bytes());
        assert_eq!(ack[2].value, 2u32.to_be_bytes());
        assert!(stream_offer(&packet(b"../unsafe", 1)).is_err());
        assert_eq!(
            stream_offer(&packet(b"unknown", u64::MAX))
                .unwrap()
                .unwrap()
                .length,
            None
        );
        assert!(stream_offer(&packet(b"invalid", u64::MAX - 1)).is_err());
        assert!(stream_offer(&packet(b"large", crate::file_store::MAX_FILE_SIZE + 1)).is_err());
        for end in 0..input.len() {
            assert!(stream_offer(&input[..end]).is_err());
        }
    }
    #[test]
    fn express_bytes_wait_for_last_chunk_and_reject_interleaving() {
        let packet = |id: u32, data: &[u8], more: u8| {
            let mut fields = tlv(3, 0, &id.to_be_bytes()).unwrap();
            fields.extend(tlv(5, 1, data).unwrap());
            fields.extend(tlv(1, 2, &[more]).unwrap());
            union(2, 0x100, &fields).unwrap()
        };
        let mut stream = BytesAssembler::default();
        assert!(stream.push(&packet(7, b"first", 1)).unwrap().is_none());
        assert!(stream.push(&packet(8, b"other", 0)).is_err());
        assert_eq!(
            stream.push(&packet(7, b"last", 0)).unwrap().unwrap(),
            b"firstlast"
        );
        assert_eq!(stream.push(&packet(8, b"new", 0)).unwrap().unwrap(), b"new");
        assert!(stream.push(&packet(9, b"bad", 2)).is_err());
        assert!(stream.push(&packet(9, &vec![0; 61441], 1)).is_err());
        let chunk = packet(9, &vec![0; 61440], 1);
        for _ in 0..68 {
            assert!(stream.push(&chunk).unwrap().is_none());
        }
        assert!(stream.push(&chunk).is_err());
    }
    #[test]
    fn event_negotiation_checks_identity_version_and_duplicate_fields() {
        let mut fields = tlv(3, 0, &7u32.to_be_bytes()).unwrap();
        fields.extend(tlv(3, 1, &1u32.to_be_bytes()).unwrap());
        fields.extend(tlv(3, 2, &0xff00u32.to_be_bytes()).unwrap());
        let client = |fields: &[u8]| union(0, 0x100, fields).unwrap();
        let packet = client(&fields);
        let response = negotiation_reply(&packet, 7).unwrap().unwrap();
        let response = frame(&response).unwrap();
        assert_eq!((response.kind, response.tag), (0x100, 4));
        let server_fields = nodes(response.value).unwrap();
        assert_eq!(server_fields.len(), 2);
        assert_eq!(server_fields[0].value, 7u32.to_be_bytes());
        assert_eq!(server_fields[1].value, 0xff00u32.to_be_bytes());
        assert!(negotiation_reply(&packet, 8).is_err());
        fields[23] = 2;
        assert!(negotiation_reply(&client(&fields), 7).is_err());
        fields[23] = 1;
        fields[26..28].fill(1);
        assert!(negotiation_reply(&client(&fields), 7).is_err());
        fields[26..28].copy_from_slice(&1u16.to_be_bytes());
        assert!(negotiation_reply(&client(&fields), 7).is_err());
    }
    #[cfg(feature = "file-sharing")]
    #[test]
    fn reliable_transport_rejects_truncated_oversized_and_foreign_segments() {
        // KCP push: fixed native conversation, sequence zero, three-byte body.
        let mut packet = vec![0x78, 0x56, 0x34, 0x12, 81, 0, 32, 0];
        packet.extend([0; 12]);
        packet.extend(3u32.to_le_bytes());
        packet.extend(b"abc");
        validate_datagram(&packet).unwrap();
        for end in 0..packet.len() {
            assert!(validate_datagram(&packet[..end]).is_err());
        }
        let mut fragmented = packet.clone();
        fragmented[5] = 47;
        validate_datagram(&fragmented).unwrap();
        for (index, byte) in [(0, 0), (4, 85), (5, 128), (20, 255)] {
            let mut bad = packet.clone();
            bad[index] = byte;
            assert!(validate_datagram(&bad).is_err());
        }
        let mut ack = packet[..24].to_vec();
        ack[4] = 82;
        ack[20..24].fill(0);
        validate_datagram(&ack).unwrap();
        let mut joined = packet.clone();
        joined.extend(ack);
        validate_datagram(&joined).unwrap();
        joined.push(0);
        assert!(validate_datagram(&joined).is_err());
    }
    #[test]
    fn bulk_packets_reject_tampering_truncation_and_security_downgrades() {
        let key = [0x42; 32];
        let packet = seal(&key, b"synthetic control").unwrap();
        assert_eq!(packet[..4], [0x81, 0x04, 0, 49]);
        assert_eq!(open(&key, &packet).unwrap(), b"synthetic control");
        assert!(open(&[0x43; 32], &packet).is_err());
        for index in [0, 1, 2, 3, 4, 16, packet.len() - 1] {
            let mut bad = packet.clone();
            bad[index] ^= 1;
            assert!(open(&key, &bad).is_err());
        }
        for end in 0..packet.len() {
            assert!(open(&key, &packet[..end]).is_err());
        }
        assert!(seal(&key, &[]).is_err());
        assert!(seal(&key, &vec![0; MAX_PACKET - OVERHEAD + 1]).is_err());
        let mut v2 = vec![0x82, 0x58];
        v2.extend(((packet.len() + 4) as u16).to_be_bytes());
        v2.extend([0, 0, 0, 1]);
        v2.extend(&packet[4..]);
        assert_eq!(open(&key, &v2).unwrap(), b"synthetic control");
        v2[7] = 2;
        assert!(open(&key, &v2).is_err());
        v2[7] = 1;
        v2[5] = 1;
        assert!(open(&key, &v2).is_err());
        let largest = vec![0x5a; MAX_PACKET - OVERHEAD];
        assert_eq!(open(&key, &seal(&key, &largest).unwrap()).unwrap(), largest);
    }

    #[test]
    fn tlv_dispatch_is_bounded_and_rejects_ambiguous_or_truncated_frames() {
        // Independently specified union -> client-info -> i32 version=1.
        let bytes = [
            1, 1, 255, 255, 0, 0, 0, 22, 0, 0, 1, 0, 0, 0, 0, 0, 0, 12, 0, 3, 0, 0, 0, 0, 0, 4, 0,
            0, 0, 1,
        ];
        let branch = frame(&bytes).unwrap();
        assert_eq!((branch.kind, branch.tag), (0x100, 0));
        assert_eq!(nodes(branch.value).unwrap()[0].value, 1u32.to_be_bytes());
        let mut ambiguous = branch.value.to_vec();
        ambiguous.extend(branch.value);
        assert!(frame(&tlv(0x101, u16::MAX, &ambiguous).unwrap()).is_err());
        for end in 0..bytes.len() {
            assert!(frame(&bytes[..end]).is_err());
        }
        assert!(nodes(&[0, 3, 0, 0, 0, 0, 0, 1, 0]).is_err());
        assert!(nodes(&[0, 5, 0, 0, 255, 255, 255, 255]).is_err());
        assert!(nodes(&[0; MAX_PACKET + 1]).is_err());
        let many = tlv(5, 0, &[]).unwrap().repeat(65);
        assert!(nodes(&many).is_err());
    }
}
