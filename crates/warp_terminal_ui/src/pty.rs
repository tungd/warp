use std::{
    ffi::CString,
    fmt,
    fs::File,
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        io::{AsRawFd, FromRawFd, RawFd},
    },
    path::{Path, PathBuf},
    ptr,
    sync::{mpsc, Arc, Mutex},
    thread,
};

#[derive(Clone)]
pub struct PtySession {
    inner: Arc<PtySessionInner>,
}

impl fmt::Debug for PtySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtySession")
            .field("pid", &self.inner.pid)
            .finish_non_exhaustive()
    }
}

struct PtySessionInner {
    pid: libc::pid_t,
    writer: Mutex<File>,
    output_rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    size: Mutex<Option<PtySize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySession {
    pub fn spawn_login_shell(shell: PathBuf) -> io::Result<Self> {
        let cwd = initial_working_directory();
        let mut master_fd: RawFd = -1;
        let pid = unsafe {
            libc::forkpty(
                &mut master_fd,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };

        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == 0 {
            exec_login_shell(shell, cwd);
        }

        let master = unsafe { File::from_raw_fd(master_fd) };
        let reader = master.try_clone()?;
        let writer = master;
        let (output_tx, output_rx) = mpsc::channel();

        thread::spawn(move || read_pty_loop(reader, output_tx));

        Ok(Self {
            inner: Arc::new(PtySessionInner {
                pid,
                writer: Mutex::new(writer),
                output_rx: Mutex::new(output_rx),
                size: Mutex::new(None),
            }),
        })
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.inner
            .writer
            .lock()
            .expect("pty writer mutex poisoned")
            .write_all(bytes)
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let mut current_size = self.inner.size.lock().expect("pty size mutex poisoned");
        if *current_size == Some(size) {
            return Ok(());
        }

        let writer = self.inner.writer.lock().expect("pty writer mutex poisoned");
        set_pty_size(writer.as_raw_fd(), size)?;
        *current_size = Some(size);
        Ok(())
    }

    pub fn current_size(&self) -> Option<PtySize> {
        *self.inner.size.lock().expect("pty size mutex poisoned")
    }

    pub fn drain_output(&self) -> Vec<Vec<u8>> {
        let rx = self
            .inner
            .output_rx
            .lock()
            .expect("pty output mutex poisoned");
        rx.try_iter().collect()
    }
}

impl Drop for PtySessionInner {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.pid, libc::SIGHUP);
        }
    }
}

fn read_pty_loop(mut reader: File, output_tx: mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(bytes_read) => {
                if output_tx.send(buffer[..bytes_read].to_vec()).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn exec_login_shell(shell: PathBuf, cwd: PathBuf) -> ! {
    let _ = chdir(&cwd);
    let _ = set_env("TERM", "xterm-256color");
    let _ = set_env("COLORTERM", "truecolor");

    let shell_c = CString::new(shell.as_os_str().as_bytes())
        .unwrap_or_else(|_| CString::new("/bin/zsh").expect("static shell path is valid"));
    let shell_name = shell
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zsh");
    let argv0 = CString::new(format!("-{shell_name}"))
        .unwrap_or_else(|_| CString::new("-zsh").expect("static shell argv is valid"));

    unsafe {
        libc::execl(
            shell_c.as_ptr(),
            argv0.as_ptr(),
            ptr::null::<libc::c_char>(),
        );
        libc::_exit(127);
    }
}

fn initial_working_directory() -> PathBuf {
    std::env::current_dir()
        .ok()
        .filter(|cwd| cwd != Path::new("/"))
        .or_else(home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn chdir(path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let rc = unsafe { libc::chdir(path.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_pty_size(fd: RawFd, size: PtySize) -> io::Result<()> {
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &mut winsize) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_env(key: &str, value: &str) -> io::Result<()> {
    let key = CString::new(key).expect("static env key is valid");
    let value = CString::new(value).expect("static env value is valid");
    let rc = unsafe { libc::setenv(key.as_ptr(), value.as_ptr(), 1) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
