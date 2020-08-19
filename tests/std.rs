//! These tests are borrowed from the `std::process` test suite.

use std::env;
use std::str;

use async_process::{Command, Output, Stdio};
use futures_lite::*;

#[test]
fn smoke() {
    future::block_on(async {
        let p = if cfg!(target_os = "windows") {
            Command::new("cmd").args(&["/C", "exit 0"]).spawn()
        } else {
            Command::new("true").spawn()
        };
        assert!(p.is_ok());
        let mut p = p.unwrap();
        assert!(p.status().await.unwrap().success());
    })
}

#[test]
fn smoke_failure() {
    match Command::new("if-this-is-a-binary-then-the-world-has-ended").spawn() {
        Ok(..) => panic!(),
        Err(..) => {}
    }
}

#[test]
fn exit_reported_right() {
    future::block_on(async {
        let p = if cfg!(target_os = "windows") {
            Command::new("cmd").args(&["/C", "exit 1"]).spawn()
        } else {
            Command::new("false").spawn()
        };
        assert!(p.is_ok());
        let mut p = p.unwrap();
        assert!(p.status().await.unwrap().code() == Some(1));
        drop(p.status().await);
    })
}

#[test]
#[cfg(unix)]
fn signal_reported_right() {
    use std::os::unix::process::ExitStatusExt;

    future::block_on(async {
        let mut p = Command::new("/bin/sh")
            .arg("-c")
            .arg("read a")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        p.kill().unwrap();
        match p.status().await.unwrap().signal() {
            Some(9) => {}
            result => panic!("not terminated by signal 9 (instead, {:?})", result),
        }
    })
}

pub async fn run_output(mut cmd: Command) -> String {
    let p = cmd.spawn();
    assert!(p.is_ok());
    let mut p = p.unwrap();
    assert!(p.stdout.is_some());
    let mut ret = String::new();
    p.stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut ret)
        .await
        .unwrap();
    assert!(p.status().await.unwrap().success());
    return ret;
}

#[test]
fn stdout_works() {
    future::block_on(async {
        if cfg!(target_os = "windows") {
            let mut cmd = Command::new("cmd");
            cmd.args(&["/C", "echo foobar"]).stdout(Stdio::piped());
            assert_eq!(run_output(cmd).await, "foobar\r\n");
        } else {
            let mut cmd = Command::new("echo");
            cmd.arg("foobar").stdout(Stdio::piped());
            assert_eq!(run_output(cmd).await, "foobar\n");
        }
    })
}

#[test]
#[cfg_attr(windows, ignore)]
fn set_current_dir_works() {
    future::block_on(async {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("pwd")
            .current_dir("/")
            .stdout(Stdio::piped());
        assert_eq!(run_output(cmd).await, "/\n");
    })
}

#[test]
#[cfg_attr(windows, ignore)]
fn stdin_works() {
    future::block_on(async {
        let mut p = Command::new("/bin/sh")
            .arg("-c")
            .arg("read line; echo $line")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        p.stdin
            .as_mut()
            .unwrap()
            .write("foobar".as_bytes())
            .await
            .unwrap();
        drop(p.stdin.take());
        let mut out = String::new();
        p.stdout
            .as_mut()
            .unwrap()
            .read_to_string(&mut out)
            .await
            .unwrap();
        assert!(p.status().await.unwrap().success());
        assert_eq!(out, "foobar\n");
    })
}

#[test]
fn test_process_status() {
    future::block_on(async {
        let mut status = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(&["/C", "exit 1"])
                .status()
                .await
                .unwrap()
        } else {
            Command::new("false").status().await.unwrap()
        };
        assert!(status.code() == Some(1));

        status = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(&["/C", "exit 0"])
                .status()
                .await
                .unwrap()
        } else {
            Command::new("true").status().await.unwrap()
        };
        assert!(status.success());
    })
}

#[test]
fn test_process_output_fail_to_start() {
    future::block_on(async {
        match Command::new("/no-binary-by-this-name-should-exist")
            .output()
            .await
        {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
            Ok(..) => panic!(),
        }
    })
}

#[test]
fn test_process_output_output() {
    future::block_on(async {
        let Output {
            status,
            stdout,
            stderr,
        } = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(&["/C", "echo hello"])
                .output()
                .await
                .unwrap()
        } else {
            Command::new("echo").arg("hello").output().await.unwrap()
        };
        let output_str = str::from_utf8(&stdout).unwrap();

        assert!(status.success());
        assert_eq!(output_str.trim().to_string(), "hello");
        assert_eq!(stderr, Vec::new());
    })
}

