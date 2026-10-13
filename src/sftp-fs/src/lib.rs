pub mod mount;
mod shell;
mod ssh;
mod tunnel;
use ic_plugin_api::{
    check_host, needs_plugin_assets, HostCheck, IcHost, IC_ABI_VERSION, IC_ERR_HOST_TOO_OLD,
    IC_ERR_HOST_UNKNOWN, IC_ERR_INIT_FAILED,
};
use std::ffi::CString;
use std::os::raw::{c_char, c_int};

pub(crate) fn note(what: &str, why: impl std::fmt::Display) {
    eprintln!("[ic-sftp] {what}: {why}");
}

pub const KIND: &str = "sftp";
pub const DOCUMENT: &str = include_str!("../documents/sftp.json");
pub const ID: &str = "ic-sftp-fs";
pub const ICON: &str = "sftp.svg";
pub const PICTURE: &[u8] = include_bytes!("../assets/sftp.svg");

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../version.rs"));

ic_plugin_api::declare_about!("ic-sftp-fs", "SFTP", plugins_version!(), "SFTP connections");

#[cfg_attr(feature = "export-abi", no_mangle)]
pub extern "C" fn ic_plugin_init(host: *const IcHost, _kind: *const c_char) -> c_int {
    match check_host(host, IC_ABI_VERSION, needs_plugin_assets()) {
        HostCheck::Ok => {}
        HostCheck::WrongMagic => return IC_ERR_HOST_UNKNOWN,
        HostCheck::TooOld { .. } | HostCheck::Truncated { .. } => return IC_ERR_HOST_TOO_OLD,
    }
    let Ok(id) = CString::new(KIND) else {
        return IC_ERR_INIT_FAILED;
    };
    if let (Ok(owner), Ok(name)) = (CString::new(ID), CString::new(ICON)) {
        unsafe {
            ((*host).register_plugin_asset)(
                owner.as_ptr(),
                name.as_ptr(),
                PICTURE.as_ptr(),
                PICTURE.len() as u64,
            )
        };
    }
    let fs = mount::vtable();
    let connection = mount::connection_vtable(&fs);
    unsafe {
        ((*host).register_connection_kind)(
            id.as_ptr(),
            DOCUMENT.as_ptr(),
            DOCUMENT.len() as u64,
            &connection,
            std::ptr::null_mut(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_plugin_refuses_a_host_it_does_not_recognise() {
        assert_eq!(
            ic_plugin_init(std::ptr::null(), std::ptr::null()),
            IC_ERR_HOST_UNKNOWN
        );
    }

    #[test]
    fn a_mount_refuses_settings_that_name_no_server() {
        for settings in ["{}", r#"{"user":"ivan"}"#, r#"{"host":""}"#] {
            let handle = mount::open(
                settings.as_ptr(),
                settings.len() as u64,
                std::ptr::null_mut(),
            );
            assert!(handle.is_null(), "opened on {settings}");
        }
    }

    #[test]
    fn a_mount_takes_the_ssh_port_when_the_form_leaves_it_out() {
        let settings = r#"{"host":"sftp.example.org","user":"ivan","pass":"x"}"#;
        let handle = mount::open(
            settings.as_ptr(),
            settings.len() as u64,
            std::ptr::null_mut(),
        );
        assert!(!handle.is_null());
        mount::close(handle);
    }

    #[test]
    fn paths_reach_the_server_as_given_and_only_an_empty_one_means_the_login_folder() {
        assert_eq!(crate::ssh::on_server(""), ".");
        assert_eq!(crate::ssh::on_server("/"), "/");
        assert_eq!(crate::ssh::on_server("/docs"), "/docs");
        assert_eq!(
            crate::ssh::on_server("/home/ivan/docs/a.txt"),
            "/home/ivan/docs/a.txt"
        );
        assert_eq!(crate::ssh::on_server("docs"), "docs");
    }

    #[test]
    fn the_terminal_starts_where_the_panel_stands() {
        assert_eq!(crate::shell::starts_in("/"), "/");
        assert_eq!(crate::shell::starts_in(""), "/");
        assert_eq!(
            crate::shell::starts_in("/home/ivan/docs"),
            "/home/ivan/docs"
        );
    }

    #[test]
    fn the_filesystem_side_offers_every_operation_the_protocol_supports() {
        let table = mount::vtable();
        assert!(table.write.is_some());
        assert!(table.create_dir.is_some());
        assert!(table.remove.is_some());
        assert!(table.rename.is_some());
        assert!(table.set_permissions.is_some());
    }

    #[test]
    fn the_kind_says_it_can_carry_a_terminal() {
        let table = mount::vtable();
        assert!(table.shell_open.is_some());
        assert!(table.shell_read.is_some());
        assert!(table.shell_write.is_some());
        assert!(table.shell_resize.is_some());
        assert!(table.shell_close.is_some());
    }

    #[test]
    fn nothing_shell_shaped_touches_a_null_handle() {
        assert!(mount::shell_open(std::ptr::null_mut(), std::ptr::null(), 24, 80).is_null());
        assert_eq!(mount::shell_read(std::ptr::null_mut()).len, 0);
        assert_ne!(
            mount::shell_write(std::ptr::null_mut(), b"x".as_ptr(), 1),
            ic_plugin_api::IC_OK
        );
        assert_ne!(
            mount::shell_resize(std::ptr::null_mut(), 24, 80),
            ic_plugin_api::IC_OK
        );
        mount::shell_close(std::ptr::null_mut());
    }
}
