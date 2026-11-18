//! 发布脚本（`scripts/build-release.ps1`）的 SHA-256 交叉校验。
//!
//! 发布包里的 `checksums.txt` 是 PowerShell `Get-FileHash` 写的，更新器却用
//! `snug-win` 自己的 [`snug_win::update::sha256_hex`] 去验。两套实现一旦分叉，
//! 用户要么收到莫名其妙的"校验失败"，要么更糟——校验形同虚设。
//!
//! 脚本会设置 `SNUG_SHA256_FILE` / `SNUG_SHA256_EXPECTED` 后调起这个测试；
//! 平时没有这两个环境变量（比如直接 `cargo test --workspace`）就跳过，不制造噪声。

use std::env;
use std::fs;

#[test]
fn rust_sha256_matches_the_system_hash_of_the_release_asset() {
    let Ok(path) = env::var("SNUG_SHA256_FILE") else {
        eprintln!("跳过 SHA-256 交叉校验：没有 SNUG_SHA256_FILE（由 scripts/build-release.ps1 设置）");
        return;
    };
    let expected = env::var("SNUG_SHA256_EXPECTED").expect("要同时给 SNUG_SHA256_EXPECTED");
    let bytes = fs::read(&path).unwrap_or_else(|error| panic!("读不了 {path}：{error}"));
    let actual = snug_win::update::sha256_hex(&bytes);

    assert_eq!(
        actual,
        expected.to_ascii_uppercase(),
        "Rust 与 Get-FileHash 对 {path} 的 SHA-256 不一致"
    );
    eprintln!("SHA-256 交叉校验通过：{path} -> {actual}");
}
