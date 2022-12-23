use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Read, Write},
    mem::MaybeUninit,
    net::{Ipv4Addr, SocketAddr, TcpListener, ToSocketAddrs},
    str,
    sync::{Mutex, MutexGuard},
    thread,
    time::Duration,
};

use mio::net::TcpStream;

use crate::{error::*, E};

const MAX_WORKER_THREADS: usize = 128;
const WORKER_DELAY: Duration = Duration::from_millis(1);
const BUFFER_SIZE: usize = 4096;
const GET: &[u8] = b"GET";
const HOST_HEADER: &str = "host:";

pub type Hosts = HashSet<String>;

pub struct App {
    proxy: SocketAddr,
    hosts: Hosts,
    debug: bool,
    dns: Dns,
}

impl App {
    pub fn new(proxy: SocketAddr, hosts: Hosts, debug: bool) -> Self {
        App {
            proxy,
            hosts,
            debug,
            dns: Dns::new(),
        }
    }

    pub fn start(&self, port: u16, worker_threads: usize) -> Result<(), AppError> {
        println!("Proxy: {}", self.proxy);
        println!();
        println!("Hosts:");

        for host in &self.hosts {
            println!("  {host}");
        }

        println!();
        let server = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;
        println!("Listen port: {port}");

        let workers = {
            let count = match worker_threads {
                0 => match thread::available_parallelism() {
                    Ok(n) => n.get(),
                    _ => 1,
                },
                n => n,
            }
            .min(MAX_WORKER_THREADS);

            println!("Worker threads: {count}");
            (0..count).map(|_| Worker::new(self)).collect::<Vec<_>>()
        };

        thread::scope(|scope| {
            for worker in &workers {
                scope.spawn(move || worker.run());
            }

            loop {
                for worker in &workers {
                    Self::accept(&server, worker)?;
                }
            }
        })
    }

    fn accept(server: &TcpListener, worker: &Worker) -> Result<(), AppError> {
        let (client, _) = server.accept()?;
        client.set_nonblocking(true)?;
        client.set_nodelay(true)?;

        let Ok(mut connections) = worker.connections.lock() else {
            E!(AppError::Unknown);
        };

        connections.push(Connection {
            app: worker.app,
            client: TcpStream::from_std(client),
            server: None,
            state: State::Init,
        });

        Ok(())
    }
}

struct Worker<'a> {
    app: &'a App,
    connections: Mutex<Vec<Connection<'a>>>,
}

impl<'a> Worker<'a> {
    pub fn new(app: &'a App) -> Self {
        Worker {
            app,
            connections: Mutex::new(Vec::new()),
        }
    }

    pub fn run(&self) {
        loop {
            thread::sleep(WORKER_DELAY);
            self.handle_connections();
        }
    }

    fn handle_connections(&self) {
        let Ok(mut connections) = self.connections.try_lock() else {
            return;
        };

        if connections.len() == 0 {
            return;
        }

        let mut index = 0;

        while index < connections.len() {
            let done = {
                let Some(connection) = connections.get_mut(index) else {
                    break;
                };

                match connection.progress() {
                    Ok(d) => d,
                    Err(e) => {
                        if self.app.debug {
                            err(e);
                        }
                        true
                    }
                }
            };

            if !done {
                index += 1;
                continue;
            }

            let Some(last) = connections.pop() else {
                break;
            };

            if index >= connections.len() {
                break;
            }

            connections[index] = last;
        }
    }
}

struct Connection<'a> {
    app: &'a App,
    client: TcpStream,
    server: Option<TcpStream>,
    state: State,
}

enum State {
    Init,
    Conn,
    Send,
    Recv,
    Done,
}

impl<'a> Connection<'a> {
    fn progress(&mut self) -> Result<bool, ConnError> {
        use State::*;

        while match self.state {
            Init => self.init()?,
            Conn => self.connect()?,
            Send => self.send()?,
            Recv => self.recv()?,
            Done => return Ok(true),
        } {}

        Ok(false)
    }

    fn init(&mut self) -> Result<bool, ConnError> {
        let mut buf = uninit_buffer::<BUFFER_SIZE>();

        let Ok(count) = self.client.peek(buf.as_mut_slice()) else {
            return Ok(false);
        };

        let data = &mut buf[..count];

        if !Self::check_http(data) {
            E!(ConnError::NotHttp);
        }

        let server = TcpStream::connect(self.resolve(data)?)?;
        self.server = Some(server);
        self.state = State::Conn;
        Ok(true)
    }

