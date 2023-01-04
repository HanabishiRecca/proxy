use std::{env, net::ToSocketAddrs, process::ExitCode};

mod error;
use error::*;
mod app;
use app::*;

fn main() -> ExitCode {
    match start() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            err(e);
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), MainError> {
    let mut proxy = None;
    let mut listen_port = 3128;
    let mut hosts = Hosts::new();
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
                hosts.extend(next!().split(',').map(|s| s.trim().to_owned()));
            }
            "-t" => {
                worker_threads = parse!(next!().parse());
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
        E!(ArgError::NoProxy);
    };

    if hosts.is_empty() {
        E!(ArgError::NoHosts);
    }

    App::start(proxy, hosts, listen_port, worker_threads, debug)?;
    Ok(())
}
