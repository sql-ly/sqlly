#[cfg(target_os = "windows")]
use winreg::RegKey;

#[cfg(target_os = "windows")]
pub fn ensure_url_scheme_registered() -> std::io::Result<()> {
    use std::env;
    use winreg::enums::*;

    // A Store install declares the sqlly: protocol and the SQLite file types
    // in its package manifest. Its HKCU writes would land in the package's
    // private registry hive, invisible to the shell, and point at a
    // versioned install folder that moves on every update.
    if crate::distribution::is_microsoft_store() {
        return Ok(());
    }

    let exe = env::current_exe()?;
    let exe = exe.to_string_lossy().replace('/', "\\");
    let quoted = format!("\"{exe}\"");

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    register_url_protocol(&hkcu, &quoted, &exe)?;
    register_sqlite_open_with(&hkcu, &quoted, "sqlite")?;
    register_sqlite_open_with(&hkcu, &quoted, "db")?;
    register_sqlite_open_with(&hkcu, &quoted, "sqlite3")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn register_url_protocol(hkcu: &RegKey, quoted_exe: &str, _exe: &str) -> std::io::Result<()> {
    let (scheme, _) = hkcu.create_subkey("Software\\Classes\\sqlly")?;
    let current_default: String = scheme.get_value("").unwrap_or_default();
    if current_default != "URL:SQLLY Protocol" {
        scheme.set_value("", &"URL:SQLLY Protocol")?;
    }
    if scheme.get_value::<String, _>("URL Protocol").is_err() {
        scheme.set_value("URL Protocol", &"")?;
    }
    let (icon, _) = scheme.create_subkey("DefaultIcon")?;
    let icon_value = format!("{quoted_exe},0");
    if icon.get_value::<String, _>("").unwrap_or_default() != icon_value {
        icon.set_value("", &icon_value)?;
    }
    let (command, _) = scheme.create_subkey("shell\\open\\command")?;
    let command_value = format!("{quoted_exe} \"%1\"");
    if command.get_value::<String, _>("").unwrap_or_default() != command_value {
        command.set_value("", &command_value)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn register_sqlite_open_with(
    hkcu: &RegKey,
    quoted_exe: &str,
    extension: &str,
) -> std::io::Result<()> {
    let prog_id = format!("SQLLY.{extension}");
    let (open_with, _) =
        hkcu.create_subkey(format!("Software\\Classes\\.{extension}\\OpenWithProgids"))?;
    if open_with.get_value::<String, _>(&prog_id).is_err() {
        open_with.set_value(&prog_id, &"")?;
    }
    let (prog, _) = hkcu.create_subkey(format!("Software\\Classes\\{prog_id}"))?;
    let (command, _) = prog.create_subkey("shell\\open\\command")?;
    let command_value = format!("{quoted_exe} \"%1\"");
    if command.get_value::<String, _>("").unwrap_or_default() != command_value {
        command.set_value("", &command_value)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn ensure_url_scheme_registered() -> std::io::Result<()> {
    Ok(())
}
