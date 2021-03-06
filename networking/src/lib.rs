#![feature(duration)]
#![feature(socket_timeout)]
#![feature(tcp)]
#[cfg(unix)]extern crate libc;
#[cfg(unix)]extern crate unix_socket;

extern crate config;
extern crate util;
extern crate parser;
extern crate response;
extern crate database;
extern crate command;

use std::collections::HashMap;
use std::time::Duration;
use std::io;
use std::io::{Read, Write};
use std::net::{ToSocketAddrs, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{Sender, channel};
use std::thread;

#[cfg(unix)] use std::path::Path;
#[cfg(unix)] use std::fs::File;

#[cfg(unix)] use libc::funcs::posix88::unistd::fork;
#[cfg(unix)] use libc::funcs::c95::stdlib::exit;
#[cfg(unix)] use libc::funcs::posix88::unistd::getpid;
#[cfg(unix)] use unix_socket::{UnixStream, UnixListener};

use config::Config;
use database::{Database, PubsubEvent};
use response::{Response, ResponseError};
use command::command;
use parser::parse;
use parser::ParseError;

enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Stream {
    fn try_clone(&self) -> io::Result<Stream> {
        match *self {
            Stream::Tcp(ref s) => Ok(Stream::Tcp(try!(s.try_clone()))),
            Stream::Unix(ref s) => Ok(Stream::Unix(try!(s.try_clone()))),
        }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            Stream::Tcp(ref mut s) => s.write(buf),
            Stream::Unix(ref mut s) => s.write(buf),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            Stream::Tcp(ref mut s) => s.read(buf),
            Stream::Unix(ref mut s) => s.read(buf),
        }
    }

    fn set_keepalive(&self, seconds: Option<u32>) -> io::Result<()> {
        match *self {
            Stream::Tcp(ref s) => s.set_keepalive(seconds),
            Stream::Unix(_) => Ok(()),
        }
    }

    fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match *self {
            Stream::Tcp(ref s) => s.set_write_timeout(dur),
            // TODO: couldn't figure out how to enable this in unix_socket
            Stream::Unix(_) => Ok(()),
        }
    }

    fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        match *self {
            Stream::Tcp(ref s) => s.set_read_timeout(dur),
            // TODO: couldn't figure out how to enable this in unix_socket
            Stream::Unix(_) => Ok(()),
        }
    }
}

struct Client {
    stream: Stream,
    db: Arc<Mutex<Database>>
}

pub struct Server {
    config: Config,
    db: Arc<Mutex<Database>>,
    listener_channels: Vec<Sender<u8>>,
    listener_threads: Vec<thread::JoinHandle<()>>,
}

impl Client {
    pub fn tcp(stream: TcpStream, db: Arc<Mutex<Database>>) -> Client {
        return Client {
            stream: Stream::Tcp(stream),
            db: db,
        }
    }

    pub fn unix(stream: UnixStream, db: Arc<Mutex<Database>>) -> Client {
        return Client {
            stream: Stream::Unix(stream),
            db: db,
        }
    }