#[test]
fn test_process_output_error() {
    future::block_on(async {
        let Output {
            status,
            stdout,
            stderr,
        } = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(&["/C", "mkdir ."])
                .output()
                .await
                .unwrap()
        } else {
            Command::new("mkdir").arg("./").output().await.unwrap()
        };

        assert!(status.code() == Some(1));
        assert_eq!(stdout, Vec::new());
        assert!(!stderr.is_empty());
    })
}

#[test]
fn test_finish_once() {
    future::block_on(async {
        let mut prog = if cfg!(target_os = "windows") {
            Command::new("cmd").args(&["/C", "exit 1"]).spawn().unwrap()
        } else {
            Command::new("false").spawn().unwrap()
        };
        assert!(prog.status().await.unwrap().code() == Some(1));
    })
}

#[test]
fn test_finish_twice() {
    future::block_on(async {
        let mut prog = if cfg!(target_os = "windows") {
            Command::new("cmd").args(&["/C", "exit 1"]).spawn().unwrap()
        } else {
            Command::new("false").spawn().unwrap()
        };
        assert!(prog.status().await.unwrap().code() == Some(1));
        assert!(prog.status().await.unwrap().code() == Some(1));
    })
}

#[test]
fn test_wait_with_output_once() {
    future::block_on(async {
        let prog = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(&["/C", "echo hello"])
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        } else {
            Command::new("echo")
                .arg("hello")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        };

        let Output {
            status,
            stdout,
            stderr,
        } = prog.output().await.unwrap();
        let output_str = str::from_utf8(&stdout).unwrap();

        assert!(status.success());
        assert_eq!(output_str.trim().to_string(), "hello");
        assert_eq!(stderr, Vec::new());
    })
}

#[cfg(all(unix, not(target_os = "android")))]
pub fn env_cmd() -> Command {
    Command::new("env")
}

#[cfg(target_os = "android")]
pub fn env_cmd() -> Command {
    let mut cmd = Command::new("/system/bin/sh");
    cmd.arg("-c").arg("set");
    cmd
}

#[cfg(windows)]
pub fn env_cmd() -> Command {
    let mut cmd = Command::new("cmd");
    cmd.arg("/c").arg("set");
    cmd
}

#[test]
fn test_override_env() {
    future::block_on(async {
        // In some build environments (such as chrooted Nix builds), `env` can
        // only be found in the explicitly-provided PATH env variable, not in
        // default places such as /bin or /usr/bin. So we need to pass through
        // PATH to our sub-process.
        let mut cmd = env_cmd();
        cmd.env_clear().env("RUN_TEST_NEW_ENV", "123");
        if let Some(p) = env::var_os("PATH") {
            cmd.env("PATH", &p);
        }
        let result = cmd.output().await.unwrap();
        let output = String::from_utf8_lossy(&result.stdout).to_string();

        assert!(
            output.contains("RUN_TEST_NEW_ENV=123"),
            "didn't find RUN_TEST_NEW_ENV inside of:\n\n{}",
            output
        );
    })
}

#[test]
fn test_add_to_env() {
    future::block_on(async {
        let result = env_cmd()
            .env("RUN_TEST_NEW_ENV", "123")
            .output()
            .await
            .unwrap();
        let output = String::from_utf8_lossy(&result.stdout).to_string();

        assert!(
            output.contains("RUN_TEST_NEW_ENV=123"),
            "didn't find RUN_TEST_NEW_ENV inside of:\n\n{}",
            output
        );
    })
}

#[test]
fn test_capture_env_at_spawn() {
    future::block_on(async {
        let mut cmd = env_cmd();
        cmd.env("RUN_TEST_NEW_ENV1", "123");

        // This variable will not be present if the environment has already
        // been captured above.
        env::set_var("RUN_TEST_NEW_ENV2", "456");
        let result = cmd.output().await.unwrap();
        env::remove_var("RUN_TEST_NEW_ENV2");

        let output = String::from_utf8_lossy(&result.stdout).to_string();

        assert!(
            output.contains("RUN_TEST_NEW_ENV1=123"),
            "didn't find RUN_TEST_NEW_ENV1 inside of:\n\n{}",
            output
        );
        assert!(
            output.contains("RUN_TEST_NEW_ENV2=456"),
            "didn't find RUN_TEST_NEW_ENV2 inside of:\n\n{}",
            output
        );
    })
}
