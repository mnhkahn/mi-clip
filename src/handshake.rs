//! Initial same-account handshake research. A server hello is not authentication.
use crate::{
    account, private,
    transport::Transport,
    wire::{self, integer, message},
};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use p256::{SecretKey, elliptic_curve::sec1::ToEncodedPoint};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    net::{SocketAddr, UdpSocket},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn nested<'a>(input: &'a [u8], path: &[u32]) -> anyhow::Result<&'a [u8]> {
    path.iter().try_fold(input, |bytes, &id| {
        wire::bytes_field(bytes, id).ok_or_else(|| anyhow::anyhow!("Missing handshake field"))
    })
}
fn number(input: &[u8], id: u32) -> anyhow::Result<u64> {
    wire::fields(input)
        .and_then(|fields| {
            fields.into_iter().find_map(|f| match f {
                wire::Field::Integer(n, v) if n == id => Some(v),
                _ => None,
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Missing handshake integer"))
}
fn server_suites(notify: &[u8]) -> anyhow::Result<&[u8]> {
    let suites = nested(notify, &[1])?;
    anyhow::ensure!(
        number(suites, 1)? == 1 && number(suites, 3)? == 16 && number(suites, 4)? == 8,
        "Unsupported server cipher suite"
    );
    anyhow::ensure!(nested(suites, &[2])?.len() == 32, "Invalid server random");
    let public = nested(suites, &[5])?;
    let bytes = nested(public, &[2])?;
    anyhow::ensure!(
        number(public, 1)? == 1 && bytes.len() == 65 && bytes.first() == Some(&4),
        "Invalid server ephemeral key format"
    );
    Ok(suites)
}
fn frame(kind: u8, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let size = u16::try_from(body.len() + 4)?;
    let mut out = vec![kind, 4];
    out.extend(size.to_be_bytes());
    out.extend(body);
    Ok(out)
}
fn logical_encrypted(
    reply: &[u8],
    session_key: &[u8; 32],
    inner: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let incoming = nested(reply, &[2, 1])?;
    logical_packet(
        number(incoming, 2)?,
        number(incoming, 1)?,
        number(incoming, 3)?,
        session_key,
        inner,
    )
}
fn logical_packet(
    source: u64,
    destination: u64,
    nonce: u64,
    session_key: &[u8; 32],
    inner: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let iv = private::random::<12>()?;
    let mut encrypted = iv.to_vec();
    encrypted.extend(
        Aes256Gcm::new_from_slice(session_key)
            .unwrap()
            .encrypt(Nonce::from_slice(&iv), inner)
            .map_err(|_| anyhow::anyhow!("Logical message encryption failed"))?,
    );
    let mut logical = integer(1, source);
    logical.extend(integer(2, destination));
    logical.extend(integer(3, nonce));
    logical.extend(integer(4, 1));
    logical.extend(message(5, &encrypted));
    frame(17, &message(2, &message(1, &logical)))
}
fn networking_probe(
    config: &ProbeConfig,
    identity: &account::DeviceIdentity,
    session: &account::Session,
    key: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    let did = URL_SAFE_NO_PAD.decode(&identity.did)?;
    let mut device = message(1, crate::networking::DEVICE_NAME.as_bytes());
    // Supply real_device_name too, so peers can avoid a model-based fallback label.
    device.extend(message(37, crate::networking::DEVICE_NAME.as_bytes()));
    device.extend(integer(2, 14));
    let device_id: String = did.iter().map(|b| format!("{b:02X}")).collect();
    let account_id: String = Sha256::digest(session.user_id.as_bytes())
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    device.extend(message(3, device_id.as_bytes()));
    device.extend(message(4, account_id.as_bytes()));
    device.extend(integer(16, 2));
    device.extend(integer(18, 131072));
    // Networking's WLAN capability is 128; netbus's transport enum uses 256.
    device.extend(integer(21, 128));
    device.extend(message(23, device_id.as_bytes()));
    device.extend(integer(10, 5));
    let default_services = crate::networking::Service::local_services();
    let services = config.services.as_ref().unwrap_or(&default_services);
    anyhow::ensure!(services.len() <= 8, "Too many advertised services");
    for service in services {
        device.extend(message(14, &service.encode()?));
    }
    // The networking registry separately checks proof of account membership.
    // Sign a fresh challenge with our enrolled key, never a vendor-owned key.
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use x509_cert::der::{DecodePem, Encode};
    let certificate_path = Path::new(&config.identity)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing identity directory"))?
        .join("device-cert.pem");
    let certificate = x509_cert::Certificate::from_pem(&std::fs::read(certificate_path)?)?;
    let challenge = private::random::<32>()?;
    let signing = SigningKey::from_slice(&STANDARD.decode(&identity.private_key)?)?;
    let signature: Signature = signing.sign(&challenge);
    let mut cert_cred = message(1, &challenge);
    cert_cred.extend(message(2, &certificate.to_der()?));
    cert_cred.extend(message(3, signature.to_der().as_bytes()));
    let mut credential = integer(1, 3);
    credential.extend(message(4, &cert_cred));
    let mut group = integer(1, 1);
    group.extend(message(3, &credential));
    device.extend(message(15, &group));
    let mut sync = integer(1, 1);
    sync.extend(message(2, &device));
    let mut networking = integer(1, 1);
    networking.extend(integer(3, 1));
    networking.extend(message(5, &sync));
    let mut plaintext = vec![0];
    plaintext.extend(networking);
    encrypted_payload(key, &plaintext)
}
fn encrypted_payload(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let iv = private::random::<12>()?;
    let mut body = vec![1, 1];
    body.extend(iv);
    body.extend(
        Aes256Gcm::new_from_slice(key)
            .unwrap()
            .encrypt(Nonce::from_slice(&iv), plaintext)
            .map_err(|_| anyhow::anyhow!("Networking message encryption failed"))?,
    );
    frame(0x29, &body)
}
fn verify_finished(
    secret: &SecretKey,
    random: &[u8],
    notify_reply: &[u8],
    finished_reply: &[u8],
) -> anyhow::Result<[u8; 32]> {
    let notify = nested(notify_reply, &[2, 1, 5, 7, 2, 6, 3])?;
    let suites = server_suites(notify)?;
    anyhow::ensure!(random.len() == 32, "Invalid client random");
    let public_bytes = nested(suites, &[5, 2])?;
    let peer = p256::PublicKey::from_sec1_bytes(public_bytes)?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
    let mut context = random.to_vec();
    context.extend(nested(suites, &[2])?);
    // Protocol constant, identical for every account and device.
    let salt = [
        0x32, 0x9b, 0xfc, 0x53, 0x39, 0x36, 0x55, 0xd7, 0x5a, 0xb0, 0x83, 0x98, 0xca, 0x91, 0x91,
        0xef, 0xfa, 0xa3, 0x37, 0xf2, 0xe0, 0xbe, 0xb5, 0x73, 0xb1, 0xf9, 0xa3, 0xd0, 0x15, 0x57,
        0x64, 0x80,
    ];
    let mut session_key = [0; 32];
    hkdf::Hkdf::<Sha256>::new(Some(&salt), shared.raw_secret_bytes())
        .expand(&context, &mut session_key)
        .map_err(|_| anyhow::anyhow!("Session key derivation failed"))?;
    let encrypted = nested(finished_reply, &[2, 1, 5, 7, 2, 6, 5, 1])?;
    anyhow::ensure!(encrypted.len() >= 28, "Truncated server Finished");
    let plaintext = Aes256Gcm::new_from_slice(&session_key)
        .unwrap()
        .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
        .map_err(|_| anyhow::anyhow!("Server Finished GCM verification failed"))?;
    let mut expected = shared.raw_secret_bytes().to_vec();
    expected.extend(public_bytes);
    anyhow::ensure!(
        plaintext == expected,
        "Server Finished key confirmation mismatch"
    );
    Ok(session_key)
}
pub fn verify_saved_finished(directory: &Path) -> anyhow::Result<()> {
    let secret = SecretKey::from_slice(&std::fs::read(directory.join("ephemeral-key.bin"))?)?;
    let random = std::fs::read(directory.join("client-random.bin"))?;
    let notify = std::fs::read(directory.join("reply-3.bin"))?;
    let finished = std::fs::read(directory.join("reply-4.bin"))?;
    let key = verify_finished(&secret, &random, &notify[4..], &finished[4..])?;
    private::save(&directory.join("session-key.bin"), &key)?;
    println!(
        "Server Finished authenticated and confirmed the independently derived session key. Clipboard sync remains unimplemented."
    );
    Ok(())
}
fn client_finished(
    config: &ProbeConfig,
    session: &account::Session,
    identity: &account::DeviceIdentity,
    secret: &SecretKey,
    reply: &[u8],
    info: &[u8],
) -> anyhow::Result<(Vec<u8>, p256::ecdsa::VerifyingKey)> {
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};
    use x509_cert::der::{DecodePem, Encode};
    let notify = nested(reply, &[2, 1, 5, 7, 2, 6, 3])?;
    let server_public = nested(server_suites(notify)?, &[5, 2])?;
    let peer = p256::PublicKey::from_sec1_bytes(server_public)?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
    let encrypted = nested(notify, &[2])?;
    anyhow::ensure!(encrypted.len() >= 28, "Truncated peer credential");
    let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes()).unwrap();
    let credential = cipher
        .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
        .map_err(|_| anyhow::anyhow!("Peer credential GCM verification failed"))?;
    let client_public = secret.public_key().to_encoded_point(false);
    let peer_key = verify_credential(
        &credential,
        &std::fs::read(&config.account_certificate)?,
        &session.user_id,
        number(info, 5)?,
        server_public,
        client_public.as_bytes(),
    )?;
    let own_cert_path = Path::new(&config.identity)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Missing identity directory"))?
        .join("device-cert.pem");
    let own_cert = x509_cert::Certificate::from_pem(&std::fs::read(own_cert_path)?)?;
    let signing = SigningKey::from_slice(&STANDARD.decode(&identity.private_key)?)?;
    anyhow::ensure!(
        certificate_key(&own_cert)? == *signing.verifying_key(),
        "Local certificate/key mismatch"
    );
    let mut signed = client_public.as_bytes().to_vec();
    signed.extend(server_public);
    let signature: Signature = signing.sign(&signed);
    let mut own_credential = message(1, &own_cert.to_der()?);
    own_credential.extend(message(2, signature.to_der().as_bytes()));
    let iv = private::random::<12>()?;
    let mut encrypted = iv.to_vec();
    encrypted.extend(
        cipher
            .encrypt(Nonce::from_slice(&iv), own_credential.as_slice())
            .map_err(|_| anyhow::anyhow!("Client credential encryption failed"))?,
    );
    let mut pair = integer(1, 3);
    pair.extend(message(4, &message(1, &encrypted)));
    let mut handshake = integer(1, 2);
    handshake.extend(integer(2, 4));
    handshake.extend(message(6, &pair));
    let mut auth = integer(1, 1);
    auth.extend(message(2, &handshake));
    let mut inner = integer(1, 6);
    inner.extend(message(7, &auth));
    let incoming = nested(reply, &[2, 1])?;
    let mut logical = integer(1, number(incoming, 2)?);
    logical.extend(integer(2, number(incoming, 1)?));
    logical.extend(integer(3, number(incoming, 3)?));
    logical.extend(message(5, &inner));
    Ok((frame(17, &message(2, &message(1, &logical)))?, peer_key))
}
pub fn inspect(directory: &Path, output: &Path) -> anyhow::Result<()> {
    let secret = SecretKey::from_slice(&std::fs::read(directory.join("ephemeral-key.bin"))?)?;
    let reply = std::fs::read(directory.join("reply-3.bin"))?;
    anyhow::ensure!(reply.len() >= 4, "Truncated peer frame");
    let notify = nested(&reply[4..], &[2, 1, 5, 7, 2, 6, 3])?;
    let suites = nested(notify, &[1])?;
    let public = p256::PublicKey::from_sec1_bytes(nested(suites, &[5, 2])?)?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), public.as_affine());
    let encrypted = nested(notify, &[2])?;
    anyhow::ensure!(encrypted.len() >= 28, "Truncated peer credential");
    let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid handshake key"))?;
    let credential = cipher
        .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
        .map_err(|_| anyhow::anyhow!("Peer credential GCM authentication failed"))?;
    private::save(&output.join("credential.bin"), &credential)?;
    private::save(
        &output.join("server-certificate.der"),
        nested(&credential, &[1])?,
    )?;
    println!(
        "Peer credential decrypted with independently generated P256 key; certificate {} bytes, signature {} bytes. Trust verification remains required.",
        nested(&credential, &[1])?.len(),
        nested(&credential, &[2])?.len()
    );
    Ok(())
}

