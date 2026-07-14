fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=DBOTTER_TARGET={target}");
    println!("cargo:rustc-env=DBOTTER_ARCH={arch}");
}
