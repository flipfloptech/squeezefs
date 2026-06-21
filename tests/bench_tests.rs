use std::process::Command;
use tempfile::tempdir;

#[test]
fn test_cli_help() {
    let output = Command::new("./target/debug/squeezefs")
        .arg("--help")
        .output()
        .expect("Failed to execute squeezefs help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mount"));
    assert!(stdout.contains("bench"));
}

#[test]
fn test_cli_bench_local() {
    let temp_dir = tempdir().unwrap();
    let output = Command::new("./target/debug/squeezefs")
        .arg("bench")
        .arg(temp_dir.path())
        .arg("--threads")
        .arg("2")
        .arg("--size")
        .arg("1") // 1MB for fast execution in test
        .output()
        .expect("Failed to execute squeezefs bench");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Write big file"));
    assert!(stdout.contains("Read big file"));
    assert!(stdout.contains("Write small files"));
    assert!(stdout.contains("Read small files"));
    assert!(stdout.contains("Stat files"));
}
