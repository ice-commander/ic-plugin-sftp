// Every call may arrive on a worker thread, so nothing here touches the host.
use crate::ssh;
use crate::tunnel::TunnelConfig;
use ic_plugin_api::{
    IcBytes, IcConnectionVTable, IcDirEntry, IcFsHandle, IcFsVTable, IcListing, IC_ERR_INIT_FAILED,
    IC_OK,
};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};

struct Mounted {
    mount: ssh::Mount,
    names: Vec<CString>,
    entries: Vec<IcDirEntry>,
    body: Vec<u8>,
    failure: Option<CString>,
    said: Option<CString>,
}

impl Mounted {
    fn remember(&mut self, reason: String) {
        self.failure = CString::new(reason).ok();
    }
}

fn wanted(path: *const c_char) -> String {
    if path.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .to_string()
}

fn held<R>(handle: IcFsHandle, work: impl FnOnce(&mut Mounted) -> R) -> Option<R> {
    if handle.is_null() {
        return None;
    }
    Some(work(unsafe { &mut *(handle as *mut Mounted) }))
}

fn text_of(settings: &serde_json::Value, key: &str) -> Option<String> {
    settings
        .get(key)
        .and_then(|value| value.as_str())
        .map(|text| text.to_string())
        .filter(|text| !text.is_empty())
}

fn flag_of(settings: &serde_json::Value, key: &str) -> bool {
    match settings.get(key) {
        Some(serde_json::Value::Bool(on)) => *on,
        Some(serde_json::Value::String(text)) => text == "true",
        _ => false,
    }
}

fn tunnel_of(settings: &serde_json::Value) -> Option<TunnelConfig> {
    flag_of(settings, "use_tunnel").then(|| TunnelConfig {
        host: text_of(settings, "tunnel_host"),
        port: text_of(settings, "tunnel_port")
            .and_then(|text| text.parse().ok())
            .unwrap_or(22),
        user: text_of(settings, "tunnel_user"),
        auth_type: text_of(settings, "tunnel_auth_type"),
        pass: text_of(settings, "tunnel_pass"),
        key_path: text_of(settings, "tunnel_key_path"),
        passphrase: text_of(settings, "tunnel_passphrase"),
    })
}

pub extern "C" fn open(bytes: *const u8, len: u64, _user_data: *mut c_void) -> IcFsHandle {
    if bytes.is_null() || len == 0 {
        crate::note("open", "the host passed no connection settings");
        return std::ptr::null_mut();
    }
    let encoded = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
    let settings = match serde_json::from_slice::<serde_json::Value>(encoded) {
        Ok(settings) => settings,
        Err(e) => {
            crate::note("open", format_args!("the connection settings are not valid JSON ({e})"));
            return std::ptr::null_mut();
        }
    };
    let Some(host) = text_of(&settings, "host") else {
        crate::note("open", "the connection settings have no host");
        return std::ptr::null_mut();
    };
    let cfg = ssh::Config {
        host,
        port: text_of(&settings, "port")
            .and_then(|text| text.parse().ok())
            .unwrap_or(22),
        user: text_of(&settings, "user").unwrap_or_default(),
        pass: text_of(&settings, "pass"),
        key_path: text_of(&settings, "key_path"),
        passphrase: text_of(&settings, "passphrase"),
        tunnel: tunnel_of(&settings),
    };
    Box::into_raw(Box::new(Mounted {
        mount: ssh::Mount::new(cfg),
        names: Vec::new(),
        entries: Vec::new(),
        body: Vec::new(),
        failure: None,
        said: None,
    })) as IcFsHandle
}

pub extern "C" fn close(handle: IcFsHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle as *mut Mounted) });
}

pub extern "C" fn list(handle: IcFsHandle, path: *const c_char) -> IcListing {
    let path = wanted(path);
    held(handle, |mounted| {
        let found = mounted.mount.with(|session| session.list(&path));
        mounted.names.clear();
        mounted.entries.clear();
        match found {
            Ok(rows) => {
                for row in &rows {
                    let Ok(name) = CString::new(row.name.clone()) else {
                        continue;
                    };
                    mounted.names.push(name);
                    mounted.entries.push(IcDirEntry {
                        name: std::ptr::null(),
                        is_dir: if row.is_dir { 1 } else { 0 },
                        size: row.size,
                        modified: row.modified,
                        permissions: row.permissions.unwrap_or(0),
                        has_permissions: if row.permissions.is_some() { 1 } else { 0 },
                    });
                }
                for (slot, name) in mounted.entries.iter_mut().zip(mounted.names.iter()) {
                    slot.name = name.as_ptr();
                }
                IcListing {
                    items: mounted.entries.as_ptr(),
                    count: mounted.entries.len() as u32,
                }
            }
            Err(reason) => {
                crate::note(&format!("list {path}"), &reason);
                mounted.remember(reason);
                IcListing::EMPTY
            }
        }
    })
    .unwrap_or(IcListing::EMPTY)
}

