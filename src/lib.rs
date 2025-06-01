#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

pub mod noop;
mod rw_condvar;

use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::io::Error;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::raw::{c_char, c_int};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

use env_logger::Env;
use libc::{IN_NONBLOCK, O_CLOEXEC, pipe2};
use log::{debug, error, warn};

pub mod inotifan {
    use super::*;
    pub trait Inotify: Send + Sync + AsRawFd {
        /// Adds a watch for the given path.
        fn add_watch(&self, path: &Path, mask: u32) -> io::Result<i32>;

        /// Removes a watch.
        fn rm_watch(&self, wd: i32) -> io::Result<()>;

        fn read_events(&self) -> io::Result<Vec<InotifyEvent>>;

        /// Closes the inotify file descriptor.
        fn close(&self) -> io::Result<()>;
    }

    /// Represents an inotify event.
    #[derive(Debug)]
    pub struct InotifyEvent {
        pub wd: i32,
        pub mask: u32,
        pub cookie: u32,
        pub name: Option<String>,
    }

    #[repr(C)]
    #[derive(Debug)]
    struct CInotifyEvent {
        wd: i32,
        mask: u32,
        cookie: u32,
        len: u32,
        name: [c_char; 0], // Flexible array member
    }

    pub fn write_event(fd: c_int, event: InotifyEvent) -> io::Result<()> {
        let name_bytes = event.name.as_ref().map_or(&b""[..], |s| s.as_bytes());
        let len = name_bytes.len() as u32;
        let c_event = CInotifyEvent {
            wd: event.wd,
            mask: event.mask,
            cookie: event.cookie,
            len,
            name: [0; 0], // Placeholder, will be overwritten
        };

        let res = unsafe {
            libc::write(
                fd,
                &c_event as *const _ as *const libc::c_void,
                std::mem::size_of::<CInotifyEvent>(),
            )
        };
        if res < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }
}

use inotifan::Inotify;

type InotifyFd = RawFd;

static INIT: Once = Once::new();
// Create a global map to hold the file descriptors, use read write lock
lazy_static::lazy_static! {
    static ref INOTIFAN_MAP: RwLock<HashMap<InotifyFd, Box<dyn Inotify>>> = RwLock::new(HashMap::new());
    static ref CV: rw_condvar::CondvarAny = rw_condvar::CondvarAny::new();
}

const ENV_LOG_LEVEL: &str = "INOTIFAN_LOG_LEVEL";
const ENV_INOTIFY_BACKEND: &str = "INOTIFAN_BACKEND";

fn init() {
    INIT.call_once(|| {
        let env = Env::default().filter_or(ENV_LOG_LEVEL, "info"); // Use a different env var to avoid conflicts
        env_logger::init_from_env(env);
        error!("Initializing inotify");
        start_thread();
    });
}

fn start_thread() {
    thread::spawn(move || {
        loop {
            let epoll_fd = create_cleanup_epoll();
            if epoll_fd == -2 {
                debug!("No inotify handles to clean up, sleeping");
                let _unused = CV.wait().unwrap();
                continue;
            }
            let results = wait_for_events(epoll_fd, Duration::from_secs(1));
            // so it will be dropped
            let _owned_fd = unsafe { OwnedFd::from_raw_fd(epoll_fd) };

            if remove_all_closed(results) {
                continue;
            }
        }
    });
}

// add all wfd in InotifanHandle to epoll and return the epoll fd
fn create_cleanup_epoll() -> i32 {
    let map = INOTIFAN_MAP.read().unwrap();
    if map.is_empty() {
        return -2;
    }
    // create a new epoll instance
    let epoll_fd = unsafe { libc::epoll_create1(0) };
    if epoll_fd == -1 {
        error!("Error creating epoll instance: {}", Error::last_os_error());
        return -1;
    }

    // add all the wfd in InotifanHandle to epoll
    for (wfd, _) in map.iter() {
        // Check if the wfd is valid
        // Add the wfd to epoll, store handle.rfd in epoll_event
        let mut event = libc::epoll_event {
            events: libc::EPOLLHUP as u32,
            u64: *wfd as u64,
        };
        let res = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, *wfd, &mut event) };
        if res != 0 {
            error!("Error adding wfd to epoll: {}", Error::last_os_error());
        }
    }
    epoll_fd
}

fn remove_all_closed(fds: Vec<c_int>) -> bool {
    // Remove all closed file descriptors from the map
    let mut map = INOTIFAN_MAP.write().unwrap();
    for fd in fds {
        if let Some(_) = map.remove(&fd) {
            debug!("Removed closed file descriptor: {}", fd);
        } else {
            warn!("File descriptor {} closed but handle not found!", fd);
        }
        unsafe { libc::close(fd) };
    }

    return map.is_empty();
}

