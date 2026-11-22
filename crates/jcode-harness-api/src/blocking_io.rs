#[cfg(debug_assertions)]
thread_local! {
    static FORBIDDEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark a synchronous UI callback where SDK filesystem readers must not run.
/// Hold the guard only for the callback, never across an await.
#[must_use]
pub fn forbid_blocking_local_io() -> impl Drop {
    struct Scope {
        #[cfg(debug_assertions)]
        previous: bool,
        _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            #[cfg(debug_assertions)]
            FORBIDDEN.set(self.previous);
        }
    }
    Scope {
        #[cfg(debug_assertions)]
        previous: FORBIDDEN.replace(true),
        _thread_bound: std::marker::PhantomData,
    }
}

#[inline]
pub(crate) fn assert_allowed() {
    #[cfg(debug_assertions)]
    FORBIDDEN.with(|forbidden| {
        assert!(
            !forbidden.get(),
            "blocking harness filesystem read in a UI callback"
        );
    });
}

#[cfg(all(test, debug_assertions))]
#[test]
fn the_guard_is_nested_and_thread_local() {
    use crate::pending_awaits_in;
    let read = || pending_awaits_in(std::path::Path::new("does-not-exist"), "debug", 0);
    let outer = forbid_blocking_local_io();
    drop(forbid_blocking_local_io());
    assert!(std::panic::catch_unwind(read).is_err());
    std::thread::spawn(read).join().unwrap();
    drop(outer);
    read();
}