fn client_hello(public_key: &[u8], random: &[u8; 32]) -> Vec<u8> {
    let mut key = integer(1, 1);
    key.extend(message(2, public_key));
    let mut suites = integer(1, 1);
    suites.extend(message(2, random));
    suites.extend(integer(3, 16));
    suites.extend(integer(4, 8));
    suites.extend(message(5, &key));
    let mut notify = message(1, &suites);
    notify.extend(message(2, &integer(1, 1)));
    notify.extend(message(3, &integer(1, 1)));
    let mut pair = integer(1, 1);
    pair.extend(message(2, &notify));
    let mut frame = integer(1, 2);
    frame.extend(integer(2, 4));
    frame.extend(message(6, &pair));
    frame
}
fn sync_auth(uid: &str, salt: &[u8; 8], connection_nonce: u32, hello: &[u8]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(salt);
    hash.update(Sha256::digest(uid.as_bytes()));
    let mut feature = message(1, salt);
    feature.extend(message(2, &hash.finalize()));
    let mut handshake = integer(1, 1);
    handshake.extend(message(2, hello));
    let mut auth_inner = integer(1, 6);
    auth_inner.extend(message(7, &handshake));
    let mut sync = integer(1, 10000);
    sync.extend(integer(2, 16));
    sync.extend(message(4, b"00150323"));
    sync.extend(message(5, &feature));
    sync.extend(message(8, &auth_inner));
    let mut inner = integer(1, 5);
    inner.extend(message(6, &sync));
    let mut logical = integer(1, 1);
    logical.extend(integer(3, u64::from(connection_nonce)));
    logical.extend(message(5, &inner));
    message(2, &message(1, &logical))
}
#[derive(Deserialize)]
struct ProbeConfig {
    #[serde(default = "default_endpoint")]
    endpoint: SocketAddr,
    session: String,
    identity: String,
    account_certificate: String,
    #[serde(default)]
    listen_seconds: Option<u64>,
    #[serde(default)]
    clipboard_sync: bool,
    #[serde(default)]
    services: Option<Vec<crate::networking::Service>>,
    #[serde(default)]
    receive_directory: Option<String>,
}
fn default_endpoint() -> SocketAddr {
    ([0, 0, 0, 0], 0).into()
}

