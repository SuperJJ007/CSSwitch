use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

const CONNECT_DEADLINE: Duration = Duration::from_secs(10);

fn write_status(mut stream: TcpStream, code: u16, reason: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    let _ = stream.flush();
}

pub fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_matches('.').to_ascii_lowercase();
    host == "anthropic.com"
        || host.ends_with(".anthropic.com")
        || host == "claude.ai"
        || host.ends_with(".claude.ai")
        || host == "claude.com"
        || host.ends_with(".claude.com")
}

fn parse_target(target: &str) -> Result<(String, u16), ()> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']').ok_or(())?;
        if host.is_empty() {
            return Err(());
        }
        let port = suffix
            .strip_prefix(':')
            .ok_or(())?
            .parse()
            .map_err(|_| ())?;
        return Ok((host.to_string(), port));
    }
    let (host, port) = target.rsplit_once(':').ok_or(())?;
    if host.is_empty() {
        return Err(());
    }
    Ok((host.to_string(), port.parse().map_err(|_| ())?))
}

fn connect_addrs_until<T, I, N, C>(
    addrs: I,
    deadline: Instant,
    mut now: N,
    mut connect: C,
) -> io::Result<T>
where
    I: IntoIterator<Item = SocketAddr>,
    N: FnMut() -> Instant,
    C: FnMut(&SocketAddr, Duration) -> io::Result<T>,
{
    let mut attempted = false;
    let mut last_error = None;
    for addr in addrs {
        let remaining = deadline
            .checked_duration_since(now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "CONNECT deadline elapsed"))?;
        attempted = true;
        match connect(&addr, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    if !attempted {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "CONNECT target resolved to no addresses",
        ));
    }
    Err(last_error.expect("attempted address must record an error"))
}

fn connect_upstream(host: &str, port: u16) -> io::Result<TcpStream> {
    // std::net does not offer deadline-aware DNS resolution. Start the budget before
    // resolution so a slow lookup cannot also receive a fresh ten-second dial budget;
    // once resolution returns, all candidate addresses share the remaining time.
    let deadline = Instant::now() + CONNECT_DEADLINE;
    let addrs = (host, port).to_socket_addrs()?;
    connect_addrs_until(addrs, deadline, Instant::now, TcpStream::connect_timeout)
}

#[derive(Debug, PartialEq, Eq)]
enum ConnectTargetError {
    BadTarget,
    Blocked,
    DialFailed,
}

fn open_target_with<T, C>(target: &str, mut connect: C) -> Result<T, ConnectTargetError>
where
    C: FnMut(&str, u16) -> io::Result<T>,
{
    let (host, port) = parse_target(target).map_err(|_| ConnectTargetError::BadTarget)?;
    if is_blocked_host(&host) {
        return Err(ConnectTargetError::Blocked);
    }
    connect(&host, port).map_err(|_| ConnectTargetError::DialFailed)
}

pub fn handle_connect(target: &str, mut client: TcpStream) {
    let upstream = match open_target_with(target, connect_upstream) {
        Ok(stream) => stream,
        Err(ConnectTargetError::BadTarget) => {
            write_status(client, 400, "Bad Request");
            return;
        }
        Err(ConnectTargetError::Blocked) => {
            write_status(client, 401, "Unauthorized");
            return;
        }
        Err(ConnectTargetError::DialFailed) => {
            write_status(client, 502, "Bad Gateway");
            return;
        }
    };
    let _ = client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let _ = client.flush();

    let mut client_r = match client.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut upstream_w = match upstream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut upstream_r = upstream;
    let mut client_w = client;

    let to_upstream = thread::spawn(move || {
        let _ = std::io::copy(&mut client_r, &mut upstream_w);
        let _ = upstream_w.shutdown(std::net::Shutdown::Write);
    });
    let to_client = thread::spawn(move || {
        let _ = std::io::copy(&mut upstream_r, &mut client_w);
        let _ = client_w.shutdown(std::net::Shutdown::Write);
    });
    let _ = to_upstream.join();
    let _ = to_client.join();
}

#[cfg(test)]
mod tests {
    use super::{
        connect_addrs_until, is_blocked_host, open_target_with, parse_target, ConnectTargetError,
        CONNECT_DEADLINE,
    };
    use std::io;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{Duration, Instant};

    #[test]
    fn blocks_anthropic_claude_hosts_only() {
        assert!(is_blocked_host("api.anthropic.com"));
        assert!(is_blocked_host("claude.ai"));
        assert!(is_blocked_host("foo.claude.com"));
        assert!(!is_blocked_host("example.com"));
    }

    #[test]
    fn parses_ipv4_hostname_and_ipv6_targets() {
        assert_eq!(
            parse_target("example.com:443"),
            Ok(("example.com".into(), 443))
        );
        assert_eq!(parse_target("127.0.0.1:80"), Ok(("127.0.0.1".into(), 80)));
        assert_eq!(parse_target("[::1]:8443"), Ok(("::1".into(), 8443)));
        assert!(parse_target("example.com").is_err());
        assert!(parse_target(":443").is_err());
        assert!(parse_target("[]:443").is_err());
        assert!(parse_target("example.com:not-a-port").is_err());
    }

    #[test]
    fn blocked_and_bad_targets_never_reach_the_dialer() {
        for (target, expected) in [
            ("api.anthropic.com:443", ConnectTargetError::Blocked),
            ("example.com", ConnectTargetError::BadTarget),
        ] {
            let mut dialed = false;
            let result: Result<(), _> = open_target_with(target, |_host, _port| {
                dialed = true;
                Ok(())
            });
            assert_eq!(result.unwrap_err(), expected);
            assert!(!dialed, "{target} unexpectedly reached the dialer");
        }
    }

    #[test]
    fn all_addresses_share_one_connect_budget() {
        let start = Instant::now();
        let deadline = start + CONNECT_DEADLINE;
        let addrs = [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 2),
        ];
        let mut times = [start, start + Duration::from_secs(6)].into_iter();
        let mut observed = Vec::new();

        let result: io::Result<()> = connect_addrs_until(
            addrs,
            deadline,
            || times.next().expect("one clock reading per address"),
            |addr, timeout| {
                observed.push((*addr, timeout));
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            },
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(observed[0].1, Duration::from_secs(10));
        assert_eq!(observed[1].1, Duration::from_secs(4));
    }

    #[test]
    fn elapsed_deadline_never_attempts_a_dial() {
        let now = Instant::now();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        let mut dialed = false;

        let result: io::Result<()> = connect_addrs_until(
            [addr],
            now,
            || now,
            |_addr, _| {
                dialed = true;
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(!dialed);
    }

    #[test]
    fn empty_resolution_is_an_address_error() {
        let result: io::Result<()> = connect_addrs_until(
            [],
            Instant::now() + CONNECT_DEADLINE,
            Instant::now,
            |_addr, _| Ok(()),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AddrNotAvailable);
    }
}
