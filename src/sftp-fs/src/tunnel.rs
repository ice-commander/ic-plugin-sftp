use crate::ssh::expand_tilde;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

const ACCEPT_POLL: Duration = Duration::from_millis(20);
const PUMP_IDLE: Duration = Duration::from_millis(5);
const LIBSSH2_ERROR_SOCKET_SEND: i32 = -7;
const LIBSSH2_ERROR_SOCKET_DISCONNECT: i32 = -13;
const LIBSSH2_ERROR_SOCKET_RECV: i32 = -43;

#[derive(Clone)]
pub struct TunnelConfig {
    pub host: Option<String>,
    pub port: u16,
    pub user: Option<String>,
    pub auth_type: Option<String>,
    pub pass: Option<String>,
    pub key_path: Option<String>,
    pub passphrase: Option<String>,
}

pub struct TunnelSession {
    session: Arc<Mutex<ssh2::Session>>,
    pub(crate) local_port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for TunnelSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub type TunnelSlot = Arc<Mutex<Option<TunnelSession>>>;

fn live_port(ts: &TunnelSession) -> Option<u16> {
    let session = ts.session.lock().unwrap_or_else(|e| e.into_inner());
    session.authenticated().then_some(ts.local_port)
}

pub fn ensure_tunnel_helper(
    target_host: &str,
    target_port: u16,
    cfg: &TunnelConfig,
    tunnel: &TunnelSlot,
) -> Result<(String, u16), String> {
    {
        let mut slot = tunnel.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(port) = slot.as_ref().and_then(live_port) {
            return Ok(("127.0.0.1".to_string(), port));
        }
        *slot = None;
    }

    let t_host = cfg
        .host
        .as_ref()
        .ok_or_else(|| "Tunnel host not configured".to_string())?;
    let t_port = cfg.port;
    let t_user = cfg
        .user
        .as_ref()
        .ok_or_else(|| "Tunnel user not configured".to_string())?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed to bind local loopback listener: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Failed to set listener non-blocking: {e}"))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get local port: {e}"))?
        .port();

    let tcp = TcpStream::connect(format!("{t_host}:{t_port}"))
        .map_err(|e| format!("Tunnel TCP connect error: {e}"))?;
    let mut sess =
        ssh2::Session::new().map_err(|e| format!("Failed to create SSH session: {e}"))?;
    sess.set_tcp_stream(tcp);
    sess.handshake()
        .map_err(|e| format!("Tunnel SSH Handshake failed: {e}"))?;

    match cfg.auth_type.as_deref() {
        Some("key") => {
            let Some(kp) = cfg.key_path.as_deref() else {
                return Err("Tunnel SSH private key path not configured".to_string());
            };
            sess.userauth_pubkey_file(t_user, None, &expand_tilde(kp), cfg.passphrase.as_deref())
                .map_err(|e| format!("Tunnel SSH key auth failed: {e}"))?;
        }
        Some(_) => {
            let Some(p) = cfg.pass.as_deref() else {
                return Err("Tunnel SSH password not configured".to_string());
            };
            sess.userauth_password(t_user, p)
                .map_err(|e| format!("Tunnel SSH password auth failed: {e}"))?;
        }
        None => return Err("Tunnel SSH auth type not specified".to_string()),
    }

    sess.set_blocking(false);

    let sess_arc = Arc::new(Mutex::new(sess));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut slot = tunnel.lock().unwrap_or_else(|e| e.into_inner());
        // A concurrent resolve stored its tunnel meanwhile; replacing it would stop the listener its caller dials.
        if let Some(port) = slot.as_ref().and_then(live_port) {
            return Ok(("127.0.0.1".to_string(), port));
        }
        *slot = Some(TunnelSession {
            session: sess_arc.clone(),
            local_port,
            stop: stop.clone(),
        });
    }

    let target_host_owned = target_host.to_string();
    // Weak: the slot owns the TunnelSession whose accept thread would otherwise keep the slot alive.
    let tunnel_for_loop = Arc::downgrade(tunnel);
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((client_stream, _)) => {
                    let target_host_c = target_host_owned.clone();
                    let sess_c = sess_arc.clone();
                    let tunnel_c = tunnel_for_loop.clone();
                    std::thread::spawn(move || {
                        pump(
                            client_stream,
                            &target_host_c,
                            target_port,
                            &sess_c,
                            &tunnel_c,
                        )
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL)
                }
                Err(_) => break,
            }
        }
    });

    Ok(("127.0.0.1".to_string(), local_port))
}

