use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4 + bytes.len() / 57);
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    output
        .as_bytes()
        .chunks(76)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn rust_lld(rustc: &str) -> PathBuf {
    let output = |argument: &str| {
        String::from_utf8(
            Command::new(rustc)
                .args(["--print", argument])
                .output()
                .expect("run rustc")
                .stdout,
        )
        .expect("UTF-8 rustc output")
        .trim()
        .to_owned()
    };
    Path::new(&output("sysroot"))
        .join("lib/rustlib")
        .join(output("host-tuple"))
        .join("bin/rust-lld")
}

fn main() {
    println!("cargo:rerun-if-changed=guest/docker-shutdown.rs");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let executable = output.join("luhmen-docker-shutdown");
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let status = Command::new(&rustc)
        .arg("guest/docker-shutdown.rs")
        .args([
            "--edition=2024",
            "--target=aarch64-unknown-linux-musl",
            "-Cpanic=abort",
            "-Copt-level=s",
            "-Cstrip=symbols",
            "-Clink-self-contained=yes",
        ])
        .arg(format!("-Clinker={}", rust_lld(&rustc).display()))
        .arg("-o")
        .arg(&executable)
        .status()
        .expect("compile the Linux guest helper");
    assert!(
        status.success(),
        "failed to compile the Linux guest helper; run `rustup target add aarch64-unknown-linux-musl`"
    );
    let bytes = fs::read(executable).expect("read Linux guest helper");
    fs::write(
        output.join("luhmen-docker-shutdown.b64"),
        encode_base64(&bytes),
    )
    .expect("write encoded Linux guest helper");
}
