//! Bounded Minecraft Java Server List Ping. This never logs in or joins a world.

use std::{
    net::IpAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
};

const DEFAULT_PORT: u16 = 25565;
const MAX_STATUS_PACKET_SIZE: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ServerStatusError {
    #[error("invalid Minecraft server address")]
    InvalidAddress,
    #[error("Minecraft server status request timed out")]
    Timeout,
    #[error("Minecraft server connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid Minecraft server status response: {0}")]
    InvalidResponse(&'static str),
}

pub async fn ping_minecraft_server(
    address: &str,
    deadline: Duration,
) -> Result<Duration, ServerStatusError> {
    let (host, port) = parse_address(address)?;
    tokio::time::timeout(deadline, query(&host, port))
        .await
        .map_err(|_| ServerStatusError::Timeout)?
}

fn parse_address(address: &str) -> Result<(String, u16), ServerStatusError> {
    let address = address.trim();
    let invalid = || ServerStatusError::InvalidAddress;
    if address.is_empty()
        || address == "—"
        || address.len() > 255
        || address
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "/\\?#@%".contains(c))
    {
        return Err(invalid());
    }
    let (host, port) = if let Some(bracketed) = address.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']').ok_or_else(invalid)?;
        host.parse::<std::net::Ipv6Addr>().map_err(|_| invalid())?;
        let port = if suffix.is_empty() {
            DEFAULT_PORT
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?
        };
        (host, port)
    } else if address.parse::<IpAddr>().is_ok() {
        (address, DEFAULT_PORT)
    } else if let Some((host, port)) = address.split_once(':') {
        (host, port.parse().map_err(|_| invalid())?)
    } else {
        (address, DEFAULT_PORT)
    };
    if host.is_empty() || port == 0 {
        return Err(invalid());
    }
    let host = if host.parse::<IpAddr>().is_ok() {
        host.to_owned()
    } else {
        url::Host::parse(host).map_err(|_| invalid())?.to_string()
    };
    Ok((host, port))
}

async fn query(host: &str, port: u16) -> Result<Duration, ServerStatusError> {
    let mut stream = TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true)?;
    let mut handshake = vec![0];
    // -1 means an unspecified client version. Status is independent of login compatibility.
    append_varint(&mut handshake, u32::MAX);
    append_varint(&mut handshake, host.len() as u32);
    handshake.extend_from_slice(host.as_bytes());
    handshake.extend_from_slice(&port.to_be_bytes());
    handshake.push(1); // status state
    let mut request = Vec::with_capacity(handshake.len() + 7);
    append_varint(&mut request, handshake.len() as u32);
    request.extend_from_slice(&handshake);
    request.extend_from_slice(&[1, 0]); // status request
    stream.write_all(&request).await?;

    let response = read_packet(&mut stream).await?;
    let mut body = response.as_slice();
    if take_varint(&mut body)? != 0 {
        return Err(ServerStatusError::InvalidResponse(
            "expected a status packet",
        ));
    }
    let json_length = take_varint(&mut body)? as usize;
    if json_length != body.len() {
        return Err(ServerStatusError::InvalidResponse(
            "incorrect status string length",
        ));
    }
    let status: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| ServerStatusError::InvalidResponse("status is not valid JSON"))?;
    if !status.is_object()
        || status
            .get("version")
            .and_then(|v| v.get("protocol"))
            .and_then(|v| v.as_i64())
            .is_none()
    {
        return Err(ServerStatusError::InvalidResponse(
            "missing Minecraft version",
        ));
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut ping = vec![9, 1];
    ping.extend_from_slice(&nonce.to_be_bytes());
    // Measure just the actual ping/pong RTT, excluding DNS, connect and status JSON.
    let started = Instant::now();
    stream.write_all(&ping).await?;
    let pong = read_packet(&mut stream).await?;
    let latency = started.elapsed();
    if pong.len() != 9 || pong[0] != 1 || pong[1..] != nonce.to_be_bytes() {
        return Err(ServerStatusError::InvalidResponse(
            "pong does not match the ping",
        ));
    }
    Ok(latency)
}

fn append_varint(bytes: &mut Vec<u8>, mut value: u32) {
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        bytes.push(if value == 0 { low } else { low | 0x80 });
        if value == 0 {
            break;
        }
    }
}

fn varint_byte(value: &mut u32, index: u32, byte: u8) -> Result<bool, ServerStatusError> {
    if index == 4 && byte > 0x0f {
        return Err(ServerStatusError::InvalidResponse("VarInt is too large"));
    }
    *value |= u32::from(byte & 0x7f) << (index * 7);
    Ok(byte & 0x80 == 0)
}