    fn check_http(data: &[u8]) -> bool {
        (data.len() > GET.len()) && (&data[..GET.len()] == GET)
    }

    fn resolve(&self, data: &mut [u8]) -> Result<SocketAddr, ConnError> {
        let host = {
            let content = str::from_utf8_mut(data)?;
            content.make_ascii_lowercase();

            let line = content
                .lines()
                .map(|s| s.trim_start())
                .find(|s| s.starts_with(HOST_HEADER));

            match line {
                Some(s) => s[HOST_HEADER.len()..].trim(),
                _ => E!(ConnError::ParseError),
            }
        };

        if self.app.hosts.contains(host) {
            if self.app.debug {
                println!("{host} => PROXY");
            }
            return Ok(self.app.proxy);
        }

        if self.app.debug {
            println!("{host} => DIRECT");
        }

        self.app.dns.resolve(host)
    }

    fn connect(&mut self) -> Result<bool, ConnError> {
        let Some(server) = &self.server else {
            E!(ConnError::Unknown);
        };

        if let Err(e) = server.peer_addr() {
            if matches!(e.kind(), ErrorKind::NotConnected | ErrorKind::WouldBlock) {
                return Ok(false);
            }
            E!(e);
        }

        if let Err(e) = server.set_nodelay(true) {
            if e.kind() == ErrorKind::InvalidInput {
                return Ok(false);
            }
            E!(e);
        }

        self.state = State::Send;
        Ok(true)
    }

    fn send(&mut self) -> Result<bool, ConnError> {
        let mut buf = uninit_buffer::<BUFFER_SIZE>();

        let Ok(count) = self.client.read(buf.as_mut_slice()) else {
            return Ok(false);
        };

        if count == 0 {
            self.state = State::Recv;
            return Ok(true);
        }

        let Some(server) = &mut self.server else {
            E!(ConnError::Unknown);
        };

        server.write_all(&buf[..count])?;

        if count < buf.len() {
            self.state = State::Recv;
        }

        Ok(true)
    }

    fn recv(&mut self) -> Result<bool, ConnError> {
        let Some(server) = &mut self.server else {
            E!(ConnError::Unknown);
        };

        if match self.client.peek(uninit_buffer::<1>().as_mut_slice()) {
            Ok(c) => c == 0,
            Err(e) => e.kind() != ErrorKind::WouldBlock,
        } {
            self.state = State::Done;
            return Ok(true);
        }

        let mut buf = uninit_buffer::<BUFFER_SIZE>();

        let Ok(count) = server.read(buf.as_mut_slice()) else {
            return Ok(false);
        };

        if count == 0 {
            self.state = State::Done;
            return Ok(true);
        }

        self.client.write_all(&buf[..count])?;
        Ok(true)
    }
}

fn uninit_buffer<const N: usize>() -> [u8; N] {
    // SAFETY: uninitialized byte buffer should be fine
    #[allow(clippy::uninit_assumed_init)]
    unsafe {
        MaybeUninit::uninit().assume_init()
    }
}

type DnsCache = HashMap<String, SocketAddr>;

struct Dns {
    cache: Mutex<DnsCache>,
}

impl Dns {
    pub fn new() -> Self {
        Dns {
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn resolve(&self, host: &str) -> Result<SocketAddr, ConnError> {
        if let Some(cached) = self.cache()?.get(host) {
            return Ok(*cached);
        }

        let Ok(mut resolved) = ({
            let v6 = host.starts_with('[');
            if (v6 && host.contains("]:")) || (!v6 && host.contains(':')) {
                host.to_socket_addrs()
            } else {
                (host, 80).to_socket_addrs()
            }
        }) else {
            E!(ConnError::DnsError);
        };

        if let Some(addr) = resolved.next() {
            self.cache()?.insert(host.to_owned(), addr);
            return Ok(addr);
        }

        E!(ConnError::DnsError);
    }

    fn cache(&self) -> Result<MutexGuard<DnsCache>, ConnError> {
        match self.cache.lock() {
            Ok(c) => Ok(c),
            _ => E!(ConnError::Unknown),
        }
    }
}
