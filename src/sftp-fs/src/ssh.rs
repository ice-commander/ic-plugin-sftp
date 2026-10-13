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

pub(crate) fn key_file(cfg: &Config) -> Option<std::path::PathBuf> {
    cfg.key_path
        .as_deref()
        .filter(|k| !k.is_empty())
        .map(expand_tilde)
}

pub(crate) fn authenticate(session: &Session, cfg: &Config) -> Result<(), String> {
    if let Some(key) = key_file(cfg) {
        session
            .userauth_pubkey_file(
                &cfg.user,
                None,
                &key,
                cfg.passphrase.as_deref().filter(|p| !p.is_empty()),
            )
            .map_err(|e| format!("SSH Key Auth failed: {e}"))?;
    } else if let Some(pass) = cfg.pass.as_deref() {
        session
            .userauth_password(&cfg.user, pass)
            .map_err(|e| format!("SSH Password Auth failed: {e}"))?;
    } else {
        return Err("No SSH authentication method specified".to_string());
    }
    if session.authenticated() {
        Ok(())
    } else {
        Err("the server did not accept the credentials".to_string())
    }
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
    authenticate(&session, cfg)?;

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
                Err(reason) => {
                    crate::note(&format!("{host}:{port}, reconnecting once after"), &reason);
                    self.session = None;
                }
            }
        }
        Err("the server kept refusing".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_key(key: Option<&str>) -> Config {
        Config {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            pass: None,
            key_path: key.map(str::to_string),
            passphrase: None,
            tunnel: None,
        }
    }

    #[test]
    fn a_key_under_the_home_folder_is_found_from_its_tilde() {
        let home = dirs::home_dir().expect("a home folder");
        assert_eq!(expand_tilde("~/.ssh/id_ed25519"), home.join(".ssh/id_ed25519"));
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn a_path_without_a_leading_tilde_is_left_as_it_is() {
        assert_eq!(expand_tilde("/etc/ssh/key"), std::path::PathBuf::from("/etc/ssh/key"));
        assert_eq!(expand_tilde("keys/id"), std::path::PathBuf::from("keys/id"));
        assert_eq!(expand_tilde("~someone/key"), std::path::PathBuf::from("~someone/key"));
    }

    #[test]
    fn the_key_handed_to_ssh_never_keeps_a_tilde() {
        let key = key_file(&with_key(Some("~/.ssh/id_ed25519"))).expect("a key");
        assert!(!key.to_string_lossy().starts_with('~'), "{}", key.display());
    }

    #[test]
    fn no_key_or_an_empty_one_means_signing_in_with_a_password() {
        assert!(key_file(&with_key(None)).is_none());
        assert!(key_file(&with_key(Some(""))).is_none());
    }

    #[test]
    fn the_terminal_signs_in_through_the_same_code_as_the_files() {
        let shell = include_str!("shell.rs");
        assert!(
            !shell.contains("userauth_pubkey_file") && !shell.contains("userauth_password"),
            "shell.rs signs in by itself again; it must call ssh::authenticate"
        );
        assert!(shell.contains("crate::ssh::authenticate("));
    }
}

