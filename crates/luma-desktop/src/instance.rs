//! 单实例、再次启动时唤起窗口，以及安装程序请求退出。
//!
//! 用当前登录会话的命名 mutex 判重；两个命名 event 分别表示“显示窗口”和“退出”。
//! 第二次启动不会创建托盘/状态条/云端连接，只把已有主窗口拉到前台。

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;

    use windows::core::w;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, ReleaseMutex, SetEvent, WaitForMultipleObjects,
        EVENT_MODIFY_STATE, INFINITE,
    };

    const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\Luma.Desktop.SingleInstance");
    const ACTIVATE_NAME: windows::core::PCWSTR = w!("Local\\Luma.Desktop.Activate");
    const QUIT_NAME: windows::core::PCWSTR = w!("Local\\Luma.Desktop.Quit");

    pub struct SingleInstance {
        mutex: HANDLE,
        activate: HANDLE,
        quit: HANDLE,
    }

    pub enum Acquire {
        Primary(SingleInstance),
        AlreadyRunning,
        Unavailable,
    }

    #[derive(Clone, Copy)]
    enum Signal {
        Activate,
        Quit,
    }

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            unsafe {
                let _ = ReleaseMutex(self.mutex);
                close(&self.activate);
                close(&self.quit);
                let _ = CloseHandle(self.mutex);
            }
        }
    }

    fn valid(handle: HANDLE) -> bool {
        !handle.0.is_null() && handle.0 != -1_isize as *mut c_void
    }

    fn close(handle: &HANDLE) {
        if valid(*handle) {
            unsafe {
                let _ = CloseHandle(*handle);
            }
        }
    }

    fn signal(name: windows::core::PCWSTR) {
        unsafe {
            if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, name) {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
        }
    }

    /// 安装/升级前由 MSI 调用：请求已运行的实例退出；没有实例时静默返回。
    pub fn request_quit() {
        signal(QUIT_NAME);
    }

    pub fn acquire() -> Acquire {
        unsafe {
            let mutex = match CreateMutexW(None, true, MUTEX_NAME) {
                Ok(handle) => handle,
                // 无法创建同步对象时宁可让程序启动，也不要卡死。
                Err(error) => {
                    tracing::warn!(%error, "single-instance mutex unavailable");
                    return Acquire::Unavailable;
                }
            };

            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                signal(ACTIVATE_NAME);
                return Acquire::AlreadyRunning;
            }

            let activate = match CreateEventW(None, false, false, ACTIVATE_NAME) {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(%error, "activation event unavailable");
                    HANDLE::default()
                }
            };
            let quit = match CreateEventW(None, false, false, QUIT_NAME) {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(%error, "quit event unavailable");
                    HANDLE::default()
                }
            };

            Acquire::Primary(SingleInstance {
                mutex,
                activate,
                quit,
            })
        }
    }

    impl SingleInstance {
        /// 后台等待“显示窗口”或“退出”信号；退出直接结束 Slint 事件循环。
        pub fn on_activate<F>(&self, callback: F)
        where
            F: Fn() + Send + 'static,
        {
            let mut handles = Vec::with_capacity(2);
            let mut signals = Vec::with_capacity(2);
            if valid(self.activate) {
                handles.push(self.activate);
                signals.push(Signal::Activate);
            }
            if valid(self.quit) {
                handles.push(self.quit);
                signals.push(Signal::Quit);
            }
            if handles.is_empty() {
                return;
            }
            let raw = handles
                .iter()
                .map(|handle| handle.0 as usize)
                .collect::<Vec<_>>();

            std::thread::Builder::new()
                .name("luma-signals".into())
                .spawn(move || {
                    let handles = raw
                        .into_iter()
                        .map(|value| HANDLE(value as *mut c_void))
                        .collect::<Vec<_>>();
                    loop {
                        let result = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
                        if result != WAIT_OBJECT_0 && result.0 as usize >= handles.len() {
                            break;
                        }
                        match signals[result.0 as usize] {
                            Signal::Activate => callback(),
                            Signal::Quit => {
                                let _ = slint::quit_event_loop();
                                break;
                            }
                        }
                    }
                })
                .ok();
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub struct SingleInstance;

    pub enum Acquire {
        Primary(SingleInstance),
        AlreadyRunning,
        Unavailable,
    }

    pub fn request_quit() {}

    pub fn acquire() -> Acquire {
        Acquire::Primary(SingleInstance)
    }

    impl SingleInstance {
        pub fn on_activate<F>(&self, _callback: F)
        where
            F: Fn() + Send + 'static,
        {
        }
    }
}

pub use platform::{acquire, request_quit, Acquire};
