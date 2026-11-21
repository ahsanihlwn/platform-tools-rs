//! Clean-room MIT Android Debug Bridge (`adb`) CLI client and host server daemon (`1.0.41`).

pub mod auth;
pub mod client;
pub mod config;
pub mod protocol;
pub mod server;
pub mod usb;

pub use client::run_cli;
pub use config::{
    help_body, help_output, parse_cli_args, version_output, AdbCliConfig, AdbCommand, DeviceTarget,
};
pub use protocol::{AdbError, AdbPacket};
pub use server::AdbHostServer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        let args: Vec<String> = vec![
            "-H",
            "localhost",
            "-P",
            "5039",
            "-s",
            "127.0.0.1:5555",
            "shell",
            "getprop",
            "ro.build.version.release",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let cfg = parse_cli_args(&args).unwrap();
        assert_eq!(cfg.server_host, "127.0.0.1");
        assert_eq!(cfg.server_port, 5039);
        assert_eq!(cfg.target.serial.as_deref(), Some("127.0.0.1:5555"));
        assert!(matches!(cfg.command, AdbCommand::Shell { .. }));
    }
}