fn take_varint(bytes: &mut &[u8]) -> Result<u32, ServerStatusError> {
    let mut value = 0;
    for index in 0..5 {
        let (&byte, remaining) = bytes
            .split_first()
            .ok_or(ServerStatusError::InvalidResponse("truncated VarInt"))?;
        *bytes = remaining;
        if varint_byte(&mut value, index, byte)? {
            return Ok(value);
        }
    }
    Err(ServerStatusError::InvalidResponse("VarInt is too long"))
}

async fn read_packet(stream: &mut TcpStream) -> Result<Vec<u8>, ServerStatusError> {
    let mut length = 0;
    for index in 0..5 {
        if varint_byte(&mut length, index, stream.read_u8().await?)? {
            let length = length as usize;
            if length == 0 || length > MAX_STATUS_PACKET_SIZE {
                return Err(ServerStatusError::InvalidResponse(
                    "packet size exceeds the limit",
                ));
            }
            let mut packet = vec![0; length];
            stream.read_exact(&mut packet).await?;
            return Ok(packet);
        }
    }
    Err(ServerStatusError::InvalidResponse("VarInt is too long"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn fake_server(
        pong_delay: Duration,
        wrong_pong: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // The handshake fits in one byte of length for this local address.
            let length = stream.read_u8().await.unwrap() as usize;
            let mut handshake = vec![0; length];
            stream.read_exact(&mut handshake).await.unwrap();
            assert_eq!(handshake[0], 0); // handshake packet ID
            assert_eq!(*handshake.last().unwrap(), 1); // status, not login
            assert_eq!(stream.read_u8().await.unwrap(), 1);
            assert_eq!(stream.read_u8().await.unwrap(), 0); // status request
            let json = br#"{"version":{"name":"1.21.1","protocol":767},"players":{"max":20,"online":3},"description":{"text":"Test"}}"#;
            let mut response = vec![(json.len() + 2) as u8, 0, json.len() as u8];
            response.extend_from_slice(json);
            // Split the frame to exercise reads across TCP packet boundaries.
            stream.write_all(&response[..2]).await.unwrap();
            stream.write_all(&response[2..]).await.unwrap();
            let mut ping = [0; 10];
            stream.read_exact(&mut ping).await.unwrap();
            assert_eq!(ping[..2], [9, 1]);
            tokio::time::sleep(pong_delay).await;
            if wrong_pong {
                ping[9] ^= 1;
            }
            let _ = stream.write_all(&ping).await;
        });
        (address, task)
    }

    #[tokio::test]
    async fn measures_minecraft_pong_instead_of_tcp_connection_time() {
        let (address, task) = fake_server(Duration::from_millis(80), false).await;
        let latency = ping_minecraft_server(&address, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(
            latency >= Duration::from_millis(75),
            "the delayed pong must contribute to latency"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_a_pong_that_does_not_echo_the_request() {
        let (address, task) = fake_server(Duration::ZERO, true).await;
        assert!(matches!(
            ping_minecraft_server(&address, Duration::from_secs(2)).await,
            Err(ServerStatusError::InvalidResponse(_))
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn bounds_a_server_that_accepts_tcp_but_never_responds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        assert!(matches!(
            ping_minecraft_server(&address, Duration::from_millis(80)).await,
            Err(ServerStatusError::Timeout)
        ));
        task.abort();
    }

    #[tokio::test]
    async fn rejects_an_oversized_response_before_allocating_its_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let length = stream.read_u8().await.unwrap() as usize;
            let mut request = vec![0; length + 2];
            stream.read_exact(&mut request).await.unwrap();
            let _ = stream.write_all(&[0xff, 0xff, 0xff, 0x7f]).await;
        });
        assert!(matches!(
            ping_minecraft_server(&address, Duration::from_secs(1)).await,
            Err(ServerStatusError::InvalidResponse(_))
        ));
        task.await.unwrap();
    }

    #[test]
    fn parses_hostnames_ports_and_ipv6_without_accepting_urls() {
        for (address, host, port) in [
            ("example.org", "example.org", 25565),
            ("example.org:25566", "example.org", 25566),
            ("127.0.0.1:3000", "127.0.0.1", 3000),
            ("[::1]:25566", "::1", 25566),
            ("::1", "::1", 25565),
        ] {
            assert_eq!(parse_address(address).unwrap(), (host.to_owned(), port));
        }
        for address in [
            "",
            "—",
            "http://example.org",
            "example.org:0",
            "example.org:65536",
            "example.org:abc",
            "example.org/path",
            "bad host",
            "[bad]:25565",
        ] {
            assert!(parse_address(address).is_err(), "{address}");
        }
    }

    #[test]
    fn rejects_truncated_and_overflowing_varints() {
        assert!(take_varint(&mut &[0x80][..]).is_err());
        assert!(take_varint(&mut &[0xff, 0xff, 0xff, 0xff, 0xff][..]).is_err());
        assert_eq!(
            take_varint(&mut &[0xff, 0xff, 0xff, 0xff, 0x0f][..]).unwrap(),
            u32::MAX
        );
    }
}
