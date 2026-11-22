//! The proxy side of a dynamic forward (`ssh -D`): SOCKS5 without
//! authentication, and SOCKS4 and 4a for older programs. Only CONNECT is
//! offered; the SSH server then makes the connection.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V4,
    V5,
}

/// Where the client wants to go.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub version: Version,
    /// A name, or an address written out. Names are resolved by the server,
    /// which is what `curl --socks5-hostname` and browsers expect.
    pub host: String,
    pub port: u16,
}

/// Why a request is turned down, as SOCKS5 replies say it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    General = 1,
    NotAllowed = 2,
    HostUnreachable = 4,
    CommandNotSupported = 7,
    AddressNotSupported = 8,
}

#[derive(Debug)]
pub enum Error {
    /// The client went away, or said too little.
    Io,
    /// Not SOCKS at all, e.g. a browser speaking HTTP to the proxy port.
    NotSocks,
    /// The client insists on authentication, which is not offered.
    NoAcceptableMethod,
    /// BIND or UDP ASSOCIATE. The client was told.
    Unsupported,
    /// An address of a type SOCKS does not define. The client was told.
    BadAddress,
}

impl From<io::Error> for Error {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

impl Error {
    /// What to tell the user; nothing when the client merely went away.
    pub fn describe(&self) -> Option<&'static str> {
        match self {
            Self::Io => None,
            Self::NotSocks => Some(tr!(
                "Une application a parlé au proxy sans utiliser SOCKS : réglez-la sur un proxy SOCKS5, pas HTTP"
            )),
            Self::NoAcceptableMethod => {
                Some(tr!("Une application exige une authentification SOCKS, que ce proxy ne demande pas"))
            }
            Self::Unsupported => Some(tr!("Une application a demandé autre chose qu'une connexion (BIND ou UDP)")),
            Self::BadAddress => Some(tr!("Une application a envoyé une adresse SOCKS illisible")),
        }
    }
}

/// Reads the client's greeting and request, answering the greeting. The
/// request itself is answered by [`reply`] once the server tried.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Request, Error> {
    match stream.read_u8().await? {
        5 => socks5(stream).await,
        4 => socks4(stream).await,
        _ => Err(Error::NotSocks),
    }
}

async fn socks5<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Request, Error> {
    let count = stream.read_u8().await?;
    let mut methods = vec![0; usize::from(count)];
    stream.read_exact(&mut methods).await?;
    // 0: no authentication.
    if !methods.contains(&0) {
        stream.write_all(&[5, 0xff]).await?;
        return Err(Error::NoAcceptableMethod);
    }
    stream.write_all(&[5, 0]).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let [version, command, _, kind] = head;
    if version != 5 {
        return Err(Error::NotSocks);
    }
    let host = match kind {
        1 => {
            let mut address = [0u8; 4];
            stream.read_exact(&mut address).await?;
            Ipv4Addr::from(address).to_string()
        }
        3 => {
            let length = stream.read_u8().await?;
            let mut name = vec![0; usize::from(length)];
            stream.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(name) if !name.is_empty() => name,
                _ => return refuse(stream, Version::V5, Refusal::AddressNotSupported, Error::BadAddress).await,
            }
        }
        4 => {
            let mut address = [0u8; 16];
            stream.read_exact(&mut address).await?;
            Ipv6Addr::from(address).to_string()
        }
        _ => return refuse(stream, Version::V5, Refusal::AddressNotSupported, Error::BadAddress).await,
    };
    let port = stream.read_u16().await?;
    if command != 1 {
        return refuse(stream, Version::V5, Refusal::CommandNotSupported, Error::Unsupported).await;
    }
    Ok(Request {
        version: Version::V5,
        host,
        port,
    })
}

/// SOCKS4 names an IPv4 address; 4a may send a name after the user id,
/// flagged by an address of the form 0.0.0.x.
async fn socks4<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) -> Result<Request, Error> {
    let command = stream.read_u8().await?;
    let port = stream.read_u16().await?;
    let mut address = [0u8; 4];
    stream.read_exact(&mut address).await?;
    let _user = read_until_nul(stream).await?;
    let host = if address[..3] == [0, 0, 0] && address[3] != 0 {
        match String::from_utf8(read_until_nul(stream).await?) {
            Ok(name) if !name.is_empty() => name,
            _ => return refuse(stream, Version::V4, Refusal::General, Error::BadAddress).await,
        }
    } else {
        Ipv4Addr::from(address).to_string()
    };
    if command != 1 {
        return refuse(stream, Version::V4, Refusal::CommandNotSupported, Error::Unsupported).await;
    }
    Ok(Request {
        version: Version::V4,
        host,
        port,
    })
}

/// A string ended by a zero byte, of at most 255 bytes.
async fn read_until_nul<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    loop {
        match stream.read_u8().await? {
            0 => return Ok(bytes),
            _ if bytes.len() >= 255 => return Err(Error::BadAddress),
            byte => bytes.push(byte),
        }
    }
}

