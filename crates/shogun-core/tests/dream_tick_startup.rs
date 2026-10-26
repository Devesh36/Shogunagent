//! Exercise the binary's async/blocking composition without providers or user stores.
#![cfg(feature = "daemon-server")]
#![allow(clippy::unwrap_used)]

#[test]
fn an_empty_local_dream_tick_exits_without_a_nested_runtime_panic() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("shogun-dream-tick-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_shogun-api"))
        .args(["--dream-tick", "--multi"])
        .arg(root.join("principals"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &root)
        .env("SHOGUN_DB_KEY", "11".repeat(32))
        .output();
    std::fs::remove_dir_all(&root).unwrap();
    let output = output.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("dream tick:"));
}