pub extern "C" fn read(handle: IcFsHandle, path: *const c_char) -> IcBytes {
    let path = wanted(path);
    held(handle, |mounted| {
        match mounted.mount.with(|session| session.read(&path)) {
            Ok(body) => {
                mounted.body = body;
                IcBytes {
                    data: mounted.body.as_ptr(),
                    len: mounted.body.len() as u64,
                }
            }
            Err(reason) => {
                crate::note(&format!("read {path}"), &reason);
                mounted.remember(reason);
                IcBytes::EMPTY
            }
        }
    })
    .unwrap_or(IcBytes::EMPTY)
}

pub extern "C" fn is_read_only(_handle: IcFsHandle) -> c_int {
    0
}

pub extern "C" fn last_error(handle: IcFsHandle) -> *const c_char {
    held(handle, |mounted| {
        mounted.said = mounted.failure.take();
        mounted
            .said
            .as_ref()
            .map(|text| text.as_ptr())
            .unwrap_or(std::ptr::null())
    })
    .unwrap_or(std::ptr::null())
}

fn reported(handle: IcFsHandle, op: &str, outcome: Option<Result<(), String>>) -> c_int {
    match outcome.unwrap_or_else(|| Err("no connection".to_string())) {
        Ok(()) => IC_OK,
        Err(reason) => {
            crate::note(op, &reason);
            let _ = held(handle, |mounted| mounted.remember(reason));
            IC_ERR_INIT_FAILED
        }
    }
}

pub extern "C" fn write(
    handle: IcFsHandle,
    path: *const c_char,
    bytes: *const u8,
    len: u64,
) -> c_int {
    let path = wanted(path);
    let body = if bytes.is_null() || len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(bytes, len as usize) }.to_vec()
    };
    let outcome = held(handle, |mounted| {
        mounted.mount.with(|session| session.write(&path, &body))
    });
    reported(handle, &format!("write {path}"), outcome)
}

pub extern "C" fn create_dir(handle: IcFsHandle, path: *const c_char) -> c_int {
    let path = wanted(path);
    let outcome = held(handle, |mounted| {
        mounted.mount.with(|session| session.create_dir(&path))
    });
    reported(handle, &format!("create folder {path}"), outcome)
}

pub extern "C" fn remove(handle: IcFsHandle, path: *const c_char) -> c_int {
    let path = wanted(path);
    let outcome = held(handle, |mounted| {
        mounted.mount.with(|session| session.remove(&path))
    });
    reported(handle, &format!("remove {path}"), outcome)
}

pub extern "C" fn rename(handle: IcFsHandle, from: *const c_char, to: *const c_char) -> c_int {
    let (from, to) = (wanted(from), wanted(to));
    let outcome = held(handle, |mounted| {
        mounted.mount.with(|session| session.rename(&from, &to))
    });
    reported(handle, &format!("rename {from} -> {to}"), outcome)
}

pub extern "C" fn set_permissions(handle: IcFsHandle, path: *const c_char, mode: u32) -> c_int {
    let path = wanted(path);
    let outcome = held(handle, |mounted| {
        mounted
            .mount
            .with(|session| session.set_permissions(&path, mode))
    });
    reported(handle, &format!("chmod {mode:o} {path}"), outcome)
}

pub extern "C" fn shell_open(
    handle: IcFsHandle,
    cwd: *const c_char,
    rows: u32,
    cols: u32,
) -> ic_plugin_api::IcShellHandle {
    let cwd = wanted(cwd);
    held(handle, |mounted| {
        let shell = crate::shell::Shell::open(
            mounted.mount.config(),
            mounted.mount.endpoint(),
            crate::shell::starts_in(&cwd),
            rows.max(1) as u16,
            cols.max(1) as u16,
        );
        Box::into_raw(Box::new(shell)) as ic_plugin_api::IcShellHandle
    })
    .unwrap_or(std::ptr::null_mut())
}

