//! `DisplayMessage` construction for `notebook.security.*` keys: the English
//! template is the fallback, arguments fill `{name}` placeholders.

use sqlly_core::DisplayMessage;

fn fill(template: &str, arguments: &[(&str, String)]) -> String {
    arguments
        .iter()
        .fold(template.to_string(), |text, (name, value)| {
            text.replace(&format!("{{{name}}}"), value)
        })
}

pub(crate) fn message(
    key: &'static str,
    template: &str,
    arguments: &[(&str, String)],
) -> DisplayMessage {
    arguments.iter().fold(
        DisplayMessage::new(key, fill(template, arguments)),
        |message, (name, value)| message.with_arg(*name, value.clone()),
    )
}

/// Owner-only files and directories for the native stores.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod private_fs {
    use std::io::Write;
    use std::path::Path;

    /// Create `dir` (and parents), owner-only on Unix.
    pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Open options for a new owner-only file that must not exist yet.
    pub(crate) fn new_private_file() -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
    }

    /// Write `bytes` to `path` through an owner-only sibling temp file,
    /// fsync and rename, so a crash never leaves a torn file.
    pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("store");
        let tmp = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = new_private_file().open(&tmp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Overwrite a file with zeros before removing it. Best effort: copy-on-
    /// write file systems and SSD wear levelling can keep old blocks.
    pub(crate) fn secure_remove(path: &Path) -> std::io::Result<()> {
        if let Ok(meta) = std::fs::symlink_metadata(path)
            && meta.is_file()
            && let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(path)
        {
            let zeros = [0u8; 8192];
            let mut left = meta.len();
            while left > 0 {
                let n = left.min(zeros.len() as u64) as usize;
                if file.write_all(&zeros[..n]).is_err() {
                    break;
                }
                left -= n as u64;
            }
            let _ = file.sync_all();
        }
        match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlly_core::DisplayMessage;

    use crate::notebook_security::authorize::{Notice, Reason};
    use crate::notebook_security::redact::{ExportWarning, WarningKind};
    use crate::notebook_security::scan::{Finding, FindingKind, Severity};
    use crate::notebook_security::trust::{GrantError, TrustReason, TrustStoreError};

    const EN: &str = include_str!("../../../sqlly-i18n/translations/en.json");

    fn all_messages() -> Vec<DisplayMessage> {
        let s = || "x".to_string();
        let mut out: Vec<DisplayMessage> = vec![];
        out.extend(
            [
                TrustReason::CreatedHere,
                TrustReason::Granted,
                TrustReason::NotTrustedHere,
                TrustReason::ChangedOutsideApp,
            ]
            .map(TrustReason::message),
        );
        out.extend(
            [
                GrantError::NotAGrant,
                GrantError::UnknownConnection { slot: s() },
                GrantError::MissingNotebookId,
            ]
            .iter()
            .map(GrantError::message),
        );
        out.extend(
            [
                TrustStoreError::Io { detail: s() },
                TrustStoreError::Corrupt { detail: s() },
                TrustStoreError::NewerVersion { found: 9 },
            ]
            .iter()
            .map(TrustStoreError::message),
        );
        out.extend(
            [
                Notice::Production,
                Notice::SensitiveData,
                Notice::UntrustedContent,
            ]
            .map(Notice::message),
        );
        out.extend(
            [
                Reason::TrustRequired,
                Reason::UnknownCell { cell_id: s() },
                Reason::UnsupportedKind { kind: s() },
                Reason::ConnectionUnmapped { slot: s() },
                Reason::ConnectionNotAuthorized { slot: s() },
                Reason::ReadOnlyCellChangesData,
                Reason::WritesNotAllowed,
                Reason::WriteNeedsElevation,
                Reason::ProductionWritesLocked,
                Reason::ExportDisabled,
                Reason::SensitiveData,
                Reason::ExternalDestinationDisabled,
                Reason::ExternalDestination,
                Reason::MaterializeNotEnabled { slot: s() },
                Reason::CloudModelDisabled,
                Reason::CloudModelConsent,
                Reason::ScheduleNeedsFullTrust,
                Reason::ScheduleHasWriteCells { count: 2 },
                Reason::ScheduleHasWriteCells { count: 1 },
                Reason::ScheduleProduction { slot: s() },
                Reason::PublishHasWriteCells { count: 2 },
                Reason::PublishHasWriteCells { count: 1 },
                Reason::BenchmarkNoRuns,
                Reason::BenchmarkTooManyRuns { max: 200 },
                Reason::BenchmarkWritesData,
                Reason::BenchmarkProduction { slot: s() },
            ]
            .iter()
            .map(Reason::message),
        );
        out.extend(
            [
                WarningKind::SensitiveParameterRemoved,
                WarningKind::SensitiveColumnRemoved,
                WarningKind::SensitiveColumnMasked,
                WarningKind::SecretInSource,
                WarningKind::SecretInNotebook,
                WarningKind::SecretInConnection,
                WarningKind::SecretInOutput,
                WarningKind::IncompleteOutput,
            ]
            .map(|kind| {
                ExportWarning {
                    kind,
                    cell_id: Some("c_1".into()),
                    subject: Some("col".into()),
                }
                .message()
            }),
        );
        out.extend(
            [
                FindingKind::TooLarge { bytes: 9, limit: 1 },
                FindingKind::TooDeep { limit: 1 },
                FindingKind::StringTooLong { limit: 1 },
                FindingKind::NotJson,
                FindingKind::TooManyCells { count: 2, limit: 1 },
                FindingKind::TooManyParameters { count: 2, limit: 1 },
                FindingKind::TooManyConnections { count: 2, limit: 1 },
                FindingKind::EmbeddedCredential,
                FindingKind::UnsafePath,
                FindingKind::RemoteResource,
                FindingKind::ActiveContent,
                FindingKind::AutoRunRequested,
                FindingKind::EmbeddedOutputs,
                FindingKind::SecretInContent,
            ]
            .map(|kind| {
                Finding {
                    severity: Severity::Warning,
                    kind,
                    location: String::new(),
                }
                .message()
            }),
        );
        #[cfg(not(target_arch = "wasm32"))]
        {
            use crate::notebook_security::cache::CacheError;
            out.extend(
                [
                    CacheError::KeyUnavailable { detail: s() },
                    CacheError::Io { detail: s() },
                    CacheError::TooLarge { bytes: 9, limit: 1 },
                ]
                .iter()
                .map(CacheError::message),
            );
        }
        out
    }

    #[test]
    fn every_message_is_in_the_catalog_with_the_same_english_text() {
        let catalog: serde_json::Value = serde_json::from_str(EN).unwrap();
        let messages = all_messages();
        let mut keys = std::collections::BTreeSet::new();
        for m in &messages {
            let key = m.key().expect("keyed");
            keys.insert(key);
            let mut node = &catalog;
            for part in key.split('.') {
                node = node
                    .get(part)
                    .unwrap_or_else(|| panic!("{key} missing from en.json"));
            }
            let mut text = node["value"].as_str().unwrap().to_string();
            for (name, value) in m.arguments() {
                text = text.replace(&format!("{{{name}}}"), value);
            }
            assert_eq!(text, m.fallback(), "{key}");
        }
        let security = &catalog["notebook"]["security"];
        let catalog_leaves = security
            .as_object()
            .unwrap()
            .values()
            .map(|group| group.as_object().unwrap().len())
            .sum::<usize>();
        assert_eq!(
            catalog_leaves,
            keys.len(),
            "catalog has keys no message uses"
        );
    }
}