    pub fn run(&mut self) {
        #![allow(unused_must_use)]
        let (stream_tx, rx) = channel::<Option<Response>>();
        {
            let mut stream = self.stream.try_clone().unwrap();
            thread::spawn(move || {
                loop {
                    match rx.recv() {
                        Ok(m) => match m {
                            Some(msg) => stream.write(&*msg.as_bytes()),
                            None => break,
                        },
                        Err(_) => break,
                    };
                }
            });
        }
        let (pubsub_tx, pubsub_rx) = channel::<Option<PubsubEvent>>();
        {
            let tx = stream_tx.clone();
            thread::spawn(move || {
                loop {
                    match pubsub_rx.recv() {
                        Ok(m) => match m {
                            Some(msg) => tx.send(Some(msg.as_response())),
                            None => break,
                        },
                        Err(_) => break,
                    };
                }
                tx.send(None);
            });
        }
        let mut buffer = [0u8; 512];
        let mut dbindex = 0;
        let mut subscriptions = HashMap::new();
        let mut psubscriptions = HashMap::new();
        loop {
            let len = match self.stream.read(&mut buffer) {
                Ok(r) => r,
                Err(_) => break,
            };
            if len == 0 {
                break;
            }
            let parser = match parse(&buffer, len) {
                Ok(p) => p,
                Err(err) => match err {
                    ParseError::Incomplete => { continue; }
                    _ => { break; }
                },
            };

            let mut error = false;
            loop {
                let mut db = match self.db.lock() {
                    Ok(db) => db,
                    Err(_) => break,
                };
                match command(&parser, &mut *db, &mut dbindex, Some(&mut subscriptions), Some(&mut psubscriptions), Some(&pubsub_tx)) {
                    Ok(response) => {
                        match stream_tx.send(Some(response)) {
                            Ok(_) => (),
                            Err(_) => error = true,
                        };
                        break;
                    },
                    Err(err) => match err {
                        ResponseError::NoReply => (),
                        // Repeating the same command is actually wrong because of the timeout
                        ResponseError::Wait(ref receiver) => {
                            drop(db);
                            if !receiver.recv().unwrap() {
                                match stream_tx.send(Some(Response::Nil)) {
                                    Ok(_) => (),
                                    Err(_) => error = true,
                                };
                            }
                        }
                    },
                }
            }
            if error {
                stream_tx.send(None);
                pubsub_tx.send(None);
                break;
            }
        };
    }
}

macro_rules! handle_listener {
    ($listener: expr, $server: expr, $rx: expr, $tcp_keepalive: expr, $timeout: expr, $t: ident) => ({
        let db = $server.db.clone();
        thread::spawn(move || {
            for stream in $listener.incoming() {
                if $rx.try_recv().is_ok() {
                    // any new message should break
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let db1 = db.clone();
                        thread::spawn(move || {
                            let mut client = Client::$t(stream, db1);
                            client.stream.set_keepalive(if $tcp_keepalive > 0 { Some($tcp_keepalive) } else { None }).unwrap();
                            client.stream.set_read_timeout(if $timeout > 0 { Some(Duration::new($timeout, 0)) } else { None }).unwrap();
                            client.stream.set_write_timeout(if $timeout > 0 { Some(Duration::new($timeout, 0)) } else { None }).unwrap();
                            client.run();
                        });
                    }
                    Err(e) => { println!("error {}", e); }
                }
            }
        })
    })
}
impl Server {
    pub fn new(config: Config) -> Server {
        let db = Database::new(&config);
        return Server {
            config: config,
            db: Arc::new(Mutex::new(db)),
            listener_channels: Vec::new(),
            listener_threads: Vec::new(),
        }
    }

    #[cfg(unix)]
    pub fn run(&mut self) {
        if self.config.daemonize {
            unsafe {
                match fork() {
                    -1 => panic!("Fork failed"),
                    0 => {
                        if let Ok(mut fp) = File::create(Path::new(&*self.config.pidfile)) {
                            match write!(fp, "{}", getpid()) {
                                // TODO warn on error?
                                _ => (),
                            }
                        }
                        self.start();
                        self.join();
                    },
                    _ => exit(0),
                };
            }
        } else {
            self.start();
            self.join();
        }
    }

    #[cfg(not(unix))]
    pub fn run(&mut self) {
        if self.config.daemonize {
            panic!("Cannot daemonize in non-unix");
        } else {
            self.start();
            self.join();
        }
    }

    pub fn join(&mut self) {
        #![allow(unused_must_use)]
        while self.listener_threads.len() > 0 {
            self.listener_threads.pop().unwrap().join();
        }
    }

