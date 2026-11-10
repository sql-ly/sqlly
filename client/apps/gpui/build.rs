fn main() {
    // Bake the UTC build date into the binary as SQLLY_BUILD_DATE
    // (YYYY-MM-DD); the prerelease expiry policy in src/build_info.rs reads
    // it via env!. SQLLY_BUILD_DATE can be set externally to stamp a fixed
    // date (release pipeline reproducibility).
    let build_date = std::env::var("SQLLY_BUILD_DATE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    println!("cargo:rustc-env=SQLLY_BUILD_DATE={build_date}");
    println!("cargo:rerun-if-env-changed=SQLLY_BUILD_DATE");

    check_feedback_signing_key();

    // Windows-only: embed the application icon and VERSIONINFO resources.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Icon: the .rc registers it as resource ID 1, which is the icon
        // GPUI's Windows platform loads for the window class — covering the
        // Explorer/exe icon, window icon, and taskbar icon.
        embed_resource::compile("resources/windows/sqlly.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the Windows icon resource failed");

        // VERSIONINFO: generated so the release pipeline can stamp the build
        // number (SQLLY_BUILD_VERSION or APP_VERSION, `a.b.c.d`); local
        // builds fall back to the crate version.
        let rc_path =
            std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("sqlly-version.rc");
        std::fs::write(&rc_path, version_rc()).expect("writing generated version .rc failed");
        embed_resource::compile(&rc_path, embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the Windows version resource failed");

        // Stack reserve: Windows defaults the main thread to 1 MB (vs 8 MB on
        // macOS). Debug builds of the monomorphized GPUI render chain use
        // ~700 KB for a single `AppView::render` frame, so first paint dies
        // with STATUS_STACK_OVERFLOW before stacker's recursion guard can
        // help. Match the macOS main-thread stack instead.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        } else {
            println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608");
        }
    }
    // Mac App Store edition: compile the StoreKit / bookmark glue.
    if std::env::var_os("CARGO_FEATURE_APP_STORE").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
    {
        build_app_store_glue();
    }
    println!("cargo:rerun-if-changed=native/macos/SqllyAppStore.swift");

    println!("cargo:rerun-if-changed=resources/windows/sqlly.rc");
    println!("cargo:rerun-if-changed=resources/windows/sqlly.ico");
    println!("cargo:rerun-if-env-changed=SQLLY_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=APP_VERSION");
}

/// Compile native/macos/SqllyAppStore.swift into a static library and link
/// it plus StoreKit and the Swift runtime (which ships in the OS at
/// /usr/lib/swift on every supported macOS).
fn build_app_store_glue() {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok(other) => panic!("the app-store build targets Apple Silicon only, not {other}"),
        Err(_) => "arm64",
    };
    let xcrun = |args: &[&str]| -> String {
        let out = std::process::Command::new("xcrun")
            .args(args)
            .output()
            .expect("xcrun not found: the app-store build needs Xcode");
        assert!(out.status.success(), "xcrun {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let sdk = xcrun(&["--sdk", "macosx", "--show-sdk-path"]);
    let swiftc = xcrun(&["-f", "swiftc"]);
    let library = out_dir.join("libsqlly_app_store.a");
    let status = std::process::Command::new(&swiftc)
        .args([
            "-parse-as-library",
            "-emit-library",
            "-static",
            "-module-name",
            "SqllyAppStore",
            "-swift-version",
            "5",
            "-O",
            "-target",
            &format!("{arch}-apple-macos13.0"),
            "-sdk",
            &sdk,
            "native/macos/SqllyAppStore.swift",
            "-o",
        ])
        .arg(&library)
        .status()
        .expect("running swiftc failed");
    assert!(
        status.success(),
        "compiling native/macos/SqllyAppStore.swift failed"
    );

    let toolchain_swift = std::path::Path::new(&swiftc)
        .parent()
        .and_then(|bin| bin.parent())
        .map(|usr| usr.join("lib/swift/macosx"))
        .expect("unexpected swiftc location");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    println!(
        "cargo:rustc-link-search=native={}",
        toolchain_swift.display()
    );
    println!("cargo:rustc-link-lib=static=sqlly_app_store");
    println!("cargo:rustc-link-lib=framework=StoreKit");
    println!("cargo:rustc-link-lib=framework=UserNotifications");
    println!("cargo:rustc-link-lib=framework=IOKit");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}

/// The build's version as up to four numeric parts (`major.feature.minor.
/// commit`), taken from the pipeline-provided env var, else the crate
/// version. Non-numeric fragments are skipped; missing parts are zero.
fn version_parts() -> [u16; 4] {
    let raw = std::env::var("SQLLY_BUILD_VERSION")
        .or_else(|_| std::env::var("APP_VERSION"))
        .or_else(|_| std::env::var("CARGO_PKG_VERSION"))
        .unwrap_or_default();
    let mut parts = [0u16; 4];
    for (slot, piece) in parts.iter_mut().zip(
        raw.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty()),
    ) {
        *slot = piece.parse().unwrap_or(0);
    }
    parts
}

