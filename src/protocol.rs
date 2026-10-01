pub const HASH_WIDTH_IN_BYTES: usize = 32;

use crate::helper::write_and_flush;
use anyhow::{bail, Context, Result};
use lazy_static::lazy_static;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// The maximum size of a length-prefixed payload, to reject a malformed length
const MAX_PAYLOAD_SIZE: usize = 1024 * 1024;

type ProtocolVersion = u8;
const PROTO_V1: u8 = 1u8;

pub const CURRENT_PROTO_VERSION: ProtocolVersion = PROTO_V1;

pub type Digest = [u8; HASH_WIDTH_IN_BYTES];

/// The variants are named after the kind of channel they establish, so the shared
/// `ChannelHello` postfix is intended
#[allow(clippy::enum_variant_names)]
#[derive(Deserialize, Serialize, Debug)]
pub enum Hello {
    /// Sent by a client to represent a tunnel. sha256sum(tunnel name)
    ControlChannelHello(ProtocolVersion, Digest),
    /// Sent by a client to establish a data channel. The session key handed out by the control channel
    DataChannelHello(ProtocolVersion, Digest),
    /// Sent by a client to ask for its configuration. sha256sum(client name)
    ConfigChannelHello(ProtocolVersion, Digest),
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Auth(pub Digest);

#[derive(Deserialize, Serialize, Debug)]
pub enum Ack {
    Ok,
    TunnelNotExist,
    AuthFailed,
}

impl std::fmt::Display for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Ack::Ok => "Ok",
                Ack::TunnelNotExist => "No such a client or tunnel",
                Ack::AuthFailed => "Incorrect token",
            }
        )
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub enum ControlChannelCmd {
    CreateDataChannel,
    HeartBeat,
}

#[derive(Deserialize, Serialize, Debug)]
pub enum DataChannelCmd {
    StartForwardTcp,
}

pub fn digest(data: &[u8]) -> Digest {
    use sha2::{Digest, Sha256};
    let d = Sha256::new().chain_update(data).finalize();
    d.into()
}

struct PacketLength {
    hello: usize,
    ack: usize,
    auth: usize,
    c_cmd: usize,
    d_cmd: usize,
}

impl PacketLength {
    pub fn new() -> PacketLength {
        let username = "default";
        let d = digest(username.as_bytes());
        let hello = bincode::serialized_size(&Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d))
            .unwrap() as usize;
        let c_cmd =
            bincode::serialized_size(&ControlChannelCmd::CreateDataChannel).unwrap() as usize;
        let d_cmd = bincode::serialized_size(&DataChannelCmd::StartForwardTcp).unwrap() as usize;
        let ack = Ack::Ok;
        let ack = bincode::serialized_size(&ack).unwrap() as usize;

        let auth = bincode::serialized_size(&Auth(d)).unwrap() as usize;
        PacketLength {
            hello,
            ack,
            auth,
            c_cmd,
            d_cmd,
        }
    }
}

lazy_static! {
    static ref PACKET_LEN: PacketLength = PacketLength::new();
}

pub async fn read_hello<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Hello> {
    let mut buf = vec![0u8; PACKET_LEN.hello];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read hello")?;
    let hello = bincode::deserialize(&buf).with_context(|| "Failed to deserialize hello")?;

    let v = match hello {
        Hello::ControlChannelHello(v, _) => v,
        Hello::DataChannelHello(v, _) => v,
        Hello::ConfigChannelHello(v, _) => v,
    };

    if v != CURRENT_PROTO_VERSION {
        bail!(
            "Protocol version mismatched. Expected {}, got {}.",
            CURRENT_PROTO_VERSION,
            v
        );
    }

    Ok(hello)
}

pub async fn read_auth<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Auth> {
    let mut buf = vec![0u8; PACKET_LEN.auth];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read auth")?;
    bincode::deserialize(&buf).with_context(|| "Failed to deserialize auth")
}

pub async fn read_ack<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut T) -> Result<Ack> {
    let mut bytes = vec![0u8; PACKET_LEN.ack];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read ack")?;
    bincode::deserialize(&bytes).with_context(|| "Failed to deserialize ack")
}

pub async fn read_control_cmd<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<ControlChannelCmd> {
    let mut bytes = vec![0u8; PACKET_LEN.c_cmd];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read cmd")?;
    bincode::deserialize(&bytes).with_context(|| "Failed to deserialize control cmd")
}

pub async fn read_data_cmd<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
) -> Result<DataChannelCmd> {
    let mut bytes = vec![0u8; PACKET_LEN.d_cmd];
    conn.read_exact(&mut bytes)
        .await
        .with_context(|| "Failed to read cmd")?;
    bincode::deserialize(&bytes).with_context(|| "Failed to deserialize data cmd")
}

/// Write a bincode payload prefixed by its length in bytes (big-endian u32)
pub async fn write_payload<T, W>(conn: &mut W, payload: &T) -> Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let bytes = bincode::serialize(payload).with_context(|| "Failed to serialize the payload")?;
    if bytes.len() > MAX_PAYLOAD_SIZE {
        bail!("The payload is too large: {} bytes", bytes.len());
    }

    write_and_flush(conn, &(bytes.len() as u32).to_be_bytes())
        .await
        .with_context(|| "Failed to write the payload length")?;
    write_and_flush(conn, &bytes)
        .await
        .with_context(|| "Failed to write the payload")?;

    Ok(())
}

/// Read a bincode payload prefixed by its length in bytes (big-endian u32)
pub async fn read_payload<T, R>(conn: &mut R) -> Result<T>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let mut len = [0u8; 4];
    conn.read_exact(&mut len)
        .await
        .with_context(|| "Failed to read the payload length")?;

    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_PAYLOAD_SIZE {
        bail!("The payload is too large: {} bytes", len);
    }

    let mut buf = vec![0u8; len];
    conn.read_exact(&mut buf)
        .await
        .with_context(|| "Failed to read the payload")?;

    bincode::deserialize(&buf).with_context(|| "Failed to deserialize the payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hello_packet_size() {
        let d = digest(b"foo");
        let sizes: Vec<usize> = [
            Hello::ControlChannelHello(CURRENT_PROTO_VERSION, d),
            Hello::DataChannelHello(CURRENT_PROTO_VERSION, d),
            Hello::ConfigChannelHello(CURRENT_PROTO_VERSION, d),
        ]
        .iter()
        .map(|h| bincode::serialized_size(h).unwrap() as usize)
        .collect();

        assert!(sizes.iter().all(|s| *s == sizes[0]), "{:?}", sizes);
        assert_eq!(sizes[0], PACKET_LEN.hello);
    }
}
