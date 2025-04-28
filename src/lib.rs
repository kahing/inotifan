#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use libc::{IN_CLOEXEC, IN_NONBLOCK, O_CLOEXEC, O_NONBLOCK, pipe2};
use std::collections::HashMap;
use std::ffi::CStr;
use std::io::Error;
use std::os::raw::{c_char, c_int};
use std::sync::RwLock;
use std::thread;
use std::time::Duration;

// Create a struct to hold the file descriptor and its associated data
#[derive(Debug)]
struct InotifanHandle {
    rfd: c_int,
    wfd: c_int,
}
// Create a global map to hold the file descriptors, use read write lock
lazy_static::lazy_static! {
    static ref INOTIFAN_MAP: RwLock<HashMap<c_int, InotifanHandle>> = RwLock::new(HashMap::new());
}

// add all wfd in InotifanHandle to epoll and return the epoll fd
fn create_cleanup_epoll() -> i32 {
    // create a new epoll instance
    let epoll_fd = unsafe { libc::epoll_create1(0) };

    // add all the wfd in InotifanHandle to epoll
    let map = INOTIFAN_MAP.read().unwrap();
    for (_, handle) in map.iter() {
        // Check if the wfd is valid
        if handle.wfd > 0 {
            // Add the wfd to epoll, store handle.rfd in epoll_event
            let mut event = libc::epoll_event {
                events: libc::EPOLLHUP as u32,
                u64: handle.rfd as u64,
            };
            let res =
                unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, handle.wfd, &mut event) };
            if res != 0 {
                eprintln!("Error adding wfd to epoll: {}", Error::last_os_error());
            }
        }
    }
    epoll_fd
}

fn remove_all_closed(fds: Vec<c_int>) -> bool {
    // Remove all closed file descriptors from the map
    let mut map = INOTIFAN_MAP.write().unwrap();
    for fd in fds {
        if let Some(handle) = map.remove(&fd) {
            // rfd is already closed
            unsafe { libc::close(handle.wfd) };
            eprintln!("Removed closed file descriptor: {}", fd);
        }
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
        eprintln!("Error waiting for epoll events: {}", Error::last_os_error());
        return vec![];
    }

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

fn start_thread() {
    thread::spawn(move || {
        loop {
            let epoll_fd = create_cleanup_epoll();
            let results = wait_for_events(epoll_fd, Duration::from_secs(1));

            if remove_all_closed(results) {
                break;
            }
        }
    });
}

// write a test for create_cleanup_epoll
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_cleanup_epoll() {
        // Create a new InotifanHandle
        let handle = InotifanHandle::new(0).unwrap();
        unsafe {
            libc::close(handle);
        }
        // Create a new epoll instance
        let epoll_fd = create_cleanup_epoll();
        // Check if the epoll fd is valid
        assert!(epoll_fd > 0);
        let res = wait_for_events(epoll_fd, Duration::from_secs(1));
        assert!(!res.is_empty());
        assert!(res[0] == handle, "{} != {}", res[0], handle);
        remove_all_closed(res);
        assert!(INOTIFAN_MAP.read().unwrap().is_empty());
    }
}

// make constructor for InotifanHandle
impl InotifanHandle {
    fn new(flags: c_int) -> Result<c_int, Error> {
        // Create a pipe
        let pipe_flags: c_int = if flags & IN_NONBLOCK != 0 {
            O_NONBLOCK
        } else {
            0
        } | if flags & IN_CLOEXEC != 0 {
            O_CLOEXEC
        } else {
            0
        };

        let mut fds: [c_int; 2] = [0; 2];
        let res = unsafe { pipe2(fds.as_mut_ptr(), pipe_flags) };
        if res != 0 {
            return Err(Error::last_os_error());
        }
        // Create a new InotifanHandle
        let handle = InotifanHandle {
            rfd: fds[0],
            wfd: fds[1],
        };
        // Insert the handle into the map
        let mut map = INOTIFAN_MAP.write().unwrap();
        map.insert(handle.rfd, handle);
        if map.len() == 1 {
            // Start the cleanup thread if this is the first handle
            start_thread();
        }
        Ok(fds[0])
    }
}

// Intercepted inotify_init function (Returns a real file descriptor)
#[unsafe(no_mangle)]
pub extern "C" fn inotify_init() -> c_int {
    let handle = InotifanHandle::new(0);
    match handle {
        Ok(h) => h,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

// Intercepted inotify_init1 function (Returns a real file descriptor)
#[unsafe(no_mangle)]
pub extern "C" fn inotify_init1(flags: c_int) -> c_int {
    let handle = InotifanHandle::new(flags);
    match handle {
        Ok(h) => h,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

// Intercepted inotify_add_watch function (Returns a dummy watch descriptor)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inotify_add_watch(
    _fd: c_int,
    c_pathname: *const c_char,
    _mask: u32,
) -> c_int {
    // Convert the pathname to a Rust string
    let path = unsafe { CStr::from_ptr(c_pathname).to_string_lossy().into_owned() };
    eprintln!(
        "Intercepted inotify_add_watch (Pathname: {}, Mask: {})",
        path, _mask
    );

    1 // Return a dummy valid watch descriptor
}

// Intercepted inotify_rm_watch function (Closes the file descriptor and returns success)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inotify_rm_watch(_fd: c_int, _wd: c_int) -> c_int {
    0
}

// create wrapper functions for inotify and make them no-op