fn certificate_key(cert: &x509_cert::Certificate) -> anyhow::Result<p256::ecdsa::VerifyingKey> {
    use p256::pkcs8::DecodePublicKey;
    use x509_cert::der::Encode;
    Ok(p256::ecdsa::VerifyingKey::from_public_key_der(
        &cert.tbs_certificate.subject_public_key_info.to_der()?,
    )?)
}
fn verify_credential(
    credential: &[u8],
    anchor: &[u8],
    uid: &str,
    peer_type: u64,
    server_public: &[u8],
    client_public: &[u8],
) -> anyhow::Result<p256::ecdsa::VerifyingKey> {
    use p256::ecdsa::{Signature, signature::Verifier};
    use x509_cert::der::{Decode, DecodePem, Encode};
    let cert = x509_cert::Certificate::from_der(nested(credential, &[1])?)?;
    let anchor = x509_cert::Certificate::from_pem(anchor)?;
    anyhow::ensure!(
        cert.tbs_certificate.issuer == anchor.tbs_certificate.subject,
        "Peer certificate issuer mismatch"
    );
    anyhow::ensure!(
        cert.signature_algorithm == cert.tbs_certificate.signature
            && cert.signature_algorithm.oid.to_string() == "1.2.840.10045.4.3.2",
        "Unsupported peer certificate signature algorithm"
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    for item in [&cert, &anchor] {
        let validity = &item.tbs_certificate.validity;
        anyhow::ensure!(
            validity.not_before.to_unix_duration() <= now
                && now <= validity.not_after.to_unix_duration(),
            "Certificate is outside its validity period"
        );
    }
    let signature = cert
        .signature
        .as_bytes()
        .ok_or_else(|| anyhow::anyhow!("Invalid certificate signature bits"))?;
    certificate_key(&anchor)?
        .verify(
            &cert.tbs_certificate.to_der()?,
            &Signature::from_der(signature)?,
        )
        .map_err(|_| anyhow::anyhow!("Peer certificate signature verification failed"))?;
    let common_names: Vec<_> = cert
        .tbs_certificate
        .subject
        .0
        .iter()
        .flat_map(|r| r.0.iter())
        .filter(|a| a.oid.to_string() == "2.5.4.3")
        .collect();
    anyhow::ensure!(
        common_names.len() == 1,
        "Peer certificate common name is ambiguous"
    );
    let cn = std::str::from_utf8(common_names[0].value.value())?;
    let parts: Vec<_> = cn.split('.').collect();
    anyhow::ensure!(
        parts.len() == 4 && parts[0] == "lyra" && parts[3] == peer_type.to_string(),
        "Peer certificate device type mismatch"
    );
    // Android's stable certificate DID differs from its rotating four-byte advertisement DID.
    let did = URL_SAFE_NO_PAD.decode(parts[2])?;
    anyhow::ensure!(
        (4..=128).contains(&did.len()),
        "Invalid certificate device identity"
    );
    let expected_hash =
        URL_SAFE_NO_PAD.encode(Sha256::digest(format!("{}{uid}", parts[2]).as_bytes()));
    anyhow::ensure!(
        parts[1] == expected_hash,
        "Peer certificate belongs to a different account"
    );
    let mut signed = server_public.to_vec();
    signed.extend(client_public);
    certificate_key(&cert)?
        .verify(&signed, &Signature::from_der(nested(credential, &[2])?)?)
        .map_err(|_| anyhow::anyhow!("Peer ephemeral-key signature verification failed"))?;
    certificate_key(&cert)
}
pub fn verify(config: &Path, directory: &Path) -> anyhow::Result<()> {
    let config: ProbeConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    let session = account::read_session(Path::new(&config.session))?;
    let secret = SecretKey::from_slice(&std::fs::read(directory.join("ephemeral-key.bin"))?)?;
    let reply = std::fs::read(directory.join("reply-3.bin"))?;
    let notify = nested(&reply[4..], &[2, 1, 5, 7, 2, 6, 3])?;
    let public = nested(notify, &[1, 5, 2])?;
    let peer_public = p256::PublicKey::from_sec1_bytes(public)?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer_public.as_affine());
    let encrypted = nested(notify, &[2])?;
    anyhow::ensure!(encrypted.len() >= 28, "Truncated peer credential");
    let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes()).unwrap();
    let credential = cipher
        .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
        .map_err(|_| anyhow::anyhow!("Peer credential GCM verification failed"))?;
    let info_reply = std::fs::read(directory.join("reply-1.bin"))?;
    let info = nested(&info_reply[4..], &[2, 2, 4, 2])?;
    let peer_type = wire::fields(info)
        .and_then(|v| {
            v.into_iter().find_map(|f| match f {
                wire::Field::Integer(5, v) => Some(v),
                _ => None,
            })
        })
        .ok_or_else(|| anyhow::anyhow!("Missing peer device type"))?;
    verify_credential(
        &credential,
        &std::fs::read(config.account_certificate)?,
        &session.user_id,
        peer_type,
        public,
        secret.public_key().to_encoded_point(false).as_bytes(),
    )?;
    println!(
        "Independent Rust verification passed: account-issued certificate, validity, same account/device and signed ephemeral keys."
    );
    Ok(())
}

/// Replay a privately saved request without contacting the phone or opening a port.
pub fn verify_file_request(config: &Path, directory: &Path, request: &Path) -> anyhow::Result<()> {
    let config: ProbeConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    let session = account::read_session(Path::new(&config.session))?;
    let identity: account::DeviceIdentity =
        serde_json::from_slice(&std::fs::read(&config.identity)?)?;
    let secret = SecretKey::from_slice(&std::fs::read(directory.join("ephemeral-key.bin"))?)?;
    let notify = std::fs::read(directory.join("reply-3.bin"))?;
    let info = std::fs::read(directory.join("reply-1.bin"))?;
    let (_, peer) = client_finished(
        &config,
        &session,
        &identity,
        &secret,
        notify
            .get(4..)
            .ok_or_else(|| anyhow::anyhow!("Truncated notify"))?,
        nested(
            info.get(4..)
                .ok_or_else(|| anyhow::anyhow!("Truncated device info"))?,
            &[2, 2, 4, 2],
        )?,
    )?;
    let finished = std::fs::read(directory.join("reply-4.bin"))?;
    let key = verify_finished(
        &secret,
        &std::fs::read(directory.join("client-random.bin"))?,
        &notify[4..],
        finished
            .get(4..)
            .ok_or_else(|| anyhow::anyhow!("Truncated Finished"))?,
    )?;
    let request = std::fs::read(request)?;
    let logical = nested(
        request
            .get(4..)
            .ok_or_else(|| anyhow::anyhow!("Truncated request"))?,
        &[2, 1],
    )?;
    anyhow::ensure!(
        crate::file_channel::incoming(logical, &key, &peer)?.is_some(),
        "Not a supported file-channel request"
    );
    println!(
        "Saved file request verified: same-account certificate, session Finished, AES-GCM and peer signature. No private values printed."
    );
    Ok(())
}
pub fn save_endpoints(path: &Path, endpoints: &[crate::discovery::Endpoint]) -> anyhow::Result<()> {
    private::save(path, &serde_json::to_vec(endpoints)?)
}

