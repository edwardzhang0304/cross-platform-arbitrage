//! Keep a dedicated OS thread alive: Windows power requests belong to threads.
use anyhow::{Result, ensure};
pub struct Awake {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Awake {
    pub fn acquire() -> Result<Self> {
        let (stop, wait) = std::sync::mpsc::channel();
        let (ready, status) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            #[cfg(windows)]
            let ok = unsafe {
                use windows_sys::Win32::System::Power::*;
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) != 0
            };
            #[cfg(not(windows))]
            let ok = true;
            let _ = ready.send(ok);
            if ok {
                let _ = wait.recv();
                #[cfg(windows)]
                unsafe {
                    use windows_sys::Win32::System::Power::*;
                    SetThreadExecutionState(ES_CONTINUOUS);
                }
            }
        });
        ensure!(status.recv()?, "Windows 防休眠设置失败，程序没有启动");
        Ok(Self { stop: Some(stop), thread: Some(thread) })
    }
}
impl Drop for Awake {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn request_can_be_acquired_and_released() {
        for _ in 0..3 { drop(super::Awake::acquire().unwrap()); }
    }
}
