use std::{
    collections::HashSet,
    env,
    error::Error,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    str, thread,
    time::Duration,
};

type R<T> = Result<T, Box<dyn Error>>;

fn main() -> R<()> {
    let mut proxy = None;
    let mut listen_port = 3128;
    let mut hosts = HashSet::new();
    let mut debug = false;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-p" => {
                if let Some(value) = args.next() {
                    proxy = value.to_socket_addrs()?.next();
                }
            }
            "-l" => {
                if let Some(value) = args.next() {
                    listen_port = value.parse()?;
                }
            }
            "-h" => {
                if let Some(value) = args.next() {
                    hosts = value.split(',').map(|s| s.trim().to_owned()).collect();
                }
            }
            "-d" => {
                debug = true;
            }
            _ => {
                eprintln!("Unknown option '{arg}'.");
                return Ok(());
            }
        }
    }

    let Some(proxy) = proxy else {
        eprintln!("Error: proxy server not specified.");
        return Ok(());
    };

    if hosts.is_empty() {
        eprintln!("Error: target hosts not specified.");
        return Ok(());
    }

    println!("Proxy {proxy}");
    println!();
    println!("Hosts:");

    for host in &hosts {
        println!("  {host}");
    }

    let server = TcpListener::bind((Ipv4Addr::UNSPECIFIED, listen_port))?;
    println!();
    println!("Listening port {listen_port}");
    println!();

    let hosts = &*Box::leak(Box::new(hosts));

    for stream in server.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                if debug {
                    eprintln!("Error: {e}");
                }
                continue;
            }
        };

        thread::spawn(move || match handle(stream, &proxy, hosts, debug) {
            Ok(_) => {}
            Err(e) => {
                if debug {
                    eprintln!("Error: {e}")
                }
            }
        });
    }

    Ok(())
}

fn handle(client: TcpStream, proxy: &SocketAddr, hosts: &HashSet<String>, debug: bool) -> R<()> {
    if !check_http_get(&client)? {
        if debug {
            eprintln!("Error: not HTTP GET.");
        }
        return Ok(());
    }

    let Some(addr) = resolve_host(&client, proxy, hosts, debug)? else {
        if debug {
            eprintln!("Error: no host header.");
        }
        return Ok(());
    };

    transfer_data(client, TcpStream::connect(addr)?)
}

fn check_http_get(client: &TcpStream) -> R<bool> {
    let mut head = [0u8; 3];
    client.peek(head.as_mut_slice())?;
    Ok(head == *b"GET")
}

fn resolve_host(
    client: &TcpStream,
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    debug: bool,
) -> R<Option<SocketAddr>> {
    let mut buf = [0u8; 1024];

    let chunk = {
        let count = client.peek(buf.as_mut_slice())?;
        str::from_utf8(&buf[..count])?
    };

    const HOST_HEADER: &str = "Host: ";

    let Some(index) = chunk.find(HOST_HEADER) else {
        return Ok(None);
    };

    let chunk = &chunk[index + HOST_HEADER.len()..];

    let addr = match chunk.find('\n') {
        Some(end) => &chunk[..end],
        None => chunk,
    }
    .trim();

    if hosts.contains(addr) {
        if debug {
            println!("{addr} => PROXY");
        }

        Ok(Some(*proxy))
    } else {
        if debug {
            println!("{addr} => DIRECT");
        }

        let v6 = addr.starts_with('[');

        let addrs = if (v6 && addr.contains("]:")) || (!v6 && addr.contains(':')) {
            addr.to_socket_addrs()
        } else {
            (addr, 80).to_socket_addrs()
        };

        Ok(addrs?.next())
    }
}

fn transfer_data(mut client: TcpStream, mut server: TcpStream) -> R<()> {
    client.set_read_timeout(Some(Duration::from_millis(1)))?;
    let mut buf = [0u8; 1024];

    loop {
        let count = client.read(buf.as_mut_slice())?;
        server.write_all(&buf[..count])?;

        if count < buf.len() {
            break;
        }
    }

    loop {
        let count = server.read(buf.as_mut_slice())?;

        if count == 0 {
            break;
        }

        client.write_all(&buf[..count])?;

        if count < buf.len() && client.read([0u8; 1].as_mut_slice()).is_ok() {
            break;
        }
    }

    Ok(())
}