/// Configuration and raw replies stay in a private research directory.
#[derive(Clone, Copy, Debug)]
pub enum SyncEvent {
    Connecting(SocketAddr),
    Authenticating,
    Connected,
    Received(usize),
    Sent(usize),
    Unsupported,
    FileClipboardSkipped,
    FileClipboardOffered(u32),
    ImageClipboardOffered,
    FileClipboardRequested,
    FileClipboardSent(u32),
    FileClipboardFailed,
    UnsupportedClipboard,
    ShareStarted,
    ShareCompleted(u32),
    ShareFailed,
    File(crate::file_feedback::Event),
}

pub fn probe(config: &Path, directory: &Path) -> anyhow::Result<()> {
    exchange(config, Some(directory), None, None, &|event| {
        #[cfg(feature = "research")]
        if matches!(
            event,
            SyncEvent::File(crate::file_feedback::Event::Saved { .. })
        ) {
            crate::notifications::deliver(true);
        }
        #[cfg(not(feature = "research"))]
        let _ = event;
    })
}

#[cfg(feature = "clipboard")]
pub fn sync_once(
    config: &Path,
    stop: &std::sync::atomic::AtomicBool,
    on_event: &dyn Fn(SyncEvent),
) -> anyhow::Result<()> {
    exchange(config, None, Some(stop), None, on_event)
}

/// Run one independent authenticated session to a specific discovered device.
#[cfg(feature = "clipboard")]
pub fn sync_target(
    config: &Path,
    target: SocketAddr,
    stop: &std::sync::atomic::AtomicBool,
    on_event: &dyn Fn(SyncEvent),
) -> anyhow::Result<()> {
    exchange(config, None, Some(stop), Some(target), on_event)
}

pub fn validate_config(config: &Path) -> anyhow::Result<()> {
    let _: ProbeConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    Ok(())
}