async fn refuse<S: AsyncWrite + Unpin>(
    stream: &mut S,
    version: Version,
    refusal: Refusal,
    error: Error,
) -> Result<Request, Error> {
    reply(stream, version, Err(refusal)).await?;
    Err(error)
}

/// Answers the request: the connection is there, or why it is not. The
/// bound address is left empty, as clients do not use it for CONNECT.
pub async fn reply<S: AsyncWrite + Unpin>(stream: &mut S, version: Version, outcome: Result<(), Refusal>) -> io::Result<()> {
    let bytes: &[u8] = match (version, outcome) {
        (Version::V5, Ok(())) => &[5, 0, 0, 1, 0, 0, 0, 0, 0, 0],
        (Version::V5, Err(refusal)) => &[5, refusal as u8, 0, 1, 0, 0, 0, 0, 0, 0],
        (Version::V4, Ok(())) => &[0, 0x5a, 0, 0, 0, 0, 0, 0],
        (Version::V4, Err(_)) => &[0, 0x5b, 0, 0, 0, 0, 0, 0],
    };
    stream.write_all(bytes).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    /// Plays `sent` as the client would and returns what the proxy made of
    /// it, with every byte it answered.
    fn exchange(sent: &[u8]) -> (Result<Request, Error>, Vec<u8>) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (mut client, mut proxy) = duplex(1024);
            client.write_all(sent).await.unwrap();
            // The client has nothing more to say: what is missing never comes.
            client.shutdown().await.unwrap();
            let result = handshake(&mut proxy).await;
            drop(proxy);
            let mut answered = Vec::new();
            client.read_to_end(&mut answered).await.unwrap();
            (result, answered)
        })
    }

    fn request(host: &str, port: u16, version: Version) -> Request {
        Request {
            version,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn socks5_names_are_left_for_the_server_to_resolve() {
        let (result, answered) = exchange(&[5, 2, 0, 2, 5, 1, 0, 3, 3, b'w', b'e', b'b', 0, 80]);
        assert_eq!(result.unwrap(), request("web", 80, Version::V5));
        assert_eq!(answered, [5, 0], "no authentication");
    }

    #[test]
    fn socks5_addresses_are_written_out() {
        let (result, _) = exchange(&[5, 1, 0, 5, 1, 0, 1, 10, 0, 0, 7, 0x1f, 0x90]);
        assert_eq!(result.unwrap(), request("10.0.0.7", 8080, Version::V5));

        let mut ipv6 = vec![5, 1, 0, 5, 1, 0, 4];
        ipv6.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        ipv6.extend_from_slice(&443u16.to_be_bytes());
        let (result, _) = exchange(&ipv6);
        assert_eq!(result.unwrap(), request("::1", 443, Version::V5));
    }

    #[test]
    fn socks5_without_the_no_authentication_method_is_refused() {
        let (result, answered) = exchange(&[5, 1, 2]);
        assert!(matches!(result, Err(Error::NoAcceptableMethod)));
        assert_eq!(answered, [5, 0xff]);
    }

    #[test]
    fn only_connect_is_offered() {
        // BIND to 1.2.3.4:21.
        let (result, answered) = exchange(&[5, 1, 0, 5, 2, 0, 1, 1, 2, 3, 4, 0, 21]);
        assert!(matches!(result, Err(Error::Unsupported)));
        assert_eq!(answered, [5, 0, 5, 7, 0, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn unknown_address_types_are_refused() {
        let (result, answered) = exchange(&[5, 1, 0, 5, 1, 0, 9]);
        assert!(matches!(result, Err(Error::BadAddress)));
        assert_eq!(answered[2..4], [5, 8]);
    }

    #[test]
    fn socks4_and_4a_are_understood() {
        let (result, _) = exchange(&[4, 1, 0, 80, 192, 168, 1, 2, b'm', b'e', 0]);
        assert_eq!(result.unwrap(), request("192.168.1.2", 80, Version::V4));

        let (result, _) = exchange(&[4, 1, 0x1f, 0x90, 0, 0, 0, 1, 0, b'w', b'e', b'b', 0]);
        assert_eq!(result.unwrap(), request("web", 8080, Version::V4));
    }

    #[test]
    fn http_on_the_proxy_port_is_not_socks() {
        let (result, answered) = exchange(b"GET / HTTP/1.1\r\n\r\n");
        assert!(matches!(result, Err(Error::NotSocks)));
        assert!(answered.is_empty());
        assert!(Error::NotSocks.describe().unwrap().contains("SOCKS5"));
    }

    #[test]
    fn a_client_that_hangs_up_is_no_error_to_report() {
        let (result, _) = exchange(&[5, 1]);
        let error = result.unwrap_err();
        assert!(matches!(error, Error::Io));
        assert_eq!(error.describe(), None);
    }

    #[test]
    fn replies_follow_the_version_asked() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let mut written = Vec::new();
            reply(&mut written, Version::V5, Err(Refusal::HostUnreachable)).await.unwrap();
            reply(&mut written, Version::V4, Ok(())).await.unwrap();
            assert_eq!(written, [5, 4, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0x5a, 0, 0, 0, 0, 0, 0]);
        });
    }
}
