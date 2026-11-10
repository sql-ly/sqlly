// SQLLY patch: the Mac App Store build
// sets SQLLY_APP_STORE=1, which compiles out the private AppKit selectors
// App Review rejects. See client/vendor/README.md.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(sqlly_app_store)");
    println!("cargo:rerun-if-env-changed=SQLLY_APP_STORE");
    if std::env::var("SQLLY_APP_STORE").as_deref() == Ok("1") {
        println!("cargo:rustc-cfg=sqlly_app_store");
    }
}
