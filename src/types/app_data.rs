use std::any::{Any, TypeId};
use std::cell::{Cell, Ref, RefCell, RefMut, UnsafeCell};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::result::Result as StdResult;

use rustc_hash::FxHashMap;

use crate::state::LuaGuard;

use super::MaybeSend;

#[cfg(not(feature = "send"))]
type Container = UnsafeCell<FxHashMap<TypeId, RefCell<Box<dyn Any>>>>;

#[cfg(feature = "send")]
type Container = UnsafeCell<FxHashMap<TypeId, RefCell<Box<dyn Any + Send>>>>;

/// A container for arbitrary data associated with the Lua state.
#[derive(Debug, Default)]
pub struct AppData {
    container: Container,
    borrow: Cell<usize>,
}

impl AppData {
    #[track_caller]
    pub(crate) fn insert<T: MaybeSend + 'static>(&self, data: T) -> Option<T> {
        match self.try_insert(data) {
            Ok(data) => data,
            Err(_) => panic!("cannot mutably borrow app data container"),
        }
    }

    pub(crate) fn try_insert<T: MaybeSend + 'static>(&self, data: T) -> StdResult<Option<T>, T> {
        if self.borrow.get() != 0 {
            return Err(data);
        }
        // SAFETY: we checked that there are no other references to the container
        Ok(unsafe { &mut *self.container.get() }
            .insert(TypeId::of::<T>(), RefCell::new(Box::new(data)))
            .and_then(|data| data.into_inner().downcast::<T>().ok().map(|data| *data)))
    }

    #[track_caller]
    pub(crate) fn borrow<T: 'static>(&self, guard: Option<LuaGuard>) -> Option<AppDataRef<T>> {
        let data = unsafe { &*self.container.get() }
            .get(&TypeId::of::<T>())?
            .borrow();
        self.borrow.set(self.borrow.get() + 1);
        Some(AppDataRef {
            data: Ref::filter_map(data, |data| data.downcast_ref()).ok()?,
            borrow: &self.borrow,
            _guard: guard,
        })
    }

    #[track_caller]
    pub(crate) fn borrow_mut<T: 'static>(
        &self,
        guard: Option<LuaGuard>,
    ) -> Option<AppDataRefMut<T>> {
        let data = unsafe { &*self.container.get() }
            .get(&TypeId::of::<T>())?
            .borrow_mut();
        self.borrow.set(self.borrow.get() + 1);
        Some(AppDataRefMut {
            data: RefMut::filter_map(data, |data| data.downcast_mut()).ok()?,
            borrow: &self.borrow,
            _guard: guard,
        })
    }

    #[track_caller]
    pub(crate) fn remove<T: 'static>(&self) -> Option<T> {
        if self.borrow.get() != 0 {
            panic!("cannot mutably borrow app data container");
        }
        // SAFETY: we checked that there are no other references to the container
        unsafe { &mut *self.container.get() }
            .remove(&TypeId::of::<T>())?
            .into_inner()
            .downcast::<T>()
            .ok()
            .map(|data| *data)
    }
}

/// A wrapper type for an immutably borrowed value from an app data container.
///
/// This type is similar to [`Ref`].
pub struct AppDataRef<'a, T: ?Sized + 'a> {
    data: Ref<'a, T>,
    borrow: &'a Cell<usize>,
    _guard: Option<LuaGuard>,
}

impl<T: ?Sized> Drop for AppDataRef<'_, T> {
    fn drop(&mut self) {
        self.borrow.set(self.borrow.get() - 1);
    }
}

impl<T: ?Sized> Deref for AppDataRef<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for AppDataRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for AppDataRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

/// A wrapper type for a mutably borrowed value from an app data container.
///
/// This type is similar to [`RefMut`].
pub struct AppDataRefMut<'a, T: ?Sized + 'a> {
    data: RefMut<'a, T>,
    borrow: &'a Cell<usize>,
    _guard: Option<LuaGuard>,
}

impl<T: ?Sized> Drop for AppDataRefMut<'_, T> {
    fn drop(&mut self) {
        self.borrow.set(self.borrow.get() - 1);
    }
}

impl<T: ?Sized> Deref for AppDataRefMut<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T: ?Sized> DerefMut for AppDataRefMut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl<T: ?Sized + fmt::Display> fmt::Display for AppDataRefMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for AppDataRefMut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

#[cfg(test)]
mod assertions {
    use super::*;

    #[cfg(not(feature = "send"))]
    static_assertions::assert_not_impl_any!(AppData: Send);
    #[cfg(feature = "send")]
    static_assertions::assert_impl_all!(AppData: Send);

    // Must be !Send
    static_assertions::assert_not_impl_any!(AppDataRef<()>: Send);
    static_assertions::assert_not_impl_any!(AppDataRefMut<()>: Send);
}