fn exchange(
    config: &Path,
    directory: Option<&Path>,
    stop: Option<&std::sync::atomic::AtomicBool>,
    target: Option<SocketAddr>,
    on_event: &dyn Fn(SyncEvent),
) -> anyhow::Result<()> {
    macro_rules! diagnostic { ($($arg:tt)*) => { if directory.is_some() { println!($($arg)*); } }; }
    let base = config.canonicalize()?.parent().unwrap().to_path_buf();
    let mut config: ProbeConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    for value in [
        &mut config.session,
        &mut config.identity,
        &mut config.account_certificate,
    ] {
        if Path::new(value).is_relative() {
            *value = base.join(&*value).to_string_lossy().into_owned();
        }
    }
    if stop.is_some() {
        config.clipboard_sync = true;
        #[cfg(feature = "file-sharing")]
        if config.receive_directory.is_none() {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| anyhow::anyhow!("Home directory unavailable"))?;
            config.receive_directory = Some(
                std::path::PathBuf::from(home)
                    .join("Downloads")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    config
        .services
        .get_or_insert_with(crate::networking::Service::local_services);
    let selected_target = target;
    if stop.is_some() && selected_target.is_none() {
        if let Ok(endpoints) = crate::discovery::discover(Duration::from_secs(3)) {
            if let Some(address) = endpoints
                .iter()
                .flat_map(crate::discovery::Endpoint::remote_addresses)
                .find(|a| a.is_ipv4())
            {
                config.endpoint = address;
            }
        }
    }
    if let Some(target) = selected_target {
        config.endpoint = target;
    }
    anyhow::ensure!(config.endpoint.port() != 0, "No phone discovered");
    let save = |name: &str, bytes: &[u8]| -> anyhow::Result<()> {
        if let Some(directory) = directory {
            private::save(&directory.join(name), bytes)?;
        }
        Ok(())
    };
    let session = account::read_session(Path::new(&config.session))?;
    let identity: account::DeviceIdentity =
        serde_json::from_slice(&std::fs::read(&config.identity)?)?;
    let did = URL_SAFE_NO_PAD.decode(&identity.did)?;
    anyhow::ensure!(did.len() == 4, "Invalid local device identity");
    let key = loop {
        if let Ok(key) = SecretKey::from_slice(&private::random::<32>()?) {
            break key;
        }
    };
    let random = private::random::<32>()?;
    let hello = client_hello(key.public_key().to_encoded_point(false).as_bytes(), &random);
    let auth = sync_auth(
        &session.user_id,
        &private::random()?,
        u32::from_be_bytes(private::random()?),
        &hello,
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let request = wire::sync_request_with_auth(
        u32::from_be_bytes(did.try_into().unwrap()),
        u32::from_be_bytes(private::random()?),
        now,
        Some(&auth),
    );
    let bind = if config.endpoint.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    on_event(SyncEvent::Connecting(config.endpoint));
    let socket = UdpSocket::bind(bind)?;
    socket.connect(config.endpoint)?;
    #[cfg(feature = "file-sharing")]
    let local_ip = socket.local_addr()?.ip();
    let mut transport = Transport::new(socket)?;
    save("client-random.bin", &random)?;
    save("ephemeral-key.bin", &key.to_bytes())?;
    save("request.bin", &request)?;
    transport.send(&request)?;
    on_event(SyncEvent::Authenticating);
    let started = Instant::now();
    let deadline = started
        + Duration::from_secs(if stop.is_some() {
            86400
        } else {
            config.listen_seconds.unwrap_or(15).clamp(1, 86400)
        });
    let mut last_received = started;
    let mut count = 0;
    let mut info = None;
    let mut finished_sent = false;
    let mut server_notify = None;
    let mut authenticated = false;
    let mut active_key = None;
    let mut peer_key = None;
    // File services require the receiver feature and an explicit advertisement.
    let file_advertised = config.services.as_ref().is_some_and(|services| {
        services.iter().any(|s| {
            s.name == crate::networking::FILE_TRANSFER
                && s.package == crate::networking::MAC_PACKAGE
        })
    });
    let files_enabled = cfg!(feature = "file-sharing") && file_advertised;
    let mut file_notice = None;
    let mut file_connection: Option<(u8, u32)> = None;
    let mut file_service: Option<&'static str> = None;
    let mut file_port_binding: Option<crate::file_channel::PortBinding> = None;
    #[cfg(feature = "file-sharing")]
    let mut port_probe: Option<crate::file_channel::FileReceiver> = None;
    #[cfg(feature = "file-sharing")]
    let mut outgoing: Option<crate::share_sender::Sender> = None;
    #[cfg(feature = "file-sharing")]
    let mut share_scan = Instant::now();
    #[cfg(feature = "file-sharing")]
    let mut clipboard_offer: Option<std::sync::Arc<crate::clipboard_files::Offer>> = None;
    anyhow::ensure!(
        !config.clipboard_sync || cfg!(feature = "clipboard"),
        "Clipboard feature required"
    );
    #[cfg(feature = "clipboard")]
    let mut clipboard_count = if config.clipboard_sync {
        crate::clipboard::change_count()
    } else {
        0
    };
    // A manual interoperability run may start after the user copied its test
    // file. Inspect that current snapshot once; resident startup reoffers only
    // the current image via read_update and does not replay old text or files.
    #[cfg(all(feature = "research", feature = "clipboard"))]
    if directory.is_some() && config.clipboard_sync {
        clipboard_count = clipboard_count.wrapping_sub(1);
    }
    #[cfg(feature = "clipboard")]
    let mut last_text: Option<String> = None;
    #[cfg(feature = "clipboard")]
    let mut sequence = private::random::<1>()?[0];
    #[cfg(feature = "clipboard")]
    let mut network_ready = false;
    #[cfg(feature = "clipboard")]
    let mut initial_clipboard = true;
    while Instant::now() < deadline {
        #[cfg(feature = "file-sharing")]
        {
            if let Some(sender) = outgoing.as_mut() {
                match sender.poll() {
                    Ok(true) => {
                        on_event(SyncEvent::ShareCompleted(
                            sender.request.offer.files.len() as u32
                        ));
                        outgoing = None;
                        file_connection = None;
                        file_service = None;
                        file_notice = None;
                    }
                    Err(error) => {
                        eprintln!("Mi Clip outgoing share failed: {error:#}");
                        let _ = sender.request.fail(&format!("{error:#}"));
                        on_event(SyncEvent::ShareFailed);
                        outgoing = None;
                        file_connection = None;
                        file_service = None;
                        file_notice = None;
                    }
                    Ok(false) => (),
                }
            }
            if stop.is_some()
                && network_ready
                && outgoing.is_none()
                && file_connection.is_none()
                && share_scan.elapsed() >= Duration::from_millis(500)
            {
                share_scan = Instant::now();
                if let Some(root) = crate::share_queue::root() {
                    match crate::share_queue::Request::next_for(&root, Some(config.endpoint)) {
                        Ok(Some(request)) => {
                            let sender = crate::share_sender::Sender::new(request)?;
                            let signing = p256::ecdsa::SigningKey::from_slice(
                                &base64::engine::general_purpose::STANDARD
                                    .decode(&identity.private_key)?,
                            )?;
                            let inner = crate::file_channel::sync_reply_for_service(
                                active_key.as_ref().unwrap(),
                                &signing,
                                &session.user_id,
                                crate::file_channel::PHONE_SHARE_SERVICE,
                            )?;
                            let mut logical = integer(1, 2);
                            logical.extend(integer(3, sender.nonce.into()));
                            logical.extend(message(5, &inner));
                            transport.send(&frame(17, &message(2, &message(1, &logical)))?)?;
                            outgoing = Some(sender);
                            on_event(SyncEvent::ShareStarted);
                        }
                        Err(error) => {
                            eprintln!("Mi Clip share request rejected: {error:#}");
                        }
                        Ok(None) => (),
                    }
                }
            }
        }
        #[cfg(feature = "file-sharing")]
        if let Some(probe) = port_probe.as_mut() {
            let result = probe.poll();
            if let Some(files) = probe.take_sent() {
                last_received = Instant::now();
                on_event(SyncEvent::FileClipboardSent(files));
            }
            if let Some((received, total)) = probe.take_progress() {
                last_received = Instant::now();
                on_event(SyncEvent::File(crate::file_feedback::Event::Progress {
                    received,
                    total,
                }));
            }
            let saved = probe.take_saved();
            if let Some(files) = saved {
                on_event(SyncEvent::File(crate::file_feedback::Event::Saved {
                    files,
                }));
            }
            if let Err(error) = result {
                diagnostic!("File transport closed: {error:#}. Authentication remains active.");
                port_probe = None;
                file_port_binding = None;
                file_notice = None;
                if file_service.is_some_and(crate::file_channel::clipboard_service) {
                    on_event(SyncEvent::FileClipboardFailed);
                } else if saved.is_none() {
                    on_event(SyncEvent::File(crate::file_feedback::Event::Failed(
                        crate::file_feedback::Failure::Disconnected,
                    )));
                }
            }
        }
        if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed)) {
            break;
        }
        anyhow::ensure!(
            authenticated || started.elapsed() < Duration::from_secs(20),
            "Phone authentication timed out"
        );
        // Manual file probes must remain available between phone interactions.
        // Their overall configured deadline still bounds the session.
        anyhow::ensure!(
            (directory.is_some() && authenticated)
                || last_received.elapsed() < Duration::from_secs(120),
            "Phone connection timed out"
        );
        #[cfg(feature = "clipboard")]
        if config.clipboard_sync && network_ready && active_key.is_some() {
            let snapshot = crate::clipboard::read_update(clipboard_count, initial_clipboard);
            initial_clipboard = false;
            if let Some((count, content)) = snapshot {
                clipboard_count = count;
                #[cfg(feature = "file-sharing")]
                {
                    clipboard_offer = None;
                    if file_service.is_some_and(crate::file_channel::clipboard_service) {
                        port_probe = None;
                        file_connection = None;
                        file_port_binding = None;
                        file_notice = None;
                        file_service = None;
                    }
                }
                let text = match content {
                    crate::clipboard::Content::Text(text) => Some(text),
                    content @ (crate::clipboard::Content::Files(_)
                    | crate::clipboard::Content::Image(_)) => {
                        let image = matches!(&content, crate::clipboard::Content::Image(_));
                        #[cfg(feature = "file-sharing")]
                        {
                            sequence = (sequence % 127) + 1;
                            match crate::clipboard_files::Offer::from_content(content, sequence) {
                                Ok(offer) => {
                                    transport.send(&encrypted_payload(
                                        active_key.as_ref().unwrap(),
                                        &offer.announcement(),
                                    )?)?;
                                    on_event(if image {
                                        SyncEvent::ImageClipboardOffered
                                    } else {
                                        SyncEvent::FileClipboardOffered(offer.files.len() as u32)
                                    });
                                    diagnostic!(
                                        "Lazy file/image clipboard announced; {} items. No paths or contents printed.",
                                        offer.files.len()
                                    );
                                    clipboard_offer = Some(std::sync::Arc::new(offer));
                                    last_text = None;
                                }
                                Err(error) => {
                                    diagnostic!("Clipboard file offer unavailable: {error:#}.");
                                    on_event(SyncEvent::FileClipboardSkipped);
                                }
                            }
                        }
                        #[cfg(not(feature = "file-sharing"))]
                        {
                            let _ = content;
                            on_event(SyncEvent::FileClipboardSkipped);
                        }
                        None
                    }
                    crate::clipboard::Content::Unsupported => None,
                };
                if let Some(text) = text.filter(|t| Some(t) != last_text.as_ref()) {
                    sequence = (sequence % 127) + 1;
                    if let Some(payload) = crate::message_center::text_payload(&text, sequence) {
                        transport
                            .send(&encrypted_payload(active_key.as_ref().unwrap(), &payload)?)?;
                        on_event(SyncEvent::Sent(text.len()));
                        diagnostic!(
                            "Mac → phone: encrypted text sent, {} bytes; delivery needs phone verification.",
                            text.len()
                        );
                    } else {
                        on_event(SyncEvent::Unsupported);
                        diagnostic!(
                            "Clipboard content skipped: empty or beyond inline text limit."
                        );
                    }
                    last_text = Some(text);
                }
            }
        }
        let tick = Duration::from_millis(250);
        #[cfg(feature = "file-sharing")]
        let tick = if outgoing.is_some() || port_probe.as_ref().is_some_and(|probe| probe.running())
        {
            Duration::from_millis(20)
        } else {
            tick
        };
        let slice = deadline.min(Instant::now() + tick);
        let Some(reply) = transport.receive(slice)? else {
            continue;
        };
        last_received = Instant::now();
        count += 1;
        save(&format!("reply-{count}.bin"), &reply)?;
        diagnostic!(
            "Peer negotiation reply received: {} bytes; contents saved privately.",
            reply.len()
        );
        anyhow::ensure!(reply.len() >= 4, "Truncated peer frame");
        if let Ok(device) = nested(&reply[4..], &[2, 2, 4, 2]) {
            info = Some(device.to_vec());
        }
        if !finished_sent && nested(&reply[4..], &[2, 1, 5, 7, 2, 6, 3]).is_ok() {
            let (finished, verified_peer) = client_finished(
                &config,
                &session,
                &identity,
                &key,
                &reply[4..],
                info.as_deref()
                    .ok_or_else(|| anyhow::anyhow!("Missing peer device info"))?,
            )?;
            peer_key = Some(verified_peer);
            save("client-finished.bin", &finished)?;
            transport.send(&finished)?;
            finished_sent = true;
            server_notify = Some(reply[4..].to_vec());
            diagnostic!(
                "Peer certificate and ephemeral signature verified. Independent client certificate and signature sent."
            );
        }
        if nested(&reply[4..], &[2, 1, 5, 7, 2, 6, 5]).is_ok() {
            let session_key = verify_finished(
                &key,
                &random,
                server_notify
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("Unexpected server Finished"))?,
                &reply[4..],
            )?;
            save("session-key.bin", &session_key)?;
            authenticated = true;
            active_key = Some(session_key);
            diagnostic!(
                "Phone accepted independent client identity; server Finished and session key confirmation verified."
            );
            let mut request = message(2, b"00150323");
            request.extend(integer(4, 16));
            request.extend(integer(5, 10000));
            request.extend(integer(6, 256));
            request.extend(integer(7, 1));
            let mut inner = integer(1, 1);
            inner.extend(message(2, &request));
            let packet = logical_encrypted(&reply[4..], &session_key, &inner)?;
            save("connection-request.bin", &packet)?;
            transport.send(&packet)?;
        }
        if let (Some(session_key), Ok(logical)) =
            (active_key.as_ref(), nested(&reply[4..], &[2, 1]))
        {
            #[cfg(feature = "file-sharing")]
            if let (Some(sender), Some(peer)) = (outgoing.as_mut(), peer_key.as_ref()) {
                if number(logical, 2).unwrap_or(0) == 2
                    && number(logical, 3).unwrap_or(0) == u64::from(sender.nonce)
                    && number(logical, 4).unwrap_or(0) == 0
                    && nested(logical, &[5]).is_ok_and(|body| number(body, 1).unwrap_or(0) == 4)
                {
                    let code = nested(logical, &[5, 5])
                        .ok()
                        .and_then(|b| number(b, 1).ok())
                        .unwrap_or(0);
                    let reason = format!("手机关闭了分享通道（错误码 {code}），未开始发送文件");
                    let _ = sender.request.fail(&reason);
                    eprintln!("Mi Clip Share: {reason}");
                    on_event(SyncEvent::ShareFailed);
                    outgoing = None;
                    file_connection = None;
                    file_service = None;
                    continue;
                }
                if number(logical, 2).unwrap_or(0) == 2
                    && number(logical, 4).unwrap_or(0) == 0
                    && number(logical, 3).unwrap_or(0) == u64::from(sender.nonce)
                {
                    let incoming = match crate::file_channel::incoming_share_response(
                        logical,
                        session_key,
                        peer,
                        sender.nonce,
                    ) {
                        Ok(Some(incoming)) => incoming,
                        Ok(None) => continue,
                        Err(error) => {
                            let reason = format!("分享握手失败：{error:#}");
                            let _ = sender.request.fail(&reason);
                            eprintln!("Mi Clip Share: {reason}");
                            on_event(SyncEvent::ShareFailed);
                            outgoing = None;
                            file_connection = None;
                            file_service = None;
                            continue;
                        }
                    };
                    anyhow::ensure!(
                        incoming.service == crate::file_channel::PHONE_SHARE_SERVICE
                            && sender.peer.is_none(),
                        "Unexpected share proof"
                    );
                    sender.peer = Some(incoming.peer_id);
                    file_connection = Some((incoming.peer_id, incoming.nonce));
                    file_service = Some(crate::file_channel::PHONE_SHARE_SERVICE);
                    eprintln!(
                        "Mi Clip Share: phone service proof verified; requesting logical channel"
                    );
                    transport.send(&logical_packet(
                        2,
                        incoming.peer_id.into(),
                        incoming.nonce.into(),
                        session_key,
                        &sender.logical_request(),
                    )?)?;
                    continue;
                }
            }
            if file_advertised {
                if let Some(peer) = peer_key.as_ref() {
                    // Invalid file proofs must not interrupt clipboard sync or
                    // produce a trusted file notification in the resident app.
                    let incoming = match crate::file_channel::incoming(logical, session_key, peer) {
                        Ok(incoming) => incoming,
                        Err(error) if directory.is_some() => return Err(error),
                        Err(_) => None,
                    };
                    if let Some(incoming) = incoming {
                        #[cfg(feature = "file-sharing")]
                        if outgoing.is_some() {
                            continue;
                        }
                        let is_clipboard = crate::file_channel::clipboard_service(incoming.service);
                        #[cfg(feature = "file-sharing")]
                        if is_clipboard && clipboard_offer.is_none() {
                            continue;
                        }
                        if is_clipboard && !files_enabled {
                            continue;
                        }
                        let request = (incoming.peer_id, incoming.nonce);
                        if file_notice != Some(request) {
                            file_port_binding = None;
                            #[cfg(feature = "file-sharing")]
                            {
                                port_probe = None;
                            }
                            file_notice = Some(request);
                            file_service = Some(incoming.service);
                            if is_clipboard {
                                on_event(SyncEvent::FileClipboardRequested);
                            } else {
                                on_event(SyncEvent::File(crate::file_feedback::Event::Requested));
                            }
                            if !files_enabled {
                                on_event(SyncEvent::File(crate::file_feedback::Event::Failed(
                                    crate::file_feedback::Failure::Unavailable,
                                )));
                            }
                        }
                        if !files_enabled {
                            continue;
                        }
                        let signing = p256::ecdsa::SigningKey::from_slice(
                            &base64::engine::general_purpose::STANDARD
                                .decode(&identity.private_key)?,
                        )?;
                        let inner = crate::file_channel::sync_reply_for_service(
                            session_key,
                            &signing,
                            &session.user_id,
                            incoming.service,
                        )?;
                        // Sync-info itself stays unencrypted; its credential is AES-GCM protected.
                        let mut response = integer(1, 2);
                        response.extend(integer(2, incoming.peer_id.into()));
                        response.extend(integer(3, incoming.nonce.into()));
                        response.extend(message(5, &inner));
                        let response = frame(17, &message(2, &message(1, &response)))?;
                        save("file-connection-response.bin", &response)?;
                        transport.send(&response)?;
                        file_connection = Some((incoming.peer_id, incoming.nonce));
                        diagnostic!(
                            "File-channel peer proof verified; reciprocal session credential sent."
                        );
                    }
                }
            }
            if files_enabled && number(logical, 4).unwrap_or(0) != 1 {
                if let Ok(body) = nested(logical, &[5])
                    && number(body, 1).unwrap_or(0) == 4
                    && file_connection
                        == Some((
                            u8::try_from(number(logical, 1)?)?,
                            u32::try_from(number(logical, 3)?)?,
                        ))
                    && number(logical, 2).unwrap_or(0) == 2
                {
                    let code = nested(body, &[5])
                        .ok()
                        .and_then(|b| number(b, 1).ok())
                        .unwrap_or(0);
                    diagnostic!(
                        "Peer closed file logical connection: code {code}; ready for a new request."
                    );
                    #[cfg(feature = "file-sharing")]
                    if outgoing.take().is_some() {
                        on_event(SyncEvent::ShareFailed);
                    }
                    file_connection = None;
                    file_port_binding = None;
                    file_notice = None;
                    #[cfg(feature = "file-sharing")]
                    {
                        if let Some(probe) = port_probe.as_ref() {
                            probe.trace_peer_close(code);
                        }
                        if !port_probe.as_ref().is_some_and(|probe| probe.completed()) {
                            if file_service.is_some_and(crate::file_channel::clipboard_service) {
                                on_event(SyncEvent::FileClipboardFailed);
                            } else {
                                on_event(SyncEvent::File(crate::file_feedback::Event::Failed(
                                    crate::file_feedback::Failure::Disconnected,
                                )));
                            }
                        }
                        port_probe = None;
                    }
                    file_service = None;
                    continue;
                }
            }
            if number(logical, 4).unwrap_or(0) == 1 {
                let encrypted = nested(logical, &[5])?;
                anyhow::ensure!(encrypted.len() >= 28, "Truncated encrypted logical frame");
                let plaintext = Aes256Gcm::new_from_slice(session_key)
                    .unwrap()
                    .decrypt(Nonce::from_slice(&encrypted[..12]), &encrypted[12..])
                    .map_err(|_| anyhow::anyhow!("Logical frame authentication failed"))?;
                save(&format!("logical-{count}.bin"), &plaintext)?;
                diagnostic!(
                    "Authenticated logical response received: {} bytes, protocol type {}.",
                    plaintext.len(),
                    number(&plaintext, 1)?
                );
                if files_enabled && number(logical, 2).unwrap_or(0) == 2 {
                    anyhow::ensure!(
                        file_connection
                            == Some((
                                u8::try_from(number(logical, 1)?)?,
                                u32::try_from(number(logical, 3)?)?
                            )),
                        "Unexpected file-channel connection identity"
                    );
                    #[cfg(feature = "file-sharing")]
                    if let Some(sender) = outgoing.as_mut() {
                        anyhow::ensure!(
                            number(&plaintext, 1)? == 2,
                            "Unexpected share logical response"
                        );
                        let response = nested(&plaintext, &[3])?;
                        let code = number(response, 1).unwrap_or(0) as u32 as i32;
                        if code != 0 {
                            let reason =
                                format!("手机拒绝建立分享通道（错误码 {code}），文件尚未发送");
                            sender.request.fail(&reason)?;
                            eprintln!("Mi Clip Share: {reason}");
                            on_event(SyncEvent::ShareFailed);
                            outgoing = None;
                            file_connection = None;
                            file_service = None;
                            continue;
                        }
                        let mut ack = integer(1, 3);
                        ack.extend(message(4, &[]));
                        transport.send(&logical_encrypted(&reply[4..], session_key, &ack)?)?;
                        eprintln!(
                            "Mi Clip Share: phone accepted logical channel; requesting transfer port"
                        );
                        let mut payload = vec![sender.peer.unwrap(), 0];
                        payload.extend(sender.port_request()?);
                        transport.send(&frame(0x29, &payload)?)?;
                        continue;
                    }
                    if number(&plaintext, 1)? == 1 {
                        let request = nested(&plaintext, &[2])?;
                        anyhow::ensure!(
                            Some(nested(request, &[2])?) == file_service.map(str::as_bytes),
                            "Unexpected file-channel service"
                        );
                        file_port_binding = Some(crate::file_channel::PortBinding::from_user_info(
                            nested(request, &[3])?,
                        )?);
                        let mut response = integer(1, 2);
                        response.extend(message(3, &integer(3, 1)));
                        transport.send(&logical_encrypted(&reply[4..], session_key, &response)?)?;
                        diagnostic!(
                            "File-channel authenticated request received; connection accepted for control-message capture."
                        );
                    }
                    continue;
                }
                if number(&plaintext, 1)? == 2 {
                    let response = nested(&plaintext, &[3])?;
                    anyhow::ensure!(
                        number(response, 1).unwrap_or(0) == 0,
                        "Phone rejected logical connection"
                    );
                    let mut ack = integer(1, 3);
                    ack.extend(message(4, &[]));
                    transport.send(&logical_encrypted(&reply[4..], session_key, &ack)?)?;
                    diagnostic!("Encrypted logical connection accepted; acknowledgement sent.");
                    transport.send(&networking_probe(
                        &config,
                        &identity,
                        &session,
                        session_key,
                    )?)?;
                    #[cfg(feature = "clipboard")]
                    {
                        network_ready = true;
                    }
                    on_event(SyncEvent::Connected);
                }
            }
        }
        if let Ok(heartbeat) = nested(&reply[4..], &[2, 2, 6]) {
            let mut physical = integer(2, 5);
            physical.extend(message(7, heartbeat));
            transport.send(&frame(9, &message(2, &message(2, &physical)))?)?;
        }
        if reply[0] == 0x29 && reply.len() >= 34 {
            if let Some(key) = active_key.as_ref() {
                if files_enabled && reply[4] == 2 && file_connection.is_none() {
                    diagnostic!("Late file payload ignored after logical channel closure.");
                    continue;
                }
                if files_enabled
                    && file_connection.is_some()
                    && reply[4] == 2
                    && matches!(reply[5], 0 | 1)
                {
                    let plaintext = if reply[5] == 1 {
                        Aes256Gcm::new_from_slice(key)
                            .unwrap()
                            .decrypt(Nonce::from_slice(&reply[6..18]), &reply[18..])
                            .map_err(|_| {
                                anyhow::anyhow!("File-channel payload authentication failed")
                            })?
                    } else {
                        reply[6..].to_vec()
                    };
                    save(&format!("file-payload-{count}.bin"), &plaintext)?;
                    match crate::file_channel::control(&plaintext) {
                        Ok(control) => {
                            diagnostic!(
                                "Authenticated file control: type {}; message {}; {} body bytes.",
                                control.kind,
                                control.message_id,
                                control.body.len()
                            );
                            #[cfg(feature = "file-sharing")]
                            if let Some(sender) = outgoing.as_mut() {
                                sender.connect(&control, config.endpoint)?;
                                continue;
                            }
                            #[cfg(feature = "file-sharing")]
                            if control.kind == 2 {
                                let request = crate::file_channel::port_request(&control)?;
                                file_port_binding
                                    .as_ref()
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "Bulk control arrived before authenticated file request"
                                        )
                                    })?
                                    .verify(&request)?;
                                let mut probe = crate::file_channel::FileReceiver::open(
                                    local_ip,
                                    config.endpoint.ip(),
                                    directory,
                                    request.transfer_key,
                                    if file_service
                                        .is_some_and(crate::file_channel::clipboard_service)
                                    {
                                        None
                                    } else {
                                        config.receive_directory.as_deref().map(Path::new)
                                    },
                                )?;
                                if file_service.is_some_and(crate::file_channel::clipboard_service)
                                {
                                    probe.serve_clipboard(
                                        clipboard_offer
                                            .as_ref()
                                            .ok_or_else(|| {
                                                anyhow::anyhow!("Expired clipboard offer")
                                            })?
                                            .clone(),
                                    );
                                }
                                let response =
                                    crate::file_channel::port_response(&request, 1, probe.port()?)?;
                                let mut payload = vec![file_connection.unwrap().0, 0];
                                payload.extend(response);
                                transport.send(&frame(0x29, &payload)?)?;
                                port_probe = Some(probe);
                                diagnostic!(
                                    "Bulk port response sent; identifiers and keys remain private."
                                );
                            }
                        }
                        Err(_) => diagnostic!(
                            "Authenticated file payload has an unsupported control header: {} bytes.",
                            plaintext.len()
                        ),
                    }
                    continue;
                }
                anyhow::ensure!(
                    reply[4] == 1 && reply[5] == 1,
                    "Unexpected networking destination or encryption mode"
                );
                let plaintext = Aes256Gcm::new_from_slice(key)
                    .unwrap()
                    .decrypt(Nonce::from_slice(&reply[6..18]), &reply[18..])
                    .map_err(|_| anyhow::anyhow!("Networking response authentication failed"))?;
                save(&format!("payload-{count}.bin"), &plaintext)?;
                if let Some(peer) = crate::networking::peer(&plaintext) {
                    diagnostic!(
                        "Authenticated peer capabilities received; native file sharing: {}. Device identifiers remain private.",
                        peer.can_share_files()
                    );
                }
                #[cfg(feature = "clipboard")]
                if config.clipboard_sync {
                    match crate::message_center::clipboard_event(&plaintext) {
                        Some(crate::message_center::ClipboardEvent::Text(text)) => {
                            if last_text.as_deref() != Some(text) {
                                #[cfg(feature = "file-sharing")]
                                {
                                    clipboard_offer = None;
                                    if file_service
                                        .is_some_and(crate::file_channel::clipboard_service)
                                    {
                                        port_probe = None;
                                        file_connection = None;
                                        file_port_binding = None;
                                        file_notice = None;
                                        file_service = None;
                                    }
                                }
                                clipboard_count = crate::clipboard::write(text)?;
                                last_text = Some(text.to_owned());
                                on_event(SyncEvent::Received(text.len()));
                                diagnostic!(
                                    "Phone → Mac: text written to native clipboard, {} bytes.",
                                    text.len()
                                );
                            }
                        }
                        Some(crate::message_center::ClipboardEvent::Unsupported) => {
                            on_event(SyncEvent::UnsupportedClipboard)
                        }
                        None => (),
                    }
                }
                if plaintext.first() == Some(&1) {
                    if let Ok(packet) = nested(&plaintext[1..], &[8, 1]) {
                        if let Some(topics) = crate::message_center::decode(packet) {
                            for topic in topics {
                                diagnostic!(
                                    "Authenticated topic: {}; base {} bytes; extension {} bytes. Contents remain private.",
                                    crate::message_center::topic_name(topic.id),
                                    topic.base.len(),
                                    topic.extension.len()
                                );
                            }
                        } else {
                            diagnostic!("Unsupported or malformed message-center record; ignored.");
                        }
                    }
                }
                diagnostic!(
                    "Encrypted service payload authenticated: {} bytes.",
                    plaintext.len()
                );
            }
        }
        anyhow::ensure!(
            stop.is_some() || count <= 512,
            "Negotiation reply limit reached"
        );
    }
    anyhow::ensure!(
        count > 0,
        "Phone negotiation timed out; authentication not established"
    );
    diagnostic!(
        "Exchange ended. Authenticated: {authenticated}; clipboard enabled: {}.",
        config.clipboard_sync
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unoffered_server_suites_and_invalid_key_formats() {
        let key = SecretKey::from_slice(&[1; 32]).unwrap();
        let public = key.public_key().to_encoded_point(false);
        let notify = |version, protocol, cipher, random_len, key_type, bytes: &[u8]| {
            let mut suite = integer(1, version);
            suite.extend(message(2, &vec![2; random_len]));
            suite.extend(integer(3, protocol));
            suite.extend(integer(4, cipher));
            let mut pk = integer(1, key_type);
            pk.extend(message(2, bytes));
            suite.extend(message(5, &pk));
            message(1, &suite)
        };
        assert!(server_suites(&notify(1, 16, 8, 32, 1, public.as_bytes())).is_ok());
        for invalid in [
            notify(2, 16, 8, 32, 1, public.as_bytes()),
            notify(1, 8, 16, 32, 1, public.as_bytes()),
            notify(1, 16, 8, 31, 1, public.as_bytes()),
            notify(1, 16, 8, 32, 2, public.as_bytes()),
            notify(1, 16, 8, 32, 1, &public.as_bytes()[..64]),
        ] {
            assert!(server_suites(&invalid).is_err());
        }
    }
    #[test]
    fn hello_uses_uncompressed_p256_and_same_account_suite() {
        let key = SecretKey::from_slice(&[1; 32]).unwrap();
        let public = key.public_key().to_encoded_point(false);
        let hello = client_hello(public.as_bytes(), &[2; 32]);
        let pair = wire::bytes_field(&hello, 6).unwrap();
        let notify = wire::bytes_field(pair, 2).unwrap();
        let suites = wire::bytes_field(notify, 1).unwrap();
        let fields = wire::fields(suites).unwrap();
        assert!(fields.contains(&wire::Field::Integer(3, 16)));
        assert!(fields.contains(&wire::Field::Integer(4, 8)));
        let public_field = wire::bytes_field(suites, 5).unwrap();
        assert_eq!(
            wire::bytes_field(public_field, 2).unwrap(),
            public.as_bytes()
        );
    }
}
