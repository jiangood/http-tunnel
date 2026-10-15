pub const HASH_WIDTH_IN_BYTES: usize = 32;

use crate::helper::write_and_flush;
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

/// The maximum size of a length-prefixed payload, to reject a malformed length
const MAX_PAYLOAD_SIZE: usize = 1024 * 1024;

type ProtocolVersion = u8;
const PROTO_V3: u8 = 3u8;

// V3 removes the config channel and inverts the source of truth: a client is
// configured by its own file and declares the domain of each control channel it
// opens, instead of having its tunnels pushed by the server. The hello messages
// are length-prefixed payloads now, so a domain of any length fits, and the
// fixed-size framing (`PACKET_LEN`) is gone.
pub const CURRENT_PROTO_VERSION: ProtocolVersion = PROTO_V3;

pub type Digest = [u8; HASH_WIDTH_IN_BYTES];

/// The first message a client sends on a new connection.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum ClientHello {
    /// Opens a control channel that serves `domain`. `name` is the name of the
    /// client; it's used for logging and to warn when a domain changes hands.
    /// `nodelay` is the client's TCP_NODELAY preference for the data channels of
    /// the tunnel, which the server applies to the socket of each data channel.
    Control {
        version: ProtocolVersion,
        domain: String,
        name: String,
        nodelay: bool,
    },
    /// Opens a data channel. `nonce` is the session key handed out by the
    /// control channel.
    Data {
        version: ProtocolVersion,
        nonce: Digest,
    },
}

impl ClientHello {
    pub fn version(&self) -> ProtocolVersion {
        match self {
            ClientHello::Control { version, .. } | ClientHello::Data { version, .. } => *version,
        }
    }
}

/// The answer of the server to a `ClientHello::Control`, carrying the nonce the
/// client authenticates against.
#[derive(Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub version: ProtocolVersion,
    pub nonce: Digest,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Auth(pub Digest);

#[derive(Deserialize, Serialize, Debug)]
pub enum Ack {
    Ok,
    AuthFailed,
}

impl std::fmt::Display for Ack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Ack::Ok => "Ok",
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

/// Reject a peer that speaks another version of the protocol
pub fn check_version(version: ProtocolVersion) -> Result<()> {
    if version != CURRENT_PROTO_VERSION {
        bail!(
            "Protocol version mismatched. Expected {}, got {}.",
            CURRENT_PROTO_VERSION,
            version
        );
    }
    Ok(())
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

    use tokio::io::{duplex, AsyncWriteExt};

    #[test]
    fn test_client_hello_roundtrip() {
        let hellos = [
            ClientHello::Control {
                version: CURRENT_PROTO_VERSION,
                domain: "nas.example.com".to_string(),
                name: "home".to_string(),
                nodelay: true,
            },
            // A long domain must fit: the frame is length-prefixed
            ClientHello::Control {
                version: CURRENT_PROTO_VERSION,
                domain: "a".repeat(500) + ".example.com",
                name: "home".to_string(),
                nodelay: false,
            },
            ClientHello::Data {
                version: CURRENT_PROTO_VERSION,
                nonce: digest(b"nonce"),
            },
        ];
        for hello in hellos {
            let bytes = bincode::serialize(&hello).unwrap();
            let back: ClientHello = bincode::deserialize(&bytes).unwrap();
            assert_eq!(back, hello);
        }
    }

    #[tokio::test]
    async fn test_payload_framing() -> Result<()> {
        let (mut a, mut b) = duplex(64 * 1024);
        let hello = ClientHello::Control {
            version: CURRENT_PROTO_VERSION,
            domain: "echo.test".to_string(),
            name: "home".to_string(),
            nodelay: true,
        };
        write_payload(&mut a, &hello).await?;
        a.flush().await?;
        let back: ClientHello = read_payload(&mut b).await?;
        assert_eq!(back, hello);
        Ok(())
    }

    #[test]
    fn test_check_version() {
        assert!(check_version(CURRENT_PROTO_VERSION).is_ok());
        assert!(check_version(CURRENT_PROTO_VERSION.wrapping_add(1)).is_err());
    }

    #[test]
    fn test_server_hello_and_auth_roundtrip() {
        let hello = ServerHello {
            version: CURRENT_PROTO_VERSION,
            nonce: digest(b"nonce"),
        };
        let bytes = bincode::serialize(&hello).unwrap();
        let back: ServerHello = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, hello);

        let auth = Auth(digest(b"token"));
        let bytes = bincode::serialize(&auth).unwrap();
        let Auth(back) = bincode::deserialize::<Auth>(&bytes).unwrap();
        assert_eq!(back, auth.0);
    }

    #[test]
    fn test_acks_and_cmds_roundtrip() {
        for ack in [Ack::Ok, Ack::AuthFailed] {
            let bytes = bincode::serialize(&ack).unwrap();
            let back: Ack = bincode::deserialize(&bytes).unwrap();
            assert_eq!(format!("{}", back), format!("{}", ack));
        }

        for cmd in [
            ControlChannelCmd::CreateDataChannel,
            ControlChannelCmd::HeartBeat,
        ] {
            let bytes = bincode::serialize(&cmd).unwrap();
            let back: ControlChannelCmd = bincode::deserialize(&bytes).unwrap();
            assert_eq!(format!("{:?}", back), format!("{:?}", cmd));
        }

        let bytes = bincode::serialize(&DataChannelCmd::StartForwardTcp).unwrap();
        let back: DataChannelCmd = bincode::deserialize(&bytes).unwrap();
        assert_eq!(format!("{:?}", back), "StartForwardTcp");
    }
}
