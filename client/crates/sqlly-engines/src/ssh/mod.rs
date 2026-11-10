//! Native-only SSH tunnel manager for SQLLY database drivers.
//!
//! Opens a local loopback listener that forwards TCP to a remote host:port
//! through one or two SSH hops, with host-key verification against the user's
//! OpenSSH `known_hosts` file. Not compiled for `wasm32`.

#![forbid(unsafe_code)]
#![cfg(not(target_arch = "wasm32"))]

pub mod challenge;
pub mod keepalive;
pub mod known_hosts;
pub mod kube;
pub mod proxy;
pub mod ssh_config;
pub mod ssm;
pub mod tunnel;

pub use challenge::{
    SshChallengeField, SshChallengePrompt, SshChallengeResponder, install_ssh_challenge_responder,
};
pub use known_hosts::{HostKeyPrompt, install_host_key_decider, set_known_hosts_path_for_tests};
pub use kube::{
    KubectlLocation, locate_kubectl, open_port_forward, parse_forwarding_line, set_kubectl_override,
};
pub use ssh_config::{SshConfigHost, load_ssh_config_file, load_user_ssh_config, parse_ssh_config};
pub use ssm::{
    AwsTool, aws_cli_not_found_error, locate_aws_tool, open_ssm_forward, set_aws_tool_override,
    ssm_plugin_not_found_error,
};
pub use tunnel::{TunnelManager, open_chain};

/// The server's own name when the driver dials somewhere else — the local
/// end of an SSH tunnel, proxy or port-forward that
/// [`TunnelManager::endpoint_for`] returned. TLS must check the server's
/// certificate against this name, not against `127.0.0.1`. `None` for a
/// direct connection, where the dialed host already is the server.
pub fn server_name_behind_tunnel<'a>(
    profile: &'a sqlly_core::ConnectionProfile,
    dialed_host: &str,
) -> Option<&'a str> {
    let server = profile.server.trim();
    (!server.is_empty() && !server.eq_ignore_ascii_case(dialed_host.trim())).then_some(server)
}
