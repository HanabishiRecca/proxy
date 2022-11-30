use std::{
    collections::{HashMap, HashSet},
    env,
    io::{ErrorKind::WouldBlock, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    process::ExitCode,
    str,
    sync::Mutex,
    thread,
    time::Duration,
};

mod error;
use error::*;

fn main() -> ExitCode {
    match start() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            err(e);
            ExitCode::FAILURE
        }
    }
}

enum State {
    Send,
    Recv,
    Done,
}

struct Connection {
    client: TcpStream,
    server: Option<TcpStream>,
    state: State,
}

const MAX_WORKER_THREADS: usize = 128;
const WORKER_DELAY: Duration = Duration::from_millis(1);
const TCP_MSS: usize = 1280;
const GET: &[u8] = b"GET";
const HOST_HEADER: &str = "\nHost:";

fn start() -> Result<(), AppError> {
    let mut proxy = None;
    let mut listen_port = 3128;
    let mut hosts = HashSet::new();
    let mut worker_threads = 1;
    let mut debug = false;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        macro_rules! next {
            () => {
                match args.next() {
                    Some(value) => value,
                    _ => E!(ArgError::NoValue(arg)),
                }
            };
        }
        macro_rules! parse {
            ($value: expr) => {
                match $value {
                    Ok(value) => value,
                    _ => E!(ArgError::WrongValue(arg)),
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
            "-t" => {
                worker_threads = match parse!(next!().parse()) {
                    0 => match thread::available_parallelism() {
                        Ok(n) => n.get(),
                        _ => 1,
                    },
                    n => n,
                }
                .min(MAX_WORKER_THREADS);
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

    let hosts = &*Box::leak(Box::new(hosts));

    for host in hosts {
        println!("  {host}");
    }

    let server = TcpListener::bind((Ipv4Addr::UNSPECIFIED, listen_port))?;
    println!();
    println!("Listening port {listen_port}");

    let workers = &*(0..worker_threads)
        .map(|_| Mutex::new(Vec::new()))
        .collect::<Vec<_>>()
        .leak();

    println!("Worker threads: {}", workers.len());
    println!();
    let dns_cache = &*Box::leak(Box::new(Mutex::new(HashMap::<String, SocketAddr>::new())));

    for worker in workers {
        thread::spawn(move || run_worker_thread(worker, proxy, hosts, dns_cache, debug));
    }

    loop {
        for worker in workers {
            let (client, _) = server.accept()?;
            client.set_nonblocking(true)?;
            client.set_nodelay(true)?;

            let Ok(mut connections) = worker.lock() else {
                E!(AppError::Unknown);
            };

            connections.push(Connection {
                client,
                server: None,
                state: State::Send,
            });
        }
    }
}

fn run_worker_thread(
    worker: &Mutex<Vec<Connection>>,
    proxy: SocketAddr,
    hosts: &HashSet<String>,
    dns_cache: &Mutex<HashMap<String, SocketAddr>>,
    debug: bool,
) {
    let mut buf = [0u8; TCP_MSS];

    loop {
        thread::sleep(WORKER_DELAY);

        let Ok(mut connections) = worker.try_lock() else {
            continue;
        };

        if connections.len() == 0 {
            continue;
        }

        handle_connections(&mut connections, &mut buf, &proxy, hosts, dns_cache, debug);
    }
}

fn handle_connections(
    connections: &mut Vec<Connection>,
    buf: &mut [u8],
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    dns_cache: &Mutex<HashMap<String, SocketAddr>>,
    debug: bool,
) {
    let mut index = 0;

    while index < connections.len() {
        let done = {
            let Some(connection) = connections.get_mut(index) else {
                index += 1;
                continue;
            };

            match progress(connection, buf, proxy, hosts, dns_cache, debug) {
                Ok(d) => d,
                Err(e) => {
                    err(e);
                    true
                }
            }
        };

        if !done {
            index += 1;
            continue;
        }

        let Some(last) = connections.pop() else {
            index += 1;
            continue;
        };

        if index >= connections.len() {
            break;
        }

        connections[index] = last;
        index += 1;
    }
}

fn progress(
    connection: &mut Connection,
    buf: &mut [u8],
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    dns_cache: &Mutex<HashMap<String, SocketAddr>>,
    debug: bool,
) -> Result<bool, ConnError> {
    use State::*;

    while match connection.state {
        Send => send(connection, buf, proxy, hosts, dns_cache, debug)?,
        Recv => recv(connection, buf)?,
        Done => return Ok(true),
    } {}

    Ok(false)
}

fn init(
    buf: &[u8],
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    dns_cache: &Mutex<HashMap<String, SocketAddr>>,
    debug: bool,
) -> Result<TcpStream, ConnError> {
    if !check_http(buf) {
        E!(ConnError::NotHttp);
    }

    let server = TcpStream::connect(resolve(buf, proxy, hosts, dns_cache, debug)?)?;
    server.set_nonblocking(true)?;
    server.set_nodelay(true)?;
    Ok(server)
}

fn check_http(buf: &[u8]) -> bool {
    (buf.len() > GET.len()) && (&buf[..GET.len()] == GET)
}

fn resolve(
    buf: &[u8],
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    dns_cache: &Mutex<HashMap<String, SocketAddr>>,
    debug: bool,
) -> Result<SocketAddr, ConnError> {
    let addr = {
        let content = str::from_utf8(buf)?;

        let s = match content.find(HOST_HEADER) {
            Some(pos) => &content[(pos + HOST_HEADER.len())..],
            _ => E!(ConnError::ParseError),
        };

        match s.find('\n') {
            Some(pos) => &s[..pos],
            _ => E!(ConnError::ParseError),
        }
        .trim()
    };

    if hosts.contains(addr) {
        if debug {
            println!("{addr} => PROXY");
        }
        return Ok(*proxy);
    }

    if debug {
        println!("{addr} => DIRECT");
    }

    macro_rules! L {
        () => {
            match dns_cache.lock() {
                Ok(v) => v,
                _ => E!(ConnError::Unknown),
            }
        };
    }

    if let Some(cached) = L!().get(addr) {
        return Ok(*cached);
    }

    if let Ok(mut addrs) = {
        let v6 = addr.starts_with('[');
        if (v6 && addr.contains("]:")) || (!v6 && addr.contains(':')) {
            addr.to_socket_addrs()
        } else {
            (addr, 80).to_socket_addrs()
        }
    } {
        if let Some(a) = addrs.next() {
            L!().insert(addr.to_owned(), a);
            return Ok(a);
        }
    }

    E!(ConnError::ParseError)
}

fn send(
    connection: &mut Connection,
    buf: &mut [u8],
    proxy: &SocketAddr,
    hosts: &HashSet<String>,
    dns_cache: &Mutex<HashMap<String, SocketAddr>>,
    debug: bool,
) -> Result<bool, ConnError> {
    let Ok(count) = connection.client.read(buf) else {
        return Ok(false);
    };

    connection
        .server
        .get_or_insert(init(&buf[..count], proxy, hosts, dns_cache, debug)?)
        .write_all(&buf[..count])?;

    if count < buf.len() {
        connection.state = State::Recv;
        connection.client.set_read_timeout(Some(WORKER_DELAY))?;
    }

    Ok(true)
}

fn recv(connection: &mut Connection, buf: &mut [u8]) -> Result<bool, ConnError> {
    let Some(server) = &mut connection.server else {
        E!(ConnError::Unknown);
    };

    if match connection.client.peek([0u8; 1].as_mut_slice()) {
        Ok(c) => c == 0,
        Err(e) => !matches!(e.kind(), WouldBlock),
    } {
        connection.state = State::Done;
        return Ok(true);
    }

    let Ok(count) = server.read(buf) else {
        return Ok(false);
    };

    if count == 0 {
        connection.state = State::Done;
        return Ok(true);
    }

    connection.client.write_all(&buf[..count])?;
    Ok(true)
}
