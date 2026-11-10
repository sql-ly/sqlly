//! Plain-language hints added to connection errors the drivers report in
//! operating-system terms.

/// What macOS 15 and later says when it blocks an app from reaching an
/// address on the local network (a VM, a container, a server on the LAN)
/// because the app was not allowed under Local Network privacy: the socket
/// fails with `EHOSTUNREACH`, which reads as if the network were broken.
#[cfg(target_os = "macos")]
const LOCAL_NETWORK_HINT: &str = "macOS may be blocking SQLLY from your local network. \
     Allow SQLLY under System Settings → Privacy & Security → Local Network, then try again.";

pub fn connection_with_local_network_hint(error: String) -> sqlly_core::StudioError {
    #[cfg(target_os = "macos")]
    if error.to_ascii_lowercase().contains("no route to host") {
        return sqlly_core::StudioError::ConnectionMessage(
            sqlly_core::DisplayMessage::new(
                "services.errors.connection_macos_local_network",
                format!("Connection error: {error}. {LOCAL_NETWORK_HINT}"),
            )
            .with_arg("detail", error),
        );
    }
    sqlly_core::StudioError::Connection(error)
}

/// Adds the Local Network hint to a "No route to host" connection error on
/// macOS. Other errors, and other platforms, are returned unchanged.
pub fn with_local_network_hint(error: String) -> String {
    #[cfg(target_os = "macos")]
    if error.to_ascii_lowercase().contains("no route to host") {
        return format!("{error}. {LOCAL_NETWORK_HINT}");
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_route_to_host_points_at_the_local_network_setting_on_macos() {
        let text = with_local_network_hint("No route to host (os error 65)".into());
        #[cfg(target_os = "macos")]
        assert!(
            text.contains("Privacy & Security → Local Network"),
            "{text}"
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(text, "No route to host (os error 65)");
    }

    #[test]
    fn other_errors_are_unchanged() {
        assert_eq!(
            with_local_network_hint("Connection refused (os error 61)".into()),
            "Connection refused (os error 61)"
        );
    }
}
