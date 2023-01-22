use loom::thread;

mod event {
    use loom::sync::{Condvar, Mutex};
    pub struct Event {
        wait: Condvar,
        mutex: Mutex<bool>,
        autoclear: bool,
    }

    impl Event {
        pub fn new(autoclear: bool) -> Self {
            Self {
                wait: Condvar::new(),
                mutex: Mutex::new(false),
                autoclear,
            }
        }
        pub fn notify(&self) {
            let mut guard = self.mutex.lock().unwrap();
            *guard = true;
            if self.autoclear {
                self.wait.notify_one();
            } else {
                self.wait.notify_all();
            }
        }

        pub fn wait(&self) {
            let mut guard = self.mutex.lock().unwrap();
            while !*guard {
                guard = self.wait.wait(guard).unwrap();
            }
            if self.autoclear {
                *guard = false;
            }
        }

        pub fn clear(&self) {
            assert!(
                !self.autoclear,
                "Clearing an autoclear event makes no sense"
            );
            let mut guard = self.mutex.lock().unwrap();
            *guard = false;
        }
    }
}

mod r#impl {
    use super::event::Event;
    use loom::sync::atomic::AtomicI64;
    use loom::sync::{Mutex, MutexGuard};
    use std::cell::UnsafeCell;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::Ordering;

    pub(super) struct RwLock<T: Sync> {
        state: AtomicI64,
        departing_readers: AtomicI64,
        writer_wait: Event,
        reader_wait: Event,
        readers_waiting: AtomicI64,
        writers_lock: Mutex<()>,
        datum: UnsafeCell<T>,
    }
    const WRITER_WEIGHT: i64 = 1i64 << 60;

    pub(super) struct WriteGuard<'a, T: Sync> {
        lock: &'a RwLock<T>,
        guard: Option<MutexGuard<'a, ()>>,
    }

    impl<'a, T: Sync> Deref for WriteGuard<'a, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: we hold the lock
            unsafe { &*(self.lock.datum.get() as *const _) }
        }
    }

    impl<'a, T: Sync> DerefMut for WriteGuard<'a, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: we hold the lock
            unsafe { &mut *(self.lock.datum.get()) }
        }
    }

    impl<'a, T: Sync> Drop for WriteGuard<'a, T> {
        fn drop(&mut self) {
            self.lock
                .write_unlock(std::mem::replace(&mut self.guard, None).unwrap())
        }
    }

    pub(super) struct ReadGuard<'a, T: Sync> {
        lock: &'a RwLock<T>,
    }

    impl<'a, T: Sync> Deref for ReadGuard<'a, T> {
        type Target = T;
        fn deref(&self) -> &T {
            // SAFETY: we hold the lock for reading
            unsafe { &*(self.lock.datum.get() as *const _) }
        }
    }

    impl<'a, T: Sync> Drop for ReadGuard<'a, T> {
        fn drop(&mut self) {
            self.lock.read_unlock()
        }
    }

    impl<T: Sync> RwLock<T> {
        pub fn new(datum: T) -> Self {
            Self {
                state: AtomicI64::new(0),
                departing_readers: AtomicI64::new(0),
                writer_wait: Event::new(true),
                reader_wait: Event::new(false),
                readers_waiting: AtomicI64::new(0),
                writers_lock: Mutex::new(()),
                datum: UnsafeCell::new(datum),
            }
        }

        pub fn write_lock(&self) -> WriteGuard<T> {
            let lock = self.writers_lock.lock().unwrap();
            let state = self.state.fetch_sub(WRITER_WEIGHT, Ordering::Acquire);
            if state > 0 {
                let departing = self.departing_readers.fetch_add(state, Ordering::Relaxed) + state;
                if departing != 0 {
                    assert!(departing > 0);
                    self.writer_wait.wait();
                }
                self.state.load(Ordering::Acquire);
            }
            WriteGuard {
                lock: self,
                guard: Some(lock),
            }
        }

        fn write_unlock(&self, guard: MutexGuard<()>) {
            let state = self.state.fetch_add(WRITER_WEIGHT, Ordering::Release) + WRITER_WEIGHT;
            assert!(state >= 0);
            if state > 0 {
                assert!(self.state.load(Ordering::Relaxed) >= 0);
                self.readers_waiting.store(state, Ordering::Relaxed);
                self.reader_wait.notify();
                self.writer_wait.wait();
                self.reader_wait.clear();
            }
            drop(guard);
        }

        pub fn read_lock(&self) -> ReadGuard<T> {
            let state = self.state.fetch_add(1, Ordering::Acquire);
            if state < 0 {
                self.reader_wait.wait();
                let readers_waiting = self.readers_waiting.fetch_sub(1, Ordering::Relaxed);
                assert!(readers_waiting > 0);
                if readers_waiting == 1 {
                    self.writer_wait.notify();
                }
                self.state.load(Ordering::Acquire);
            }
            ReadGuard { lock: self }
        }
        fn read_unlock(&self) {
            let state = self.state.fetch_sub(1, Ordering::Relaxed);
            if state < 1 {
                let departing = self.departing_readers.fetch_sub(1, Ordering::Relaxed);
                if departing == 1 {
                    /* Last reader, wake up writer */
                    self.writer_wait.notify()
                }
            }
        }
    }
}

fn main() {
    loom::model(|| {
        let v1 = loom::sync::Arc::new(r#impl::RwLock::new([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]));
        let s2 = v1.clone();
        let sum_up = move || {
            for _ in 0..10 {
                let guard = s2.read_lock();
                let mut total = 0;
                let slice = &(*guard)[..];
                for &i in slice {
                    total += i
                }
                assert_eq!(total, 45);
            }
        };
        let shuffle = move || {
            for _ in 0..10 {
                let mut guard = v1.write_lock();
                guard.reverse();
            }
        };
        let a = thread::spawn(shuffle);
        let b = thread::spawn(sum_up);
        a.join().unwrap();
        b.join().unwrap();
    })
}
