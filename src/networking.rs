//! Authenticated device capabilities shared by clipboard and file services.
use crate::wire::{self, Field, message};
use serde::Deserialize;

pub const FILE_DISCOVERY: &str = "miLyraShare";
pub const DEVICE_NAME: &str = "Clip";
pub const FILE_TRANSFER: &str = "miLyraShareTransfer";
pub const MAC_PACKAGE: &str = "com.xiaomi.hyperConnect";
pub const PHONE_PACKAGE: &str = "com.miui.mishare.connectivity";

#[derive(Clone, Deserialize)]
pub struct Service {
    pub name: String,
    pub package: String,
    #[serde(default)]
    pub data: Vec<u8>,
}
impl Service {
    pub(crate) fn local_services() -> Vec<Self> {
        #[allow(unused_mut)]
        let mut services = vec![Self::clipboard()];
        #[cfg(feature = "file-sharing")]
        services.extend([Self::file_discovery(), Self::file_transfer()]);
        services
    }
    pub fn clipboard() -> Self {
        Self {
            name: "universalClipboard".into(),
            package: "com.milink.service".into(),
            data: vec![0],
        }
    }
    pub fn file_discovery() -> Self {
        Self {
            name: FILE_DISCOVERY.into(),
            package: MAC_PACKAGE.into(),
            data: vec![0, 0, 0, 0, 0, 0, 0, 1],
        }
    }
    pub fn file_transfer() -> Self {
        Self {
            name: FILE_TRANSFER.into(),
            package: MAC_PACKAGE.into(),
            data: vec![],
        }
    }
    pub(crate) fn encode(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            valid_identifier(&self.name)
                && valid_identifier(&self.package)
                && self.data.len() <= 1024,
            "Invalid service advertisement"
        );
        let mut out = message(1, self.name.as_bytes());
        out.extend(message(2, self.package.as_bytes()));
        if !self.data.is_empty() {
            out.extend(message(3, &self.data));
        }
        Ok(out)
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        let name = std::str::from_utf8(wire::bytes_field(bytes, 1)?).ok()?;
        let package = std::str::from_utf8(wire::bytes_field(bytes, 2)?).ok()?;
        let data = wire::bytes_field(bytes, 3).unwrap_or_default();
        if !valid_identifier(name) || !valid_identifier(package) || data.len() > 1024 {
            return None;
        }
        Some(Self {
            name: name.into(),
            package: package.into(),
            data: data.into(),
        })
    }
}
fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

// Names and identifiers are for the local device picker; never log them.
pub struct Peer {
    pub name: String,
    pub id: String,
    pub device_type: u64,
    pub services: Vec<Service>,
}
impl Peer {
    pub fn can_share_files(&self) -> bool {
        self.services
            .iter()
            .any(|s| s.name == FILE_DISCOVERY && s.package == PHONE_PACKAGE)
            && self
                .services
                .iter()
                .any(|s| s.name == FILE_TRANSFER && s.package == PHONE_PACKAGE)
    }
}

/// Accept only the two device-sync envelopes, after outer AES-GCM validation.
pub fn peer(payload: &[u8]) -> Option<Peer> {
    if payload.first() != Some(&0) {
        return None;
    }
    let packet = &payload[1..];
    let command = wire::fields(packet)?.into_iter().find_map(|f| match f {
        Field::Integer(1, n) => Some(n),
        _ => None,
    })?;
    let device = match command {
        1 => wire::bytes_field(wire::bytes_field(packet, 5)?, 2)?,
        2 => wire::bytes_field(wire::bytes_field(packet, 6)?, 3)?,
        _ => return None,
    };
    let name = std::str::from_utf8(wire::bytes_field(device, 1)?).ok()?;
    let id = std::str::from_utf8(wire::bytes_field(device, 3)?).ok()?;
    if name.len() > 256
        || name.chars().any(char::is_control)
        || !(8..=128).contains(&id.len())
        || id.len() % 2 != 0
        || !id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let mut device_type = None;
    let mut services = Vec::new();
    for field in wire::fields(device)? {
        match field {
            Field::Integer(2, n) => device_type = Some(n),
            Field::Bytes(14, value) => {
                if services.len() == 128 {
                    return None;
                }
                services.push(Service::decode(value)?);
            }
            _ => (),
        }
    }
    Some(Peer {
        name: name.into(),
        id: id.into(),
        device_type: device_type?,
        services,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::integer;
    #[cfg(feature = "file-sharing")]
    #[test]
    fn default_services_make_file_receiver_discoverable() {
        let services = Service::local_services();
        assert!(services.iter().any(|s| s.name == "universalClipboard"));
        for name in [FILE_DISCOVERY, FILE_TRANSFER] {
            assert!(
                services
                    .iter()
                    .any(|s| s.name == name && s.package == MAC_PACKAGE)
            );
        }
    }
    fn fixture(command: u64, services: &[Service]) -> Vec<u8> {
        let mut device = message(1, b"Test phone");
        device.extend(integer(2, 1));
        device.extend(message(3, b"0102030405060708090A0B0C0D0E0F10"));
        for service in services {
            device.extend(message(14, &service.encode().unwrap()));
        }
        let mut payload = vec![0];
        payload.extend(integer(1, command));
        payload.extend(if command == 1 {
            message(5, &message(2, &device))
        } else {
            message(6, &message(3, &device))
        });
        payload
    }
    #[test]
    fn sharing_requires_both_native_phone_services() {
        let mut discovery = Service::file_discovery();
        discovery.package = PHONE_PACKAGE.into();
        let mut transfer = Service::file_transfer();
        transfer.package = PHONE_PACKAGE.into();
        for command in [1, 2] {
            let phone = peer(&fixture(command, &[discovery.clone(), transfer.clone()])).unwrap();
            assert!(phone.can_share_files());
            for device_type in [2, 3] {
                let tablet = Peer {
                    name: "Redmi Pad Pro".into(),
                    id: phone.id.clone(),
                    device_type,
                    services: phone.services.clone(),
                };
                assert!(tablet.can_share_files());
            }
            assert_eq!(phone.name, "Test phone");
            assert!(
                !peer(&fixture(command, &[discovery.clone()]))
                    .unwrap()
                    .can_share_files()
            );
        }
        assert!(
            !peer(&fixture(
                1,
                &[Service::file_discovery(), Service::file_transfer()]
            ))
            .unwrap()
            .can_share_files()
        );
    }
    #[test]
    fn rejects_oversized_services_truncation_and_wrong_module() {
        let good = fixture(2, &[Service::clipboard()]);
        for end in 0..good.len() {
            assert!(peer(&good[..end]).is_none());
        }
        let mut wrong = good;
        wrong[0] = 1;
        assert!(peer(&wrong).is_none());
        assert!(peer(&fixture(1, &vec![Service::clipboard(); 129])).is_none());
        let mut bad = Service::clipboard();
        bad.name = "invalid\0name".into();
        assert!(bad.encode().is_err());
        bad.name = "valid".into();
        bad.data = vec![0; 1025];
        assert!(bad.encode().is_err());
    }
}
