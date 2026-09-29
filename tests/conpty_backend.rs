#![cfg(windows)]

use portable_pty::win::{configure_bundled_conpty, conpty_status, ConptyBackend};
use portable_pty::{native_pty_system, PtySize};
use std::path::PathBuf;

const CASE_ENV: &str = "NYATERM_CONPTY_TEST_CASE";

fn bundled_dll() -> PathBuf {
    PathBuf::from(
        std::env::var_os("NYATERM_CONPTY_TEST_DLL")
            .expect("set NYATERM_CONPTY_TEST_DLL to the prepared conpty.dll path"),
    )
}

#[test]
#[ignore = "requires a prepared ConPTY package; set NYATERM_CONPTY_TEST_DLL"]
fn conpty_backend_cases() {
    if let Ok(case) = std::env::var(CASE_ENV) {
        run_case(&case);
        return;
    }

    assert!(
        bundled_dll().is_file(),
        "prepare the pinned ConPTY package first"
    );
    for case in ["bundled", "missing_dll", "corrupt_dll", "missing_host"] {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "conpty_backend_cases",
                "--ignored",
                "--nocapture",
            ])
            .env(CASE_ENV, case)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let temp = std::env::temp_dir().join(format!("nyaterm-conpty-test-{}", child.id()));
        let output = child.wait_with_output().unwrap();
        let _ = std::fs::remove_dir_all(temp);
        assert!(
            output.status.success(),
            "{case} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn run_case(case: &str) {
    let temp = std::env::temp_dir().join(format!("nyaterm-conpty-test-{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();
    let dll = match case {
        "bundled" => bundled_dll(),
        "missing_dll" => temp.join("missing.dll"),
        "corrupt_dll" => {
            let path = temp.join("conpty.dll");
            std::fs::write(&path, b"not a DLL").unwrap();
            let source_dir = bundled_dll().parent().unwrap().to_path_buf();
            for arch in if cfg!(target_arch = "aarch64") {
                &["arm64"][..]
            } else {
                &["x64", "arm64"][..]
            } {
                let host_dir = temp.join(arch);
                std::fs::create_dir_all(&host_dir).unwrap();
                std::fs::copy(
                    source_dir.join(arch).join("OpenConsole.exe"),
                    host_dir.join("OpenConsole.exe"),
                )
                .unwrap();
            }
            path
        }
        "missing_host" => {
            let path = temp.join("conpty.dll");
            std::fs::copy(bundled_dll(), &path).unwrap();
            path
        }
        other => panic!("unknown ConPTY test case: {}", other),
    };
    configure_bundled_conpty(dll).unwrap();
    assert!(conpty_status().last_used.is_none());

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    pair.master
        .resize(PtySize {
            rows: 25,
            cols: 81,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let status = conpty_status();
    let expected = if case == "bundled" {
        ConptyBackend::Bundled
    } else {
        ConptyBackend::System
    };
    assert_eq!(status.last_used, Some(expected));
    assert_eq!(
        status.active_bundled,
        usize::from(expected == ConptyBackend::Bundled)
    );
    assert_eq!(
        status.active_system,
        usize::from(expected == ConptyBackend::System)
    );
    assert_eq!(
        status.fallback_reason.is_some(),
        expected == ConptyBackend::System
    );

    drop(pair);
    let status = conpty_status();
    assert_eq!(status.active_bundled, 0);
    assert_eq!(status.active_system, 0);
    assert_eq!(status.last_used, Some(expected));
}
