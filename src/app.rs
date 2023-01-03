use std::{
    collections::{HashMap, HashSet},
    io::{ErrorKind, Read, Write},
    mem::MaybeUninit,
    net::{Ipv4Addr, SocketAddr, TcpListener, ToSocketAddrs},
    str,
    sync::{
        mpsc::{self, Receiver, RecvError, Sender},
        Mutex, MutexGuard,
    },
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

    pub fn start(&self, port: u16, mut worker_threads: usize) -> Result<(), AppError> {
        worker_threads = match worker_threads {
            0 => match thread::available_parallelism() {
                Ok(n) => n.get(),
                _ => 1,
            },
            n => n,
        }
        .min(MAX_WORKER_THREADS);

        let server = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))?;

        if self.debug {
            println!("Proxy: {}", self.proxy);
            println!();
            println!("Hosts:");

            for host in &self.hosts {
                println!("  {host}");
            }

            println!();
            println!("Worker threads: {worker_threads}");
            println!("Listen port: {port}");
            println!();
        }

        thread::scope(|scope| {
            let senders = (0..worker_threads)
                .map(|_| {
                    let (sender, receiver) = mpsc::channel();
                    scope.spawn(move || Worker::new(self, receiver).run());
                    sender
                })
                .collect::<Vec<_>>();

            println!("Proxy is running.");
            println!();

            loop {
                for sender in &senders {
                    self.accept(&server, sender)?;
                }
            }
        })
    }

    fn accept<'a>(
        &'a self,
        server: &TcpListener,
        sender: &Sender<Connection<'a>>,
    ) -> Result<(), AppError> {
        let (client, _) = server.accept()?;
        client.set_nonblocking(true)?;
        client.set_nodelay(true)?;

        let connection = Connection {
            app: self,
            client: TcpStream::from_std(client),
            server: None,
            state: State::Init,
        };

        sender.send(connection).map_err(|_| AppError::Unknown)
    }
}

struct Worker<'a> {
    app: &'a App,
    receiver: Receiver<Connection<'a>>,
    connections: Vec<Connection<'a>>,
}

impl<'a> Worker<'a> {
    pub fn new(app: &'a App, receiver: Receiver<Connection<'a>>) -> Self {
        Worker {
            app,
            receiver,
            connections: Vec::new(),
        }
    }

    pub fn run(&mut self) {
        loop {
            thread::sleep(WORKER_DELAY);

            if let Err(e) = self.handle_connections() {
                err(e);
                return;
            }
        }
    }

    fn handle_connections(&mut self) -> Result<(), RecvError> {
        if self.connections.is_empty() {
            self.connections.push(self.receiver.recv()?);
        } else {
            while let Ok(connection) = self.receiver.try_recv() {
                self.connections.push(connection);
            }
        }

        let mut index = 0;

        while index < self.connections.len() {
            let done = {
                let Some(connection) = self.connections.get_mut(index) else {
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

            let Some(last) = self.connections.pop() else {
                break;
            };

            if index >= self.connections.len() {
                break;
            }

            self.connections[index] = last;
        }

        Ok(())
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
