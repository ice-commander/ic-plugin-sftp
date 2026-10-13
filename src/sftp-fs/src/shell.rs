use crate::ssh::Config;
use crate::tunnel::Endpoint;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

const READ_BUF: usize = 8192;
const IDLE_SLEEP: Duration = Duration::from_millis(10);
const WRITE_RETRY: Duration = Duration::from_millis(5);

pub struct Shell {
    input: mpsc::Sender<Vec<u8>>,
    output: mpsc::Receiver<Vec<u8>>,
    resize: mpsc::Sender<(u16, u16)>,
    held: Vec<u8>,
    ended: bool,
}

pub fn starts_in(cwd: &str) -> &str {
    if cwd.is_empty() {
        "/"
    } else {
        cwd
    }
}

fn write_all(channel: &mut ssh2::Channel, data: &[u8]) -> bool {
    let mut sent = 0;
    while sent < data.len() {
        match channel.write(&data[sent..]) {
            Ok(0) => return false,
            Ok(n) => sent += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(WRITE_RETRY)
            }
            Err(_) => return false,
        }
    }
    true
}

fn run(
    cfg: &Config,
    cwd: &str,
    rows: u16,
    cols: u16,
    input_rx: mpsc::Receiver<Vec<u8>>,
    output_tx: &mpsc::Sender<Vec<u8>>,
    resize_rx: mpsc::Receiver<(u16, u16)>,
) -> Result<(), String> {
    let address = format!("{}:{}", cfg.host, cfg.port);
    let tcp = TcpStream::connect(&address).map_err(|e| format!("cannot reach {address}: {e}"))?;
    let mut session = ssh2::Session::new().map_err(|e| e.to_string())?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|e| format!("ssh handshake failed: {e}"))?;
    crate::ssh::authenticate(&session, cfg)?;

    let mut channel = session
        .channel_session()
        .map_err(|e| format!("cannot open channel: {e}"))?;
    channel
        .request_pty(
            "xterm-256color",
            None,
            Some((cols.max(1) as u32, rows.max(1) as u32, 0, 0)),
        )
        .map_err(|e| format!("cannot request a terminal: {e}"))?;
    channel
        .shell()
        .map_err(|e| format!("cannot start a shell: {e}"))?;

    if !cwd.is_empty() {
        let quoted = cwd.replace('\'', r"'\''");
        let _ = write_all(&mut channel, format!("cd '{quoted}'\n").as_bytes());
    }

    session.set_blocking(false);
    let mut buf = [0u8; READ_BUF];
    loop {
        let mut idle = true;
        match channel.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                idle = false;
                if output_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        let mut done = false;
        loop {
            match input_rx.try_recv() {
                Ok(data) => {
                    idle = false;
                    if !write_all(&mut channel, &data) {
                        done = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        while let Ok((r, c)) = resize_rx.try_recv() {
            idle = false;
            let _ = channel.request_pty_size(c.max(1) as u32, r.max(1) as u32, None, None);
        }
        if done || channel.eof() {
            break;
        }
        if idle {
            std::thread::sleep(IDLE_SLEEP);
        }
    }

    session.set_blocking(true);
    let _ = channel.close();
    let _ = channel.wait_close();
    Ok(())
}

impl Shell {
    pub fn open(cfg: &Config, endpoint: Endpoint, cwd: &str, rows: u16, cols: u16) -> Shell {
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();

        let mut carried = cfg.clone();
        let where_from = cwd.to_string();
        std::thread::spawn(move || {
            let outcome = match endpoint.resolve() {
                Ok((host, port)) => {
                    // The endpoint holds the tunnel slot; keeping it would let the tunnel outlive its mount.
                    drop(endpoint);
                    carried.host = host;
                    carried.port = port;
                    run(
                        &carried,
                        &where_from,
                        rows,
                        cols,
                        input_rx,
                        &output_tx,
                        resize_rx,
                    )
                }
                Err(reason) => Err(reason),
            };
            if let Err(reason) = outcome {
                let _ = output_tx.send(format!("\r\n{reason}\r\n").into_bytes());
            }
        });

        Shell {
            input: input_tx,
            output: output_rx,
            resize: resize_tx,
            held: Vec::new(),
            ended: false,
        }
    }

    pub fn take(&mut self) -> Option<&[u8]> {
        if self.ended {
            return None;
        }
        match self.output.try_recv() {
            Ok(bytes) => {
                self.held = bytes;
                Some(&self.held)
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.held.clear();
                Some(&self.held)
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ended = true;
                None
            }
        }
    }

    pub fn send(&self, bytes: Vec<u8>) -> bool {
        self.input.send(bytes).is_ok()
    }

    pub fn resize(&self, rows: u16, cols: u16) -> bool {
        self.resize.send((rows, cols)).is_ok()
    }
}
