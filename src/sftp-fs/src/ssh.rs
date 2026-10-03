use crate::tunnel::{Endpoint, TunnelConfig, TunnelSlot};
use ssh2::Session;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: Option<String>,
    pub key_path: Option<String>,
    pub passphrase: Option<String>,
    pub tunnel: Option<TunnelConfig>,
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: u64,
    pub permissions: Option<u32>,
}

pub(crate) fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(mut home) = dirs::home_dir() {
            home.push(rest);
            return home;
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    std::path::PathBuf::from(path)
}

pub fn on_server(path: &str) -> &str {
    if path.is_empty() {
        "."
    } else {
        path
    }
}

pub struct Session2 {
    /// Held, not read: dropping it takes the `Sftp` channel with it.
    #[allow(dead_code)]
    pub session: Session,
    pub sftp: ssh2::Sftp,
}

fn connect(cfg: &Config, host: &str, port: u16) -> Result<Session2, String> {
    let tcp =
        TcpStream::connect(format!("{host}:{port}")).map_err(|e| format!("TCP Connect: {e}"))?;
    let mut session = Session::new().map_err(|e| format!("Session Create: {e}"))?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|e| format!("SSH Handshake: {e}"))?;

    if let Some(key) = cfg.key_path.as_deref().filter(|k| !k.is_empty()) {
        session
            .userauth_pubkey_file(
                &cfg.user,
                None,
                &expand_tilde(key),
                cfg.passphrase.as_deref(),
            )
            .map_err(|e| format!("SSH Key Auth failed: {e}"))?;
    } else if let Some(pass) = cfg.pass.as_deref() {
        session
            .userauth_password(&cfg.user, pass)
            .map_err(|e| format!("SSH Password Auth failed: {e}"))?;
    } else {
        return Err("No SSH authentication method specified".to_string());
    }

    let sftp = session.sftp().map_err(|e| format!("SFTP Init: {e}"))?;
    Ok(Session2 { session, sftp })
}

fn delete_recursive(sftp: &ssh2::Sftp, path: &Path) -> Result<(), String> {
    if let Ok(stat) = sftp.stat(path) {
        if stat.is_dir() {
            if let Ok(entries) = sftp.readdir(path) {
                for (sub, sub_stat) in entries {
                    let name = sub
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    if name == "." || name == ".." {
                        continue;
                    }
                    if sub_stat.is_dir() {
                        let _ = delete_recursive(sftp, &sub);
                    } else {
                        let _ = sftp.unlink(&sub);
                    }
                }
            }
            return sftp.rmdir(path).map_err(|e| e.to_string());
        }
    }
    sftp.unlink(path).map_err(|e| e.to_string())
}

impl Session2 {
    fn alive(&self) -> bool {
        self.sftp.stat(Path::new(".")).is_ok()
    }

    pub fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        let target = on_server(path);
        let mut found = Vec::new();
        for (entry_path, stat) in self
            .sftp
            .readdir(Path::new(target))
            .map_err(|e| format!("SFTP ReadDir: {e}"))?
        {
            let name = entry_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name == "." || name == ".." {
                continue;
            }
            found.push(Entry {
                is_dir: stat.is_dir(),
                size: stat.size.unwrap_or(0),
                modified: stat.mtime.unwrap_or(0),
                permissions: stat.perm,
                name,
            });
        }
        Ok(found)
    }

    pub fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        let mut file = self.sftp.open(Path::new(path)).map_err(|e| e.to_string())?;
        let mut body = Vec::new();
        file.read_to_end(&mut body).map_err(|e| e.to_string())?;
        Ok(body)
    }

    pub fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let mut file = self
            .sftp
            .create(Path::new(path))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())
    }

    pub fn create_dir(&self, path: &str) -> Result<(), String> {
        self.sftp
            .mkdir(Path::new(path), 0o755)
            .map_err(|e| e.to_string())
    }

    pub fn remove(&self, path: &str) -> Result<(), String> {
        delete_recursive(&self.sftp, Path::new(path))
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.sftp
            .rename(Path::new(from), Path::new(to), None)
            .map_err(|e| e.to_string())
    }

    pub fn set_permissions(&self, path: &str, mode: u32) -> Result<(), String> {
        let stat = ssh2::FileStat {
            size: None,
            uid: None,
            gid: None,
            perm: Some(mode),
            atime: None,
            mtime: None,
        };
        self.sftp
            .setstat(Path::new(path), stat)
            .map_err(|e| e.to_string())
    }
}

pub struct Mount {
    cfg: Config,
    session: Option<Session2>,
    tunnel: TunnelSlot,
}

impl Mount {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            session: None,
            tunnel: TunnelSlot::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::new(
            &self.cfg.host,
            self.cfg.port,
            self.cfg.tunnel.as_ref(),
            &self.tunnel,
        )
    }

    #[cfg(test)]
    pub fn tunnel_port(&self) -> Option<u16> {
        self.tunnel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|ts| ts.local_port)
    }

    pub fn with<T>(
        &mut self,
        mut work: impl FnMut(&Session2) -> Result<T, String>,
    ) -> Result<T, String> {
        let (host, port) = self.endpoint().resolve()?;
        for attempt in 0..2 {
            let usable = match &self.session {
                Some(open) => open.alive(),
                None => false,
            };
            if !usable {
                self.session = Some(connect(&self.cfg, &host, port)?);
            }
            let open = self.session.as_ref().expect("just connected");
            match work(open) {
                Ok(value) => return Ok(value),
                Err(reason) if attempt == 1 => return Err(reason),
                Err(_) => self.session = None,
            }
        }
        Err("the server kept refusing".to_string())
    }
}
