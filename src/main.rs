use std::{
    collections::HashSet,
    env,
    io::{Error as IOError, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    process::ExitCode,
    str, thread,
    time::Duration,
};

mod error;
use crate::error::*;

fn main() -> ExitCode {
    match start() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            err(e);
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), AppError> {
    let mut proxy = None;
    let mut listen_port = 3128;
    let mut hosts = HashSet::new();
    let mut debug = false;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        macro_rules! next {
            () => {
                match args.next() {
                    Some(value) => value,
                    None => E!(ArgError::NoValue(arg)),
                }
            };
        }
        macro_rules! parse {
            ($value: expr) => {
                match $value {
                    Ok(value) => value,
                    Err(_) => E!(ArgError::WrongValue(arg)),
                }
            };
        }
        match arg.as_str() {
            "-p" => {
                proxy = parse!(next!().to_socket_addrs()).next();
            }
            "-l" => {
                listen_port = parse!(next!().parse());
            }
            "-h" => {
                hosts = next!().split(',').map(|s| s.trim().to_owned()).collect();
            }
            "-d" => {
                debug = true;
            }
            _ => {
                E!(ArgError::Unknown(arg));
            }
        }
    }

    let Some(proxy) = proxy else {
        E!(AppError::NoProxy);
    };

    if hosts.is_empty() {
        E!(AppError::NoHosts);
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
                    err(e);
                }
                continue;
            }
        };

        thread::spawn(move || {
            if let Err(e) = handle(stream, &proxy, hosts, debug) {
                if debug {
                    err(e);
                }
            }
        });
    }

    Ok(())
}

fn handle(
    client: TcpStream,
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    debug: bool,
) -> Result<(), ConnError> {
    if !check_http_get(&client)? {
        E!(ConnError::NotHttp);
    }

    let addr = resolve_host(&client, proxy, hosts, debug)?;
    transfer_data(client, TcpStream::connect(addr)?)?;
    Ok(())
}

fn check_http_get(client: &TcpStream) -> Result<bool, IOError> {
    let mut head = [0u8; 3];
    client.peek(head.as_mut_slice())?;
    Ok(head == *b"GET")
}

fn resolve_host(
    client: &TcpStream,
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    debug: bool,
) -> Result<SocketAddr, ConnError> {
    let mut buf = [0u8; 1024];

    let chunk = {
        let count = client.peek(buf.as_mut_slice())?;
        str::from_utf8(&buf[..count])?
    };

    const HOST_HEADER: &str = "Host: ";

    let Some(index) = chunk.find(HOST_HEADER) else {
        E!(ConnError::ParseError);
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

        Ok(*proxy)
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

        match addrs?.next() {
            Some(a) => Ok(a),
            None => Err(ConnError::ParseError),
        }
    }
}

fn transfer_data(mut client: TcpStream, mut server: TcpStream) -> Result<(), IOError> {
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
