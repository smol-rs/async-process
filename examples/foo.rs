use std::io;
use std::os::unix::process::ExitStatusExt;

use async_process::{Command, ExitStatus, Stdio};
use futures_lite::*;

fn main() -> io::Result<()> {
    future::block_on(async {
        // dbg!(std::process::Command::new("ls").arg(".").spawn()?.wait_with_output())?;

        dbg!(
            Command::new("ls")
                .arg(".")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .status()
                .await
        )?;

        // let mut child = Command::new("/bin/sh")
        //     .arg("-c")
        //     .arg("kill -9 $$")
        //     .spawn()?;
        // let status = child.status().await?;
        // dbg!(status);

        Ok(())
    })
}