pub extern "C" fn shell_read(shell: ic_plugin_api::IcShellHandle) -> IcBytes {
    if shell.is_null() {
        return IcBytes::EMPTY;
    }
    let open = unsafe { &mut *(shell as *mut crate::shell::Shell) };
    match open.take() {
        Some(bytes) => IcBytes {
            data: bytes.as_ptr(),
            len: bytes.len() as u64,
        },
        None => IcBytes::EMPTY,
    }
}

pub extern "C" fn shell_write(
    shell: ic_plugin_api::IcShellHandle,
    bytes: *const u8,
    len: u64,
) -> c_int {
    if shell.is_null() || bytes.is_null() || len == 0 {
        return IC_ERR_INIT_FAILED;
    }
    let open = unsafe { &*(shell as *const crate::shell::Shell) };
    let body = unsafe { std::slice::from_raw_parts(bytes, len as usize) }.to_vec();
    if open.send(body) {
        IC_OK
    } else {
        IC_ERR_INIT_FAILED
    }
}

pub extern "C" fn shell_resize(shell: ic_plugin_api::IcShellHandle, rows: u32, cols: u32) -> c_int {
    if shell.is_null() {
        return IC_ERR_INIT_FAILED;
    }
    let open = unsafe { &*(shell as *const crate::shell::Shell) };
    if open.resize(rows.max(1) as u16, cols.max(1) as u16) {
        IC_OK
    } else {
        IC_ERR_INIT_FAILED
    }
}

pub extern "C" fn shell_close(shell: ic_plugin_api::IcShellHandle) {
    if shell.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(shell as *mut crate::shell::Shell) });
}

extern "C" fn never_opened_inside_a_file(
    _source: ic_plugin_api::IcFsSource,
    _path: *const c_char,
    _user_data: *mut c_void,
) -> IcFsHandle {
    std::ptr::null_mut()
}

pub fn vtable() -> IcFsVTable {
    IcFsVTable {
        struct_size: std::mem::size_of::<IcFsVTable>() as u32,
        open_in: never_opened_inside_a_file,
        close,
        list,
        read,
        is_read_only,
        last_error,
        write: Some(write),
        create_dir: Some(create_dir),
        remove: Some(remove),
        rename: Some(rename),
        shell_open: Some(shell_open),
        shell_read: Some(shell_read),
        shell_write: Some(shell_write),
        shell_resize: Some(shell_resize),
        shell_close: Some(shell_close),
        shell_available: None,
        columns: None,
        list_rows: None,
        action_state: None,
        cell_clicked: None,
        set_permissions: Some(set_permissions),
    }
}