    pub fn start(&mut self) {
        let tcp_keepalive = self.config.tcp_keepalive;
        let timeout = self.config.timeout;
        for addr in self.config.addresses() {
            let (tx, rx) = channel();
            self.listener_channels.push(tx);
            let listener = TcpListener::bind(addr).unwrap();
            let th = handle_listener!(listener, self, rx, tcp_keepalive, timeout, tcp);
            self.listener_threads.push(th);
        }
        self.handle_unixsocket();
    }

    #[cfg(unix)]
    fn handle_unixsocket(&mut self) {
        if let Some(ref unixsocket) = self.config.unixsocket {
            let tcp_keepalive = self.config.tcp_keepalive;
            let timeout = self.config.timeout;

            let (tx, rx) = channel();
            self.listener_channels.push(tx);
            let listener = UnixListener::bind(&*unixsocket).unwrap();
            let th = handle_listener!(listener, self, rx, tcp_keepalive, timeout, unix);
            self.listener_threads.push(th);
        }
    }

    #[cfg(not(unix))]
    fn handle_unixsocket() {
        if self.config.unixsocket.is_some() {
            writeln!(&mut std::io::stderr(), "Ignoring unixsocket in non unix environment\n");
        }
    }

    pub fn stop(&mut self) {
        #![allow(unused_must_use)]
        for sender in self.listener_channels.iter() {
            sender.send(0);
            for addr in self.config.addresses() {
                for addrs in addr.to_socket_addrs().unwrap() {
                    TcpStream::connect(addrs);
                }
            }
        }
        self.join();
    }
}

#[cfg(test)]
mod test_networking {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::str::from_utf8;

    use config::Config;

    use super::Server;

    #[test]
    fn parse_ping() {
        let port = 6379;

        let mut server = Server::new(Config::mock(port));
        server.start();

        let addr = format!("127.0.0.1:{}", port);
        let streamres = TcpStream::connect(&*addr);
        assert!(streamres.is_ok());
        let mut stream = streamres.unwrap();
        let message = b"*2\r\n$4\r\nping\r\n$4\r\npong\r\n";
        assert!(stream.write(message).is_ok());
        let mut h = [0u8; 4];
        assert!(stream.read(&mut h).is_ok());
        assert_eq!(from_utf8(&h).unwrap(), "$4\r\n");
        let mut c = [0u8; 6];
        assert!(stream.read(&mut c).is_ok());
        assert_eq!(from_utf8(&c).unwrap(), "pong\r\n");
        server.stop();
    }

    #[test]
    fn allow_multiwrite() {
        let port = 6380;
        let mut server = Server::new(Config::mock(port));
        server.start();

        let addr = format!("127.0.0.1:{}", port);
        let streamres = TcpStream::connect(&*addr);
        assert!(streamres.is_ok());
        let mut stream = streamres.unwrap();
        let message = b"*2\r\n$4\r\nping\r\n";
        assert!(stream.write(message).is_ok());
        let message = b"$4\r\npong\r\n";
        assert!(stream.write(message).is_ok());
        let mut h = [0u8; 4];
        assert!(stream.read(&mut h).is_ok());
        assert_eq!(from_utf8(&h).unwrap(), "$4\r\n");
        let mut c = [0u8; 6];
        assert!(stream.read(&mut c).is_ok());
        assert_eq!(from_utf8(&c).unwrap(), "pong\r\n");
        server.stop();
    }

    #[test]
    fn allow_stop() {
        let port = 6381;
        let mut server = Server::new(Config::mock(port));
        server.start();
        {
            let addr = format!("127.0.0.1:{}", port);
            let streamres = TcpStream::connect(&*addr);
            assert!(streamres.is_ok());
        }
        server.stop();

        {
            let addr = format!("127.0.0.1:{}", port);
            let streamres = TcpStream::connect(&*addr);
            assert!(streamres.is_err());
        }

        server.start();
        {
            let addr = format!("127.0.0.1:{}", port);
            let streamres = TcpStream::connect(&*addr);
            assert!(streamres.is_ok());
        }
        server.stop();
    }
}