// libssh2 keeps reporting authenticated() after the jump host's socket dies.
fn transport_lost(e: &ssh2::Error) -> bool {
    matches!(
        e.code(),
        ssh2::ErrorCode::Session(
            LIBSSH2_ERROR_SOCKET_SEND | LIBSSH2_ERROR_SOCKET_DISCONNECT | LIBSSH2_ERROR_SOCKET_RECV
        )
    )
}

fn pump(
    mut std_stream: TcpStream,
    target_host: &str,
    target_port: u16,
    sess_c: &Mutex<ssh2::Session>,
    tunnel_c: &Weak<Mutex<Option<TunnelSession>>>,
) {
    let mut channel = {
        let sess_lock = sess_c.lock().unwrap_or_else(|e| e.into_inner());
        sess_lock.set_blocking(true);
        let ch_res = sess_lock.channel_direct_tcpip(target_host, target_port, None);
        sess_lock.set_blocking(false);
        match ch_res {
            Ok(ch) => ch,
            Err(e) => {
                if !sess_lock.authenticated() || transport_lost(&e) {
                    drop(sess_lock);
                    if let Some(tunnel) = tunnel_c.upgrade() {
                        *tunnel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    }
                }
                return;
            }
        }
    };

    let _ = std_stream.set_nonblocking(true);

    let mut buf_tcp = vec![0u8; 16384];
    let mut buf_ssh = vec![0u8; 16384];
    let mut tcp_offset = 0;
    let mut tcp_len = 0;
    let mut ssh_offset = 0;
    let mut ssh_len = 0;

    loop {
        let mut progressed = false;

        if tcp_len == 0 {
            match std_stream.read(&mut buf_tcp) {
                Ok(0) => break,
                Ok(n) => {
                    tcp_len = n;
                    tcp_offset = 0;
                    progressed = true;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        if tcp_len > 0 {
            let write_res = {
                let _sess_lock = sess_c.lock().unwrap_or_else(|e| e.into_inner());
                channel.write(&buf_tcp[tcp_offset..tcp_offset + tcp_len])
            };
            match write_res {
                Ok(n) => {
                    tcp_offset += n;
                    tcp_len -= n;
                    progressed = true;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        if ssh_len == 0 {
            let read_res = {
                let _sess_lock = sess_c.lock().unwrap_or_else(|e| e.into_inner());
                channel.read(&mut buf_ssh)
            };
            match read_res {
                Ok(0) => break,
                Ok(n) => {
                    ssh_len = n;
                    ssh_offset = 0;
                    progressed = true;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        if ssh_len > 0 {
            match std_stream.write(&buf_ssh[ssh_offset..ssh_offset + ssh_len]) {
                Ok(n) => {
                    ssh_offset += n;
                    ssh_len -= n;
                    progressed = true;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        if !progressed {
            std::thread::sleep(PUMP_IDLE);
        }
    }
}

#[derive(Clone)]
pub struct Endpoint {
    host: String,
    port: u16,
    tunnel_cfg: Option<TunnelConfig>,
    tunnel: TunnelSlot,
}

impl Endpoint {
    pub fn new(
        host: &str,
        port: u16,
        tunnel_cfg: Option<&TunnelConfig>,
        tunnel: &TunnelSlot,
    ) -> Self {
        Self {
            host: host.to_string(),
            port,
            tunnel_cfg: tunnel_cfg.cloned(),
            tunnel: tunnel.clone(),
        }
    }

    pub fn resolve(&self) -> Result<(String, u16), String> {
        match &self.tunnel_cfg {
            Some(cfg) => ensure_tunnel_helper(&self.host, self.port, cfg, &self.tunnel),
            None => Ok((self.host.clone(), self.port)),
        }
    }
}
