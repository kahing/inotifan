use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

pub struct CondvarAny {
    c: Condvar,
    m: Mutex<()>,
}
impl CondvarAny {
    pub fn new() -> Self {
        CondvarAny {
            c: Condvar::new(),
            m: Mutex::new(()),
        }
    }

    pub fn wait(&self) -> Result<MutexGuard<'_, ()>, PoisonError<MutexGuard<'_, ()>>> {
        let guard = self.m.lock();
        self.c.wait(guard.unwrap())
    }

    pub fn notify_one(&self) {
        let _guard = self.m.lock();
        self.c.notify_one();
    }

    pub fn notify_all(&self) {
        let _guard = self.m.lock();
        self.c.notify_all();
    }
}
