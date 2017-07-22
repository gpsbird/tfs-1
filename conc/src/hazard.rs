//! Hazard pointer management.
//!
//! "Hazard" is the general name for an atomic pointer potentially "protecting" some object from
//! getting deleted. First when the hazard is changed to another state, the object can be deleted.
//!
//! Since hazards are only useful when they're shared between the global and local state, a "hazard
//! pair" refers to the collection of the two connected ends of the hazard (they both share a
//! pointer to the hazard on the heap). When such pair is created, its reading end is usually
//! stored in the global state, such that a thread can check that no hazard is matching a
//! particular object during garbage collection. The writer part controls what the state of the
//! hazard is and is usually passed around locally.
//!
//! The asymmetry of a hazard pair is strictly speaking not necessary, but it allows to enforce
//! rules (e.g. only the reader/global part may deallocate the hazard box).

use std::sync::atomic::{self, AtomicPtr};
use std::{ops, mem, thread};

use local;

/// Pointers to this represents the blocked state.
static BLOCKED: u8 = 0;
/// Pointers to this represents the free state.
static FREE: u8 = 0;
/// Pointers to this represents the dead state.
static DEAD: u8 = 0;

/// The state of a hazard.
///
/// Note that this `enum` excludes the blocked state, because it is semantically different from the
/// other states.
#[derive(PartialEq, Debug)]
#[must_use = "Hazard states are expensive to fetch and have no value unless used."]
pub enum State {
    /// The hazard does not currently protect any object.
    Free,
    /// The hazard is dead and may be deallocated when necessary.
    Dead,
    /// The hazard is protecting an object.
    ///
    /// "Protecting" means that the pointer it holds is not deleted while the hazard is in this
    /// state.
    ///
    /// The inner pointer is restricted to values not overlapping with the trap value,
    /// corresponding to one of the other states.
    Protect(*const u8),
}

/// A hazard.
///
/// This type holds an atomic pointer with the state of the hazard. It represents the same as
/// `State` but is encoded such that it is atomically accessible.
///
/// Furthermore, there is an additional state: Blocked. If the hazard is in this state, reading it
/// will block until it no longer is. This is useful for blocking garbage collection while a value
/// is being read (avoiding the ABA problem).
#[derive(Debug)]
pub struct Hazard {
    /// The object it protects.
    ///
    /// If this is a pointer to `BLOCKED`, `FREE`, `DEAD`, it represents the respectiive state.
    ptr: AtomicPtr<u8>,
}

impl Hazard {
    /// Create a new hazard in blocked state.
    pub fn blocked() -> Hazard {
        Hazard {
            ptr: AtomicPtr::new(&BLOCKED as *const u8 as *mut u8),
        }
    }

    /// Block the hazard.
    pub fn block(&self) {
        self.ptr.store(&BLOCKED as *const u8 as *mut u8, atomic::Ordering::Release);
    }

    /// Is the hazard blocked?
    pub fn is_blocked(&self) -> bool {
        self.ptr.load(atomic::Ordering::Acquire) as *const u8 == &BLOCKED
    }

    /// Set the hazard to a new state.
    ///
    /// Whether or not it is blocked has no effect on this. To get it back to the blocked state,
    /// use `self.block()`.
    pub fn set(&self, new: State) {
        // Simply encode and store.
        self.ptr.store(match new {
            State::Free => &FREE,
            State::Dead => &DEAD,
            State::Protect(ptr) => ptr,
        } as *mut u8, atomic::Ordering::Release);
    }

    /// Get the state of the hazard.
    ///
    /// It will spin until the hazard is no longer in a blocked state, unless it is in debug mode,
    /// where it will panic given enough spins.
    pub fn get(&self) -> State {
        // In debug mode, we count the number of spins. In release mode, this should be trivially
        // optimized out.
        let mut spins = 0;

        // Spin until not blocked.
        loop {
            let ptr = self.ptr.load(atomic::Ordering::Acquire) as *const u8;

            // Blocked means that the hazard is blocked by another thread, and we must loop until
            // it assumes another state.
            if ptr == &BLOCKED {
                // Increment the number of spins.
                spins += 1;
                debug_assert!(spins < 100_000_000, "\
                    Hazard blocked for 100 millions rounds. Panicking as chances are that it will \
                    never get unblocked.\
                ");

                continue;
            } else if ptr == &FREE {
                return State::Free;
            } else if ptr == &DEAD {
                return State::Dead;
            } else {
                return State::Protect(ptr);
            }
        }
    }
}

/// Create a new hazard reader-writer pair.
///
/// This creates a new hazard pair in blocked state.
pub fn create() -> (Writer, Reader) {
    // Allocate the hazard on the heap.
    let ptr: &'static Hazard = unsafe { &*Box::into_raw(Box::new(Hazard::blocked())) };

    // Construct the values.
    (Writer {
        ptr: ptr,
    }, Reader {
        ptr: ptr,
    })
}

/// An hazard reader.
///
/// This wraps a hazard and provides only ability to read and deallocate it. It is created through
/// the `create()` function.
///
/// The destructor will, for the sake of safety, panic. To deallocate, use `self.destroy()`
/// instead.
pub struct Reader {
    /// The pointer to the heap-allocated hazard.
    ptr: &'static Hazard,
}