fn wait_for_events(epoll_fd: i32, timeout: Duration) -> Vec<i32> {
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 10];
    let num_events = unsafe {
        libc::epoll_wait(
            epoll_fd,
            events.as_mut_ptr(),
            10,
            timeout.as_millis() as i32,
        )
    };
    if num_events == -1 {
        error!("Error waiting for epoll events: {}", Error::last_os_error());
        return vec![];
    }
    debug!("Number of events: {}", num_events);

    // create an array to hold the results
    let mut results = vec![];
    for i in 0..num_events as usize {
        let event = events[i];
        if event.events & libc::EPOLLERR as u32 != 0 {
            results.push(event.u64 as c_int);
        }
    }

    results
}

// write a test for create_cleanup_epoll
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup() {
        let ifd = inotify_init();
        assert!(ifd > 0);
        // make sure we store the handle correctly
        assert!(!INOTIFAN_MAP.read().unwrap().is_empty());

        thread::sleep(Duration::from_secs(2));
        // make sure we didn't remove an unclosed handle
        assert!(!INOTIFAN_MAP.read().unwrap().is_empty());

        unsafe {
            libc::close(ifd);
        }

        thread::sleep(Duration::from_secs(2));
        // make sure we cleaned up this handle
        assert!(INOTIFAN_MAP.read().unwrap().is_empty());

        // create another handle to make sure we go from 1 to 0 back to 1 correctly
        let ifd2 = inotify_init();
        assert!(ifd2 > 0);
        assert!(!INOTIFAN_MAP.read().unwrap().is_empty());

        unsafe {
            libc::close(ifd2);
        }

        thread::sleep(Duration::from_secs(2));
        // this should be cleaned up
        assert!(INOTIFAN_MAP.read().unwrap().is_empty());
    }
}

fn create_pipe(flags: c_int) -> io::Result<(c_int, c_int)> {
    // Create a pipe for inotify
    let mut fds: [c_int; 2] = [0; 2];
    let res = unsafe { pipe2(fds.as_mut_ptr(), flags | O_CLOEXEC) };
    if res != 0 {
        return Err(Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn insert_handle<T: 'static>(h: T) -> io::Result<c_int>
where
    T: Inotify,
{
    let (rfd, wfd) = create_pipe(IN_NONBLOCK)?;
    debug!("Created pipe: rfd = {}, wfd = {}", rfd, wfd);

    let mut map = INOTIFAN_MAP.write().unwrap();
    map.insert(wfd as InotifyFd, Box::new(h));
    if map.len() == 1 {
        CV.notify_one();
    }
    Ok(rfd)
}

fn internal_inotify_init(flags: c_int) -> c_int {
    // Initialize the inotify system
    init();

    let backend = std::env::var(ENV_INOTIFY_BACKEND).unwrap_or_else(|_| "noop".to_string());
    let handle = match backend.as_str() {
        "noop" => noop::Inooptify::new(flags),
        _ => {
            error!("Unsupported inotify backend: {}", backend);
            return -1;
        }
    };

    match handle {
        Ok(handle) => insert_handle(handle).unwrap_or(-1),
        Err(e) => {
            error!("Error initializing inotify: {}", e);
            -1
        }
    }
}

// Intercepted inotify_init function (Returns a real file descriptor)
#[unsafe(no_mangle)]
pub extern "C" fn inotify_init() -> c_int {
    internal_inotify_init(0)
}

// Intercepted inotify_init1 function (Returns a real file descriptor)
#[unsafe(no_mangle)]
pub extern "C" fn inotify_init1(flags: c_int) -> c_int {
    internal_inotify_init(flags)
}

// Intercepted inotify_add_watch function (Returns a dummy watch descriptor)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inotify_add_watch(
    fd: c_int,
    c_pathname: *const c_char,
    _mask: u32,
) -> c_int {
    // Convert the pathname to a Rust string
    let path = unsafe { CStr::from_ptr(c_pathname).to_string_lossy().into_owned() };
    debug!(
        "Intercepted inotify_add_watch (Pathname: {}, Mask: {})",
        path, _mask
    );
    let map = INOTIFAN_MAP.read().unwrap();
    let handle = map.get(&fd);
    if handle.is_none() {
        error!("Invalid file descriptor: {}", fd);
        return -1;
    }
    let handle = handle.unwrap();
    let res = handle.add_watch(PathBuf::from(path).as_path(), _mask);
    if res.is_err() {
        error!("Error adding watch: {}", res.unwrap_err());
        return -1;
    }
    res.unwrap()
}

// Intercepted inotify_rm_watch function (Closes the file descriptor and returns success)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inotify_rm_watch(fd: c_int, wd: c_int) -> c_int {
    let map = INOTIFAN_MAP.read().unwrap();
    let handle = map.get(&fd);
    if handle.is_none() {
        error!("Invalid file descriptor: {}", fd);
        return -1;
    }
    let handle = handle.unwrap();
    let res = handle.rm_watch(wd);
    if res.is_err() {
        error!("Error adding watch: {}", res.unwrap_err());
        return -1;
    }

    0
}

// create wrapper functions for inotify and make them no-op
