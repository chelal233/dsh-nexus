//! Keep the group leader unreaped until its descendants have been signalled.
use std::{io, process::ExitStatus};

pub(crate) struct Child {
    child: tokio::process::Child,
    group: Option<i32>,
}

pub(crate) fn spawn(mut command: tokio::process::Command) -> io::Result<Child> {
    command.process_group(0);
    let child = command.spawn()?;
    let group = child.id().map(|pid| pid as i32);
    Ok(Child { child, group })
}

impl From<tokio::process::Child> for Child {
    fn from(child: tokio::process::Child) -> Self {
        Self { child, group: None }
    }
}

impl Child {
    pub(crate) fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub(crate) fn group(&self) -> Option<i32> {
        self.group
    }
    fn signal(&self, signal: i32) -> io::Result<()> {
        let target = self
            .group
            .map(|group| -group)
            .or_else(|| self.id().map(|pid| pid as i32));
        let Some(target) = target else {
            return Ok(());
        };
        if unsafe { libc::kill(target, signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(group) = self.group {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    group as _,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(None);
            }
            // The zombie leader reserves the group identity until cleanup is requested.
            self.signal(libc::SIGKILL)?;
            self.group = None;
        }
        self.child.try_wait()
    }
    pub(crate) async fn request_stop(&mut self) -> io::Result<()> {
        self.signal(libc::SIGTERM)
    }
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    pub(crate) async fn kill(&mut self) -> io::Result<()> {
        self.signal(libc::SIGKILL)?;
        self.wait().await.map(|_| ())
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.group.is_some() {
            let _ = self.signal(libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn graceful_stop_delivers_term_before_force_kill() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap 'exit 42' TERM; printf ready; while :; do sleep 0.1; done",
            ])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = spawn(command).unwrap();
        use tokio::io::AsyncReadExt;
        let mut ready = [0; 5];
        tokio::time::timeout(
            Duration::from_secs(5),
            child.child.stdout.as_mut().unwrap().read_exact(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        child.request_stop().await.unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exit.code(), Some(42));
        assert!(child.group().is_none());
    }

    #[tokio::test]
    async fn natural_leader_exit_closes_descendants_before_reaping() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "sleep 60 & exit 0"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = spawn(command).unwrap();
        let mut output = child.child.stdout.take().unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        // An orphan sleep would keep this pipe open for 60 seconds.
        use tokio::io::AsyncReadExt;
        tokio::time::timeout(Duration::from_secs(5), output.read_to_end(&mut Vec::new()))
            .await
            .unwrap()
            .unwrap();
    }
}