pub fn connection_vtable(fs: *const IcFsVTable) -> IcConnectionVTable {
    IcConnectionVTable {
        struct_size: std::mem::size_of::<IcConnectionVTable>() as u32,
        open,
        fs,
        describe: None,
        on_event: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(settings: &str) -> IcFsHandle {
        let handle = open(
            settings.as_ptr(),
            settings.len() as u64,
            std::ptr::null_mut(),
        );
        assert!(!handle.is_null(), "refused {settings}");
        handle
    }

    fn tunnel_in(handle: IcFsHandle) -> Option<TunnelConfig> {
        held(handle, |mounted| mounted.mount.config().tunnel.clone()).flatten()
    }

    fn failure_of(handle: IcFsHandle) -> String {
        let text = last_error(handle);
        assert!(!text.is_null(), "no failure recorded");
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn the_tunnel_stays_off_unless_the_form_switches_it_on() {
        for settings in [
            r#"{"host":"sftp.example.org","user":"ivan","pass":"x","tunnel_host":"jump.example.org"}"#,
            r#"{"host":"sftp.example.org","user":"ivan","pass":"x","use_tunnel":"false","tunnel_host":"jump.example.org","tunnel_user":"bob"}"#,
        ] {
            let handle = opened(settings);
            assert!(tunnel_in(handle).is_none(), "{settings}");
            let direct = held(handle, |mounted| mounted.mount.endpoint().resolve()).unwrap();
            assert_eq!(direct, Ok(("sftp.example.org".to_string(), 22)));
            close(handle);
        }
    }

    #[test]
    fn a_tunnel_takes_the_ssh_port_when_the_form_leaves_it_out() {
        let handle = opened(
            r#"{"host":"sftp.example.org","user":"ivan","use_tunnel":"true","tunnel_host":"jump.example.org","tunnel_user":"bob"}"#,
        );
        assert_eq!(tunnel_in(handle).expect("tunnel").port, 22);
        close(handle);

        let handle = opened(
            r#"{"host":"sftp.example.org","user":"ivan","use_tunnel":true,"tunnel_host":"jump.example.org","tunnel_port":"2022","tunnel_user":"bob"}"#,
        );
        assert_eq!(tunnel_in(handle).expect("tunnel").port, 2022);
        close(handle);
    }

    #[test]
    fn a_tunnel_carries_a_key_or_a_password_as_the_form_chose() {
        let handle = opened(
            r#"{"host":"sftp.example.org","user":"ivan","use_tunnel":"true","tunnel_host":"jump.example.org","tunnel_user":"bob","tunnel_auth_type":"key","tunnel_key_path":"~/.ssh/jump","tunnel_passphrase":"p"}"#,
        );
        let tunnel = tunnel_in(handle).expect("tunnel");
        assert_eq!(tunnel.auth_type.as_deref(), Some("key"));
        assert_eq!(tunnel.key_path.as_deref(), Some("~/.ssh/jump"));
        assert_eq!(tunnel.passphrase.as_deref(), Some("p"));
        assert!(tunnel.pass.is_none());
        close(handle);

        let handle = opened(
            r#"{"host":"sftp.example.org","user":"ivan","use_tunnel":"true","tunnel_host":"jump.example.org","tunnel_user":"bob","tunnel_auth_type":"password","tunnel_pass":"s"}"#,
        );
        let tunnel = tunnel_in(handle).expect("tunnel");
        assert_eq!(tunnel.auth_type.as_deref(), Some("password"));
        assert_eq!(tunnel.pass.as_deref(), Some("s"));
        assert!(tunnel.key_path.is_none());
        close(handle);
    }

    #[test]
    fn a_tunnel_without_its_host_or_user_says_so() {
        for (settings, said) in [
            (
                r#"{"host":"sftp.example.org","user":"ivan","pass":"x","use_tunnel":"true","tunnel_user":"bob"}"#,
                "Tunnel host not configured",
            ),
            (
                r#"{"host":"sftp.example.org","user":"ivan","pass":"x","use_tunnel":"true","tunnel_host":"jump.example.org"}"#,
                "Tunnel user not configured",
            ),
        ] {
            let handle = opened(settings);
            assert_eq!(list(handle, c"/".as_ptr()).count, 0);
            assert_eq!(failure_of(handle), said);
            close(handle);
        }
    }

    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_server_that_refuses_the_connection_is_reported_not_listed_as_empty() {
        let settings = serde_json::json!({
            "host": "127.0.0.1",
            "port": closed_port().to_string(),
            "user": "nobody",
            "pass": "never-sent",
        })
        .to_string();
        let handle = opened(&settings);
        assert_eq!(list(handle, c"/".as_ptr()).count, 0);
        let why = failure_of(handle);
        assert!(why.starts_with("TCP Connect:"), "got {why}");
        assert!(!why.contains("never-sent"), "the password leaked into the reason");
        close(handle);
    }

    #[test]
    fn a_jump_host_that_refuses_the_connection_is_reported_with_its_own_reason() {
        let settings = serde_json::json!({
            "host": "10.255.255.1",
            "port": "22",
            "user": "nobody",
            "pass": "never-sent",
            "use_tunnel": "true",
            "tunnel_host": "127.0.0.1",
            "tunnel_port": closed_port().to_string(),
            "tunnel_user": "jump",
            "tunnel_auth_type": "password",
            "tunnel_pass": "also-never-sent",
        })
        .to_string();
        let handle = opened(&settings);
        assert_eq!(list(handle, c"/".as_ptr()).count, 0);
        let why = failure_of(handle);
        assert!(why.starts_with("Tunnel TCP connect error:"), "got {why}");
        assert!(!why.contains("never-sent"), "a password leaked into the reason");
        close(handle);
    }

    #[test]
    fn a_chmod_on_a_null_handle_or_path_touches_nothing() {
        assert_ne!(
            set_permissions(std::ptr::null_mut(), c"/a".as_ptr(), 0o644),
            IC_OK
        );
        assert_ne!(
            set_permissions(std::ptr::null_mut(), std::ptr::null(), 0o644),
            IC_OK
        );
        let handle = opened(
            r#"{"host":"sftp.example.org","user":"ivan","pass":"x","use_tunnel":"true","tunnel_user":"bob"}"#,
        );
        assert_ne!(set_permissions(handle, std::ptr::null(), 0o600), IC_OK);
        assert_eq!(failure_of(handle), "Tunnel host not configured");
        close(handle);
    }

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }

    fn need(name: &str) -> String {
        env(name).unwrap_or_else(|| panic!("{name} is not set"))
    }

    fn names(handle: IcFsHandle, dir: &str) -> Vec<String> {
        let dir = CString::new(dir).unwrap();
        list(handle, dir.as_ptr())
            .as_slice()
            .iter()
            .map(|entry| entry.name_string())
            .collect()
    }

    fn at(path: &str) -> CString {
        CString::new(path).unwrap()
    }

    // IC_SFTP_E2E_HOST IC_SFTP_E2E_PORT IC_SFTP_E2E_USER IC_SFTP_E2E_PASS IC_SFTP_E2E_KEY IC_SFTP_E2E_DIR IC_SFTP_E2E_HOME IC_SFTP_E2E_TUNNEL_HOST IC_SFTP_E2E_TUNNEL_PORT IC_SFTP_E2E_TUNNEL_USER IC_SFTP_E2E_TUNNEL_PASS IC_SFTP_E2E_TUNNEL_KEY IC_SFTP_E2E_TUNNEL_PASSPHRASE
    fn e2e_settings(through_tunnel: bool) -> serde_json::Value {
        let mut settings = serde_json::json!({
            "host": need("IC_SFTP_E2E_HOST"),
            "port": env("IC_SFTP_E2E_PORT").unwrap_or_else(|| "22".to_string()),
            "user": need("IC_SFTP_E2E_USER"),
        });
        match (env("IC_SFTP_E2E_KEY"), env("IC_SFTP_E2E_PASS")) {
            (Some(key), _) => {
                settings["auth_type"] = "key".into();
                settings["key_path"] = key.into();
            }
            (None, Some(pass)) => settings["pass"] = pass.into(),
            (None, None) => panic!("IC_SFTP_E2E_KEY or IC_SFTP_E2E_PASS is not set"),
        }
        if !through_tunnel {
            return settings;
        }
        settings["use_tunnel"] = "true".into();
        settings["tunnel_host"] = need("IC_SFTP_E2E_TUNNEL_HOST").into();
        settings["tunnel_port"] = env("IC_SFTP_E2E_TUNNEL_PORT")
            .unwrap_or_else(|| "22".to_string())
            .into();
        settings["tunnel_user"] = need("IC_SFTP_E2E_TUNNEL_USER").into();
        match (
            env("IC_SFTP_E2E_TUNNEL_KEY"),
            env("IC_SFTP_E2E_TUNNEL_PASS"),
        ) {
            (Some(key), _) => {
                settings["tunnel_auth_type"] = "key".into();
                settings["tunnel_key_path"] = key.into();
                if let Some(phrase) = env("IC_SFTP_E2E_TUNNEL_PASSPHRASE") {
                    settings["tunnel_passphrase"] = phrase.into();
                }
            }
            (None, Some(pass)) => {
                settings["tunnel_auth_type"] = "password".into();
                settings["tunnel_pass"] = pass.into();
            }
            (None, None) => panic!("IC_SFTP_E2E_TUNNEL_KEY or IC_SFTP_E2E_TUNNEL_PASS is not set"),
        }
        settings
    }

    fn terminal_output(handle: IcFsHandle, cwd: &str, command: &[u8], until: &str) -> String {
        let shell = shell_open(handle, at(cwd).as_ptr(), 24, 80);
        assert!(!shell.is_null());
        assert_eq!(
            shell_write(shell, command.as_ptr(), command.len() as u64),
            IC_OK
        );
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            let produced = shell_read(shell);
            assert!(
                !produced.data.is_null(),
                "the terminal ended: {}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(produced.as_slice());
            if String::from_utf8_lossy(&seen).contains(until) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        shell_close(shell);
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    #[ignore = "needs a jump host and a target; see the variables above"]
    fn a_tunnelled_mount_lists_writes_reads_renames_removes_and_opens_a_terminal() {
        let dir = need("IC_SFTP_E2E_DIR");
        let handle = opened(&e2e_settings(true).to_string());
        let within = |name: &str| format!("{}/{name}", dir.trim_end_matches('/'));
        let file = format!("ic-sftp-e2e-{}.txt", std::process::id());
        let moved = format!("ic-sftp-e2e-{}-renamed.txt", std::process::id());

        names(handle, &dir);
        assert!(last_error(handle).is_null(), "{}", failure_of(handle));
        let tunnel_port = held(handle, |mounted| mounted.mount.tunnel_port()).flatten();
        assert!(
            tunnel_port.is_some(),
            "the listing did not go through a tunnel"
        );

        let body = b"through the jump host\n";
        assert_eq!(
            write(
                handle,
                at(&within(&file)).as_ptr(),
                body.as_ptr(),
                body.len() as u64
            ),
            IC_OK,
            "{}",
            failure_of(handle)
        );
        assert_eq!(read(handle, at(&within(&file)).as_ptr()).as_slice(), body);
        assert_eq!(
            rename(
                handle,
                at(&within(&file)).as_ptr(),
                at(&within(&moved)).as_ptr()
            ),
            IC_OK
        );
        let after = names(handle, &dir);
        assert!(
            after.contains(&moved) && !after.contains(&file),
            "{after:?}"
        );
        assert_eq!(remove(handle, at(&within(&moved)).as_ptr()), IC_OK);
        assert!(!names(handle, &dir).contains(&moved));
        assert_eq!(
            held(handle, |mounted| mounted.mount.tunnel_port()).flatten(),
            tunnel_port,
            "the live tunnel was not reused"
        );

        let said = terminal_output(
            handle,
            &dir,
            b"echo ic-sftp-e2e-$((40+2)); pwd\n",
            "ic-sftp-e2e-42",
        );
        assert!(said.contains("ic-sftp-e2e-42"), "{said}");
        close(handle);

        let port = tunnel_port.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the tunnel outlived its mount"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn walks_the_server_by_its_own_paths(through_tunnel: bool) {
        let home = need("IC_SFTP_E2E_HOME");
        let home = home.trim_end_matches('/');
        let user = home.rsplit('/').next().unwrap().to_string();
        let docs = format!("{home}/docs");
        let inner = format!("{docs}/inner");
        let handle = opened(&e2e_settings(through_tunnel).to_string());

        let root = names(handle, "/");
        assert!(last_error(handle).is_null(), "{}", failure_of(handle));
        assert!(
            ["home", "etc", "usr"]
                .iter()
                .all(|name| root.contains(&name.to_string())),
            "/ is not the server root: {root:?}"
        );
        assert!(!root.contains(&"docs".to_string()), "{root:?}");
        assert_eq!(
            held(handle, |mounted| mounted.mount.tunnel_port())
                .flatten()
                .is_some(),
            through_tunnel
        );
        assert!(names(handle, "").contains(&"docs".to_string()));
        assert!(names(handle, "/home").contains(&user));
        assert!(names(handle, home).contains(&"docs".to_string()));
        let mine = format!(
            "{}-{}",
            std::process::id(),
            if through_tunnel { "tunnelled" } else { "direct" }
        );
        let fresh = format!("{docs}/fresh-{mine}.txt");
        let moved = format!("{docs}/moved-{mine}.txt");
        let made = format!("{docs}/made-{mine}");
        let leaf = |path: &str| path.rsplit('/').next().unwrap().to_string();
        let left_clean = |listed: &[String]| {
            listed.contains(&"inner".to_string())
                && [&fresh, &moved, &made]
                    .iter()
                    .all(|path| !listed.contains(&leaf(path)))
        };
        let before = names(handle, &docs);
        assert!(left_clean(&before), "{before:?}");
        assert_eq!(names(handle, &inner), vec!["file.txt".to_string()]);
        assert_eq!(
            read(handle, at(&format!("{inner}/file.txt")).as_ptr()).as_slice(),
            b"ic-sftp-e2e\n"
        );

        let body = b"written into docs\n";
        assert_eq!(
            write(
                handle,
                at(&fresh).as_ptr(),
                body.as_ptr(),
                body.len() as u64
            ),
            IC_OK,
            "{}",
            failure_of(handle)
        );
        assert_eq!(
            rename(handle, at(&fresh).as_ptr(), at(&moved).as_ptr()),
            IC_OK,
            "{}",
            failure_of(handle)
        );
        assert_eq!(read(handle, at(&moved).as_ptr()).as_slice(), body);
        assert_eq!(create_dir(handle, at(&made).as_ptr()), IC_OK);
        let after = names(handle, &docs);
        assert!(
            after.contains(&leaf(&moved))
                && after.contains(&leaf(&made))
                && !after.contains(&leaf(&fresh)),
            "{after:?}"
        );
        assert!(!names(handle, home).contains(&leaf(&moved)));

        let mode_of = |name: String| {
            let dir = at(&docs);
            list(handle, dir.as_ptr())
                .as_slice()
                .iter()
                .find(|entry| entry.name_string() == name)
                .and_then(|entry| entry.permissions_opt())
                .map(|mode| mode & 0o7777)
        };
        let stat_on_server = |paths: &str, until: &str| {
            let command = format!("stat -c 'mode:%a:%n' {paths}\n");
            terminal_output(handle, &docs, command.as_bytes(), until)
        };
        for (path, mode) in [(&moved, 0o640), (&made, 0o750)] {
            assert_eq!(
                set_permissions(handle, at(path).as_ptr(), mode),
                IC_OK,
                "{}",
                failure_of(handle)
            );
            assert_eq!(mode_of(leaf(path)), Some(mode), "{path}");
        }
        let said = stat_on_server(&format!("{moved} {made}"), &format!("mode:750:{made}"));
        assert!(
            said.contains(&format!("mode:640:{moved}"))
                && said.contains(&format!("mode:750:{made}")),
            "{said}"
        );
        assert_eq!(set_permissions(handle, at(&moved).as_ptr(), 0o600), IC_OK);
        assert_eq!(mode_of(leaf(&moved)), Some(0o600));
        let said = stat_on_server(&moved, &format!("mode:600:{moved}"));
        assert!(said.contains(&format!("mode:600:{moved}")), "{said}");
        let missing = format!("{docs}/missing-{mine}");
        assert_ne!(set_permissions(handle, at(&missing).as_ptr(), 0o644), IC_OK);
        assert!(!failure_of(handle).is_empty());

        assert_eq!(remove(handle, at(&moved).as_ptr()), IC_OK);
        assert_eq!(remove(handle, at(&made).as_ptr()), IC_OK);
        let cleared = names(handle, &docs);
        assert!(left_clean(&cleared), "{cleared:?}");

        for (from, up, holds) in [
            (inner.as_str(), docs.as_str(), "inner"),
            (docs.as_str(), home, "docs"),
            (home, "/home", user.as_str()),
            ("/home", "/", "home"),
        ] {
            assert!(
                !names(handle, from).is_empty() && names(handle, up).contains(&holds.to_string()),
                "going up from {from} to {up}"
            );
        }
        assert!(!names(handle, "/").contains(&"docs".to_string()));

        for (cwd, shown) in [(docs.as_str(), docs.as_str()), ("/", "/")] {
            let marker = format!("at:{shown}\r\n");
            let said = terminal_output(handle, cwd, b"printf 'at:%s\\n' \"$PWD\"\n", &marker);
            assert!(said.contains(&marker), "opened at {cwd}: {said}");
        }
        close(handle);
    }

    #[test]
    #[ignore = "needs IC_SFTP_E2E_HOME holding docs/inner/file.txt that reads ic-sftp-e2e"]
    fn a_direct_mount_walks_the_server_by_its_own_paths() {
        walks_the_server_by_its_own_paths(false);
    }

    #[test]
    #[ignore = "needs IC_SFTP_E2E_HOME holding docs/inner/file.txt that reads ic-sftp-e2e, behind a jump host"]
    fn a_tunnelled_mount_walks_the_server_by_its_own_paths() {
        walks_the_server_by_its_own_paths(true);
    }

    #[test]
    fn a_failure_is_handed_over_once_and_then_forgotten() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let settings = format!(r#"{{"host":"127.0.0.1","port":"{port}","user":"nobody","pass":"never-sent"}}"#);
        let handle = open(settings.as_ptr(), settings.len() as u64, std::ptr::null_mut());
        assert!(!handle.is_null());
        assert!(last_error(handle).is_null(), "nothing has failed yet");
        assert_eq!(list(handle, c"/".as_ptr()).count, 0);
        assert!(!last_error(handle).is_null(), "the failed listing gave no reason");
        assert!(last_error(handle).is_null(), "the same failure was reported twice");
        close(handle);
    }
}