fn version_rc() -> String {
    let [a, b, c, d] = version_parts();
    let dotted = format!("{a}.{b}.{c}.{d}");
    format!(
        r#"#pragma code_page(65001)
1 VERSIONINFO
FILEVERSION {a},{b},{c},{d}
PRODUCTVERSION {a},{b},{c},{d}
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Foxy Code Services, LLC"
      VALUE "FileDescription", "SQLLY, a database tool that loves you back, in a good way, nothing creepy."
      VALUE "FileVersion", "{dotted}"
      VALUE "InternalName", "SQLLY"
      VALUE "LegalCopyright", "© Foxy Code Services, LLC"
      VALUE "OriginalFilename", "SQLLY.exe"
      VALUE "ProductName", "SQLLY"
      VALUE "ProductVersion", "{dotted}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}

/// Validates `SQLLY_FEEDBACK_SIGNING_KEY`, the feedback signing key that
/// `src/intellisense_feedback.rs` reads with `option_env!`. The key is never
/// echoed. A malformed key fails the build rather than silently shipping a
/// build that cannot sign. `SQLLY_REQUIRE_FEEDBACK_KEY=1` (or `true`; the
/// release pipeline sets it on non-PR builds) makes a missing key fail the
/// build too; without it, a keyless
/// build compiles and simply cannot send feedback. Standalone builds never
/// embed the key.
fn check_feedback_signing_key() {
    println!("cargo:rerun-if-env-changed=SQLLY_FEEDBACK_SIGNING_KEY");
    println!("cargo:rerun-if-env-changed=SQLLY_REQUIRE_FEEDBACK_KEY");

    let standalone = std::env::var_os("CARGO_FEATURE_STANDALONE").is_some();
    // An Azure Pipelines macro whose secret was never fetched (PR builds)
    // arrives as the literal "$(NAME)"; that means "no key", not a bad key.
    let key = std::env::var("SQLLY_FEEDBACK_SIGNING_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty() && !v.trim().starts_with("$("));
    let required = std::env::var("SQLLY_REQUIRE_FEEDBACK_KEY")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"))
        .unwrap_or(false);

    if standalone {
        if key.is_some() {
            println!(
                "cargo:warning=SQLLY_FEEDBACK_SIGNING_KEY is ignored: standalone builds never embed it"
            );
        }
        return;
    }
    match key {
        Some(key) => {
            let key = key.trim();
            if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                panic!(
                    "SQLLY_FEEDBACK_SIGNING_KEY must be exactly 64 hex characters (a P-256 scalar); got {} characters",
                    key.len()
                );
            }
        }
        None if required => panic!(
            "SQLLY_REQUIRE_FEEDBACK_KEY=1 but SQLLY_FEEDBACK_SIGNING_KEY is not set; release builds must be able to sign feedback"
        ),
        None => {}
    }
}