impl Reader {
    /// Get the state of the hazard.
    pub fn get(&self) -> State {
        self.ptr.get()
    }

    /// Destroy the hazard.
    ///
    /// # Safety
    ///
    /// This is unsafe as it relies on the writer part being dead and not used anymore. There is
    /// currently no way to express this invariant through the type system, so we must rely on the
    /// caller to ensure that.
    ///
    /// # Panics
    ///
    /// In debug mode, this will panic if the state of the hazard is not "dead".
    pub unsafe fn destroy(self) {
        debug_assert!(self.get() == State::Dead, "Prematurely freeing an active hazard.");

        // Load the pointer and deallocate it.
        Box::from_raw(self.ptr as *const Hazard as *mut Hazard);
        // Ensure that the RAII destructor doesn't kick in and crashes the program.
        mem::forget(self);
    }
}

/// Panic when it is dropped outside `Reader::destroy()`.
///
/// This ought to catch e.g. unwinding.
impl Drop for Reader {
    fn drop(&mut self) {
        panic!("Hazard readers ought to be destroyed manually through the `Reader::destroy()` \
        method.");
    }
}

/// An hazard reader.
///
/// This wraps a hazard and provides only ability to read and deallocate it. It is created through
/// the `create()` function.
///
/// The destructor relocate the hazard to the thread-local cache.
#[derive(Debug)]
pub struct Writer {
    /// The pointer to the heap-allocated hazard.
    ptr: &'static Hazard,
}

impl Writer {
    /// Set the state of this hazard to "dead".
    ///
    /// This will ensure that the hazard won't end up in the thread-local cache, by (eventually)
    /// deleting it globally.
    ///
    /// Generally, this is not recommended, as it means that your hazard cannot be reused.
    pub fn kill(self) {
        // Set the state to dead.
        self.set(State::Dead);
        // Avoid the RAII destructor.
        mem::forget(self);
    }
}

impl ops::Deref for Writer {
    type Target = Hazard;

    fn deref(&self) -> &Hazard {
        self.ptr
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Implementation note: Freeing to local state in the destructor does lead to issues with
        // panicking, which this conditional is supposed to solve. The alternative is to outright
        // set the hazard to state "dead", disregarding whether the thread is panicking or not.
        // This is fairly nice for some purposes, but it makes it necessary to store an `Option` in
        // `Guard`, as one must avoid the hazard from being set to state "dead" after being
        // relocated to the local state. As such, this approach (where the destructor automatically
        // puts the hazard back into the local cache) is nicer. For more information on its
        // alternative, see commit b7047c263cbd614b7c828d68b29d7928be543623.
        if thread::panicking() {
            // If the thread is unwinding, there is no point in putting it back in the thread-local
            // cache. In fact, it might cause problems, if the unwinding tries to garbage collect
            // and the hazard is in blocked state. For this reason, we simply set the state to
            // "dead" and move on.
            self.ptr.set(State::Dead);
        } else {
            // Free the hazard to the thread-local cache. We have to clone the hazard to get around the
            // fact that `drop` takes `&mut self`.
            local::free_hazard(Writer {
                ptr: self.ptr,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ptr, thread};

    #[test]
    fn set_get() {
        let h = Hazard::blocked();
        assert!(h.is_blocked());

        h.set(State::Free);
        assert_eq!(h.get(), State::Free);
        h.set(State::Dead);
        assert_eq!(h.get(), State::Dead);

        let x = 2;

        h.set(State::Protect(&x));
        assert_eq!(h.get(), State::Protect(&x));

        h.set(State::Protect(ptr::null()));
        assert_eq!(h.get(), State::Protect(ptr::null()));
        h.set(State::Protect(0x1 as *const u8));
        assert_eq!(h.get(), State::Protect(0x1 as *const u8));
    }

    #[test]
    fn hazard_pair() {
        let (writer, reader) = create();
        let x = 2;

        writer.set(State::Free);
        assert_eq!(reader.get(), State::Free);
        writer.set(State::Protect(&x));
        assert_eq!(reader.get(), State::Protect(&x));
        writer.kill();
        assert_eq!(reader.get(), State::Dead);

        unsafe {
            reader.destroy();
        }
    }

    #[test]
    fn cross_thread() {
        for _ in 0..64 {
            let (writer, reader) = create();

            thread::spawn(move || {
                writer.set(State::Dead);
            }).join().unwrap();

            assert_eq!(reader.get(), State::Dead);
            unsafe { reader.destroy(); }
        }
    }

    #[test]
    fn drop() {
        for _ in 0..9000 {
            let (writer, reader) = create();
            writer.set(State::Dead);
            unsafe {
                reader.destroy();
            }
        }
    }

    #[cfg(debug_assertions)]
    #[should_panic]
    #[test]
    fn debug_infinite_blockage() {
        let h = Hazard::blocked();
        let _ = h.get();
    }

    /* FIXME: This test is broken as the unwinding calls dtor of `Writer`, which double panics.
        #[cfg(debug_assertions)]
        #[should_panic]
        #[test]
        fn debug_premature_free() {
            let (writer, reader) = create();
            writer.set(State::Free);
            mem::forget(reader);
            unsafe {
                reader.destroy();
            }
        }
    */
}
