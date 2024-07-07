use std::any::TypeId;
use std::ffi::CStr;
use std::fmt;
use std::hash::Hash;
use std::os::raw::{c_char, c_int, c_void};
use std::string::String as StdString;

#[cfg(feature = "async")]
use std::future::Future;

#[cfg(feature = "serialize")]
use {
    serde::ser::{self, Serialize, Serializer},
    std::result::Result as StdResult,
};

use crate::error::{Error, Result};
use crate::function::Function;
use crate::state::{Lua, LuaGuard};
use crate::string::String;
use crate::table::{Table, TablePairs};
use crate::types::{MaybeSend, SubtypeId, ValueRef};
use crate::util::{check_stack, get_userdata, take_userdata, StackGuard};
use crate::value::{FromLua, FromLuaMulti, IntoLua, IntoLuaMulti, Value};

// Re-export for convenience
pub(crate) use cell::UserDataVariant;
pub use cell::{UserDataRef, UserDataRefMut};
pub use ext::AnyUserDataExt;
pub(crate) use registry::UserDataProxy;
pub use registry::UserDataRegistry;

#[cfg(feature = "lua54")]
pub(crate) const USER_VALUE_MAXSLOT: usize = 8;

/// Kinds of metamethods that can be overridden.
///
/// Currently, this mechanism does not allow overriding the `__gc` metamethod, since there is
/// generally no need to do so: [`UserData`] implementors can instead just implement `Drop`.
///
/// [`UserData`]: crate::UserData
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MetaMethod {
    /// The `+` operator.
    Add,
    /// The `-` operator.
    Sub,
    /// The `*` operator.
    Mul,
    /// The `/` operator.
    Div,
    /// The `%` operator.
    Mod,
    /// The `^` operator.
    Pow,
    /// The unary minus (`-`) operator.
    Unm,
    /// The floor division (//) operator.
    /// Requires `feature = "lua54/lua53/luau"`
    #[cfg(any(feature = "lua54", feature = "lua53", feature = "luau"))]
    IDiv,
    /// The bitwise AND (&) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53"))]
    BAnd,
    /// The bitwise OR (|) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53"))]
    BOr,
    /// The bitwise XOR (binary ~) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53"))]
    BXor,
    /// The bitwise NOT (unary ~) operator.
    /// Requires `feature = "lua54/lua53"`
    #[cfg(any(feature = "lua54", feature = "lua53"))]
    BNot,
    /// The bitwise left shift (<<) operator.
    #[cfg(any(feature = "lua54", feature = "lua53"))]
    Shl,
    /// The bitwise right shift (>>) operator.
    #[cfg(any(feature = "lua54", feature = "lua53"))]
    Shr,
    /// The string concatenation operator `..`.
    Concat,
    /// The length operator `#`.
    Len,
    /// The `==` operator.
    Eq,
    /// The `<` operator.
    Lt,
    /// The `<=` operator.
    Le,
    /// Index access `obj[key]`.
    Index,
    /// Index write access `obj[key] = value`.
    NewIndex,
    /// The call "operator" `obj(arg1, args2, ...)`.
    Call,
    /// The `__tostring` metamethod.
    ///
    /// This is not an operator, but will be called by methods such as `tostring` and `print`.
    ToString,
    /// The `__pairs` metamethod.
    ///
    /// This is not an operator, but it will be called by the built-in `pairs` function.
    ///
    /// Requires `feature = "lua54/lua53/lua52"`
    #[cfg(any(
        feature = "lua54",
        feature = "lua53",
        feature = "lua52",
        feature = "luajit52",
    ))]
    Pairs,
    /// The `__ipairs` metamethod.
    ///
    /// This is not an operator, but it will be called by the built-in [`ipairs`] function.
    ///
    /// Requires `feature = "lua52"`
    ///
    /// [`ipairs`]: https://www.lua.org/manual/5.2/manual.html#pdf-ipairs
    #[cfg(any(feature = "lua52", feature = "luajit52", doc))]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "lua52", feature = "luajit52"))))]
    IPairs,
    /// The `__iter` metamethod.
    ///
    /// Executed before the iteration begins, and should return an iterator function like `next`
    /// (or a custom one).
    ///
    /// Requires `feature = "lua"`
    #[cfg(any(feature = "luau", doc))]
    #[cfg_attr(docsrs, doc(cfg(feature = "luau")))]
    Iter,
    /// The `__close` metamethod.
    ///
    /// Executed when a variable, that marked as to-be-closed, goes out of scope.
    ///
    /// More information about to-be-closed variabled can be found in the Lua 5.4
    /// [documentation][lua_doc].
    ///
    /// Requires `feature = "lua54"`
    ///
    /// [lua_doc]: https://www.lua.org/manual/5.4/manual.html#3.3.8
    #[cfg(feature = "lua54")]
    #[cfg_attr(docsrs, doc(cfg(feature = "lua54")))]
    Close,
    /// The `__name`/`__type` metafield.
    ///
    /// This is not a function, but it's value can be used by `tostring` and `typeof` built-in functions.
    #[doc(hidden)]
    Type,
}

impl PartialEq<MetaMethod> for &str {
    fn eq(&self, other: &MetaMethod) -> bool {
        *self == other.name()
    }
}

impl PartialEq<MetaMethod> for StdString {
    fn eq(&self, other: &MetaMethod) -> bool {
        self == other.name()
    }
}

impl fmt::Display for MetaMethod {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{}", self.name())
    }
}

impl MetaMethod {
    /// Returns Lua metamethod name, usually prefixed by two underscores.
    pub const fn name(self) -> &'static str {
        match self {
            MetaMethod::Add => "__add",
            MetaMethod::Sub => "__sub",
            MetaMethod::Mul => "__mul",
            MetaMethod::Div => "__div",
            MetaMethod::Mod => "__mod",
            MetaMethod::Pow => "__pow",
            MetaMethod::Unm => "__unm",

            #[cfg(any(feature = "lua54", feature = "lua53", feature = "luau"))]
            MetaMethod::IDiv => "__idiv",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BAnd => "__band",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BOr => "__bor",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BXor => "__bxor",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::BNot => "__bnot",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::Shl => "__shl",
            #[cfg(any(feature = "lua54", feature = "lua53"))]
            MetaMethod::Shr => "__shr",

            MetaMethod::Concat => "__concat",
            MetaMethod::Len => "__len",
            MetaMethod::Eq => "__eq",
            MetaMethod::Lt => "__lt",
            MetaMethod::Le => "__le",
            MetaMethod::Index => "__index",
            MetaMethod::NewIndex => "__newindex",
            MetaMethod::Call => "__call",
            MetaMethod::ToString => "__tostring",

            #[cfg(any(
                feature = "lua54",
                feature = "lua53",
                feature = "lua52",
                feature = "luajit52"
            ))]
            MetaMethod::Pairs => "__pairs",
            #[cfg(any(feature = "lua52", feature = "luajit52"))]
            MetaMethod::IPairs => "__ipairs",
            #[cfg(feature = "luau")]
            MetaMethod::Iter => "__iter",

            #[cfg(feature = "lua54")]
            MetaMethod::Close => "__close",

            #[rustfmt::skip]
            MetaMethod::Type => if cfg!(feature = "luau") { "__type" } else { "__name" },
        }
    }

    pub(crate) const fn as_cstr(self) -> &'static CStr {
        match self {
            #[rustfmt::skip]
            MetaMethod::Type => unsafe {
                CStr::from_bytes_with_nul_unchecked(if cfg!(feature = "luau") { b"__type\0" } else { b"__name\0" })
            },
            _ => unreachable!(),
        }
    }

    pub(crate) fn validate(name: &str) -> Result<&str> {
        match name {
            "__gc" => Err(Error::MetaMethodRestricted(name.to_string())),
            "__metatable" => Err(Error::MetaMethodRestricted(name.to_string())),
            _ if name.starts_with("__mlua") => Err(Error::MetaMethodRestricted(name.to_string())),
            name => Ok(name),
        }
    }
}

impl AsRef<str> for MetaMethod {
    fn as_ref(&self) -> &str {
        self.name()
    }
}

/// Method registry for [`UserData`] implementors.
///
/// [`UserData`]: crate::UserData
pub trait UserDataMethods<'a, T> {
    /// Add a regular method which accepts a `&T` as the first parameter.
    ///
    /// Regular methods are implemented by overriding the `__index` metamethod and returning the
    /// accessed method. This allows them to be used with the expected `userdata:method()` syntax.
    ///
    /// If `add_meta_method` is used to set the `__index` metamethod, the `__index` metamethod will
    /// be used as a fall-back if no regular method is found.
    fn add_method<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: Fn(&'a Lua, &T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add a regular method which accepts a `&mut T` as the first parameter.
    ///
    /// Refer to [`add_method`] for more information about the implementation.
    ///
    /// [`add_method`]: #method.add_method
    fn add_method_mut<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: FnMut(&'a Lua, &mut T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add an async method which accepts a `&T` as the first parameter and returns Future.
    ///
    /// Refer to [`add_method`] for more information about the implementation.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_method`]: #method.add_method
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_method<M, A, MR, R>(&mut self, name: impl ToString, method: M)
    where
        T: 'static,
        M: Fn(&'a Lua, &'a T, A) -> MR + MaybeSend + 'static,
        A: FromLuaMulti,
        MR: Future<Output = Result<R>> + 'a,
        R: IntoLuaMulti;

    /// Add an async method which accepts a `&mut T` as the first parameter and returns Future.
    ///
    /// Refer to [`add_method`] for more information about the implementation.
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_method`]: #method.add_method
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_method_mut<M, A, MR, R>(&mut self, name: impl ToString, method: M)
    where
        T: 'static,
        M: Fn(&'a Lua, &'a mut T, A) -> MR + MaybeSend + 'static,
        A: FromLuaMulti,
        MR: Future<Output = Result<R>> + 'a,
        R: IntoLuaMulti;

    /// Add a regular method as a function which accepts generic arguments, the first argument will
    /// be a [`AnyUserData`] of type `T` if the method is called with Lua method syntax:
    /// `my_userdata:my_method(arg1, arg2)`, or it is passed in as the first argument:
    /// `my_userdata.my_method(my_userdata, arg1, arg2)`.
    ///
    /// Prefer to use [`add_method`] or [`add_method_mut`] as they are easier to use.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`add_method`]: #method.add_method
    /// [`add_method_mut`]: #method.add_method_mut
    fn add_function<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&'a Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add a regular method as a mutable function which accepts generic arguments.
    ///
    /// This is a version of [`add_function`] that accepts a FnMut argument.
    ///
    /// [`add_function`]: #method.add_function
    fn add_function_mut<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: FnMut(&'a Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add a regular method as an async function which accepts generic arguments
    /// and returns Future.
    ///
    /// This is an async version of [`add_function`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_function`]: #method.add_function
    #[cfg(feature = "async")]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_function<F, A, FR, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&'a Lua, A) -> FR + MaybeSend + 'static,
        A: FromLuaMulti,
        FR: Future<Output = Result<R>> + 'a,
        R: IntoLuaMulti;

    /// Add a metamethod which accepts a `&T` as the first parameter.
    ///
    /// # Note
    ///
    /// This can cause an error with certain binary metamethods that can trigger if only the right
    /// side has a metatable. To prevent this, use [`add_meta_function`].
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    fn add_meta_method<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: Fn(&'a Lua, &T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add a metamethod as a function which accepts a `&mut T` as the first parameter.
    ///
    /// # Note
    ///
    /// This can cause an error with certain binary metamethods that can trigger if only the right
    /// side has a metatable. To prevent this, use [`add_meta_function`].
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    fn add_meta_method_mut<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: FnMut(&'a Lua, &mut T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add an async metamethod which accepts a `&T` as the first parameter and returns Future.
    ///
    /// This is an async version of [`add_meta_method`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_meta_method`]: #method.add_meta_method
    #[cfg(all(feature = "async", not(any(feature = "lua51", feature = "luau"))))]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_meta_method<M, A, MR, R>(&mut self, name: impl ToString, method: M)
    where
        T: 'static,
        M: Fn(&'a Lua, &'a T, A) -> MR + MaybeSend + 'static,
        A: FromLuaMulti,
        MR: Future<Output = Result<R>> + 'a,
        R: IntoLuaMulti;

    /// Add an async metamethod which accepts a `&mut T` as the first parameter and returns Future.
    ///
    /// This is an async version of [`add_meta_method_mut`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_meta_method_mut`]: #method.add_meta_method_mut
    #[cfg(all(feature = "async", not(any(feature = "lua51", feature = "luau"))))]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_meta_method_mut<M, A, MR, R>(&mut self, name: impl ToString, method: M)
    where
        T: 'static,
        M: Fn(&'a Lua, &'a mut T, A) -> MR + MaybeSend + 'static,
        A: FromLuaMulti,
        MR: Future<Output = Result<R>> + 'a,
        R: IntoLuaMulti;

    /// Add a metamethod which accepts generic arguments.
    ///
    /// Metamethods for binary operators can be triggered if either the left or right argument to
    /// the binary operator has a metatable, so the first argument here is not necessarily a
    /// userdata of type `T`.
    fn add_meta_function<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&'a Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add a metamethod as a mutable function which accepts generic arguments.
    ///
    /// This is a version of [`add_meta_function`] that accepts a FnMut argument.
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    fn add_meta_function_mut<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: FnMut(&'a Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti;

    /// Add a metamethod which accepts generic arguments and returns Future.
    ///
    /// This is an async version of [`add_meta_function`].
    ///
    /// Requires `feature = "async"`
    ///
    /// [`add_meta_function`]: #method.add_meta_function
    #[cfg(all(feature = "async", not(any(feature = "lua51", feature = "luau"))))]
    #[cfg_attr(docsrs, doc(cfg(feature = "async")))]
    fn add_async_meta_function<F, A, FR, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&'a Lua, A) -> FR + MaybeSend + 'static,
        A: FromLuaMulti,
        FR: Future<Output = Result<R>> + 'a,
        R: IntoLuaMulti;
}

/// Field registry for [`UserData`] implementors.
///
/// [`UserData`]: crate::UserData
pub trait UserDataFields<'a, T> {
    /// Add a static field to the `UserData`.
    ///
    /// Static fields are implemented by updating the `__index` metamethod and returning the
    /// accessed field. This allows them to be used with the expected `userdata.field` syntax.
    ///
    /// Static fields are usually shared between all instances of the `UserData` of the same type.
    ///
    /// If `add_meta_method` is used to set the `__index` metamethod, it will
    /// be used as a fall-back if no regular field or method are found.
    fn add_field<V>(&mut self, name: impl ToString, value: V)
    where
        V: IntoLua + Clone + 'static;

    /// Add a regular field getter as a method which accepts a `&T` as the parameter.
    ///
    /// Regular field getters are implemented by overriding the `__index` metamethod and returning the
    /// accessed field. This allows them to be used with the expected `userdata.field` syntax.
    ///
    /// If `add_meta_method` is used to set the `__index` metamethod, the `__index` metamethod will
    /// be used as a fall-back if no regular field or method are found.
    fn add_field_method_get<M, R>(&mut self, name: impl ToString, method: M)
    where
        M: Fn(&'a Lua, &T) -> Result<R> + MaybeSend + 'static,
        R: IntoLua;

    /// Add a regular field setter as a method which accepts a `&mut T` as the first parameter.
    ///
    /// Regular field setters are implemented by overriding the `__newindex` metamethod and setting the
    /// accessed field. This allows them to be used with the expected `userdata.field = value` syntax.
    ///
    /// If `add_meta_method` is used to set the `__newindex` metamethod, the `__newindex` metamethod will
    /// be used as a fall-back if no regular field is found.
    fn add_field_method_set<M, A>(&mut self, name: impl ToString, method: M)
    where
        M: FnMut(&'a Lua, &mut T, A) -> Result<()> + MaybeSend + 'static,
        A: FromLua;

    /// Add a regular field getter as a function which accepts a generic [`AnyUserData`] of type `T`
    /// argument.
    ///
    /// Prefer to use [`add_field_method_get`] as it is easier to use.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`add_field_method_get`]: #method.add_field_method_get
    fn add_field_function_get<F, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&'a Lua, AnyUserData) -> Result<R> + MaybeSend + 'static,
        R: IntoLua;

    /// Add a regular field setter as a function which accepts a generic [`AnyUserData`] of type `T`
    /// first argument.
    ///
    /// Prefer to use [`add_field_method_set`] as it is easier to use.
    ///
    /// [`AnyUserData`]: crate::AnyUserData
    /// [`add_field_method_set`]: #method.add_field_method_set
    fn add_field_function_set<F, A>(&mut self, name: impl ToString, function: F)
    where
        F: FnMut(&'a Lua, AnyUserData, A) -> Result<()> + MaybeSend + 'static,
        A: FromLua;

    /// Add a metatable field.
    ///
    /// This will initialize the metatable field with `value` on `UserData` creation.
    ///
    /// # Note
    ///
    /// `mlua` will trigger an error on an attempt to define a protected metamethod,
    /// like `__gc` or `__metatable`.
    fn add_meta_field<V>(&mut self, name: impl ToString, value: V)
    where
        V: IntoLua + Clone + 'static;

    /// Add a metatable field computed from `f`.
    ///
    /// This will initialize the metatable field from `f` on `UserData` creation.
    ///
    /// # Note
    ///
    /// `mlua` will trigger an error on an attempt to define a protected metamethod,
    /// like `__gc` or `__metatable`.
    fn add_meta_field_with<F, R>(&mut self, name: impl ToString, f: F)
    where
        F: Fn(&'a Lua) -> Result<R> + MaybeSend + 'static,
        R: IntoLua;
}

/// Trait for custom userdata types.
///
/// By implementing this trait, a struct becomes eligible for use inside Lua code.
/// Implementation of [`IntoLua`] is automatically provided, [`FromLua`] needs to be implemented
/// manually.
///
///
/// # Examples
///
/// ```
/// # use mlua::{Lua, Result, UserData};
/// # fn main() -> Result<()> {
/// # let lua = Lua::new();
/// struct MyUserData;
///
/// impl UserData for MyUserData {}
///
/// // `MyUserData` now implements `IntoLua`:
/// lua.globals().set("myobject", MyUserData)?;
///
/// lua.load("assert(type(myobject) == 'userdata')").exec()?;
/// # Ok(())
/// # }
/// ```
///
/// Custom fields, methods and operators can be provided by implementing `add_fields` or `add_methods`
/// (refer to [`UserDataFields`] and [`UserDataMethods`] for more information):
///
/// ```
/// # use mlua::{Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods};
/// # fn main() -> Result<()> {
/// # let lua = Lua::new();
/// struct MyUserData(i32);
///
/// impl UserData for MyUserData {
///     fn add_fields<'a, F: UserDataFields<'a, Self>>(fields: &mut F) {
///         fields.add_field_method_get("val", |_, this| Ok(this.0));
///     }
///
///     fn add_methods<'a, M: UserDataMethods<'a, Self>>(methods: &mut M) {
///         methods.add_method_mut("add", |_, this, value: i32| {
///             this.0 += value;
///             Ok(())
///         });
///
///         methods.add_meta_method(MetaMethod::Add, |_, this, value: i32| {
///             Ok(this.0 + value)
///         });
///     }
/// }
///
/// lua.globals().set("myobject", MyUserData(123))?;
///
/// lua.load(r#"
///     assert(myobject.val == 123)
///     myobject:add(7)
///     assert(myobject.val == 130)
///     assert(myobject + 10 == 140)
/// "#).exec()?;
/// # Ok(())
/// # }
/// ```
///
/// [`IntoLua`]: crate::IntoLua
/// [`FromLua`]: crate::FromLua
/// [`UserDataFields`]: crate::UserDataFields
/// [`UserDataMethods`]: crate::UserDataMethods
pub trait UserData: Sized {
    /// Adds custom fields specific to this userdata.
    #[allow(unused_variables)]
    fn add_fields<'a, F: UserDataFields<'a, Self>>(fields: &mut F) {}

    /// Adds custom methods and operators specific to this userdata.
    #[allow(unused_variables)]
    fn add_methods<'a, M: UserDataMethods<'a, Self>>(methods: &mut M) {}

    /// Registers this type for use in Lua.
    ///
    /// This method is responsible for calling `add_fields` and `add_methods` on the provided
    /// [`UserDataRegistry`].
    fn register(registry: &mut UserDataRegistry<Self>) {
        Self::add_fields(registry);
        Self::add_methods(registry);
    }
}

/// Handle to an internal Lua userdata for any type that implements [`UserData`].
///
/// Similar to `std::any::Any`, this provides an interface for dynamic type checking via the [`is`]
/// and [`borrow`] methods.
///
/// Internally, instances are stored in a `RefCell`, to best match the mutable semantics of the Lua
/// language.
///
/// # Note
///
/// This API should only be used when necessary. Implementing [`UserData`] already allows defining
/// methods which check the type and acquire a borrow behind the scenes.
///
/// [`UserData`]: crate::UserData
/// [`is`]: crate::AnyUserData::is
/// [`borrow`]: crate::AnyUserData::borrow
#[derive(Clone, Debug)]
pub struct AnyUserData(pub(crate) ValueRef, pub(crate) SubtypeId);

impl AnyUserData {
    /// Checks whether the type of this userdata is `T`.
    pub fn is<T: 'static>(&self) -> bool {
        self.inspect::<T, _, _>(|_, _| Ok(())).is_ok()
    }

    /// Borrow this userdata immutably if it is of type `T`.
    ///
    /// # Errors
    ///
    /// Returns a `UserDataBorrowError` if the userdata is already mutably borrowed. Returns a
    /// `UserDataTypeMismatch` if the userdata is not of type `T`.
    #[inline]
    pub fn borrow<T: 'static>(&self) -> Result<UserDataRef<T>> {
        self.inspect(|variant, guard| variant.try_make_ref(guard))
    }

    /// Borrow this userdata mutably if it is of type `T`.
    ///
    /// # Errors
    ///
    /// Returns a `UserDataBorrowMutError` if the userdata cannot be mutably borrowed.
    /// Returns a `UserDataTypeMismatch` if the userdata is not of type `T`.
    #[inline]
    pub fn borrow_mut<T: 'static>(&self) -> Result<UserDataRefMut<T>> {
        self.inspect(|variant, guard| variant.try_make_mut_ref(guard))
    }

    /// Takes the value out of this userdata.
    /// Sets the special "destructed" metatable that prevents any further operations with this userdata.
    ///
    /// Keeps associated user values unchanged (they will be collected by Lua's GC).
    pub fn take<T: 'static>(&self) -> Result<T> {
        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 2)?;

            let type_id = lua.push_userdata_ref(&self.0)?;
            match type_id {
                Some(type_id) if type_id == TypeId::of::<T>() => {
                    // Try to borrow userdata exclusively
                    let _ = (*get_userdata::<UserDataVariant<T>>(state, -1)).try_borrow_mut()?;
                    take_userdata::<UserDataVariant<T>>(state).into_inner()
                }
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }

    /// Sets an associated value to this `AnyUserData`.
    ///
    /// The value may be any Lua value whatsoever, and can be retrieved with [`user_value`].
    ///
    /// This is the same as calling [`set_nth_user_value`] with `n` set to 1.
    ///
    /// [`user_value`]: #method.user_value
    /// [`set_nth_user_value`]: #method.set_nth_user_value
    #[inline]
    pub fn set_user_value<V: IntoLua>(&self, v: V) -> Result<()> {
        self.set_nth_user_value(1, v)
    }

    /// Returns an associated value set by [`set_user_value`].
    ///
    /// This is the same as calling [`nth_user_value`] with `n` set to 1.
    ///
    /// [`set_user_value`]: #method.set_user_value
    /// [`nth_user_value`]: #method.nth_user_value
    #[inline]
    pub fn user_value<V: FromLua>(&self) -> Result<V> {
        self.nth_user_value(1)
    }

    #[doc(hidden)]
    #[deprecated(since = "0.9.0", note = "please use `user_value` instead")]
    pub fn get_user_value<V: FromLua>(&self) -> Result<V> {
        self.nth_user_value(1)
    }

    /// Sets an associated `n`th value to this `AnyUserData`.
    ///
    /// The value may be any Lua value whatsoever, and can be retrieved with [`nth_user_value`].
    /// `n` starts from 1 and can be up to 65535.
    ///
    /// This is supported for all Lua versions.
    /// In Lua 5.4 first 7 elements are stored in a most efficient way.
    /// For other Lua versions this functionality is provided using a wrapping table.
    ///
    /// [`nth_user_value`]: #method.nth_user_value
    pub fn set_nth_user_value<V: IntoLua>(&self, n: usize, v: V) -> Result<()> {
        if n < 1 || n > u16::MAX as usize {
            return Err(Error::runtime("user value index out of bounds"));
        }

        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 5)?;

            lua.push_userdata_ref(&self.0)?;
            lua.push(v)?;

            #[cfg(feature = "lua54")]
            if n < USER_VALUE_MAXSLOT {
                ffi::lua_setiuservalue(state, -2, n as c_int);
                return Ok(());
            }

            // Multiple (extra) user values are emulated by storing them in a table
            protect_lua!(state, 2, 0, |state| {
                if getuservalue_table(state, -2) != ffi::LUA_TTABLE {
                    // Create a new table to use as uservalue
                    ffi::lua_pop(state, 1);
                    ffi::lua_newtable(state);
                    ffi::lua_pushvalue(state, -1);

                    #[cfg(feature = "lua54")]
                    ffi::lua_setiuservalue(state, -4, USER_VALUE_MAXSLOT as c_int);
                    #[cfg(not(feature = "lua54"))]
                    ffi::lua_setuservalue(state, -4);
                }
                ffi::lua_pushvalue(state, -2);
                #[cfg(feature = "lua54")]
                ffi::lua_rawseti(state, -2, (n - USER_VALUE_MAXSLOT + 1) as ffi::lua_Integer);
                #[cfg(not(feature = "lua54"))]
                ffi::lua_rawseti(state, -2, n as ffi::lua_Integer);
            })?;

            Ok(())
        }
    }

    /// Returns an associated `n`th value set by [`set_nth_user_value`].
    ///
    /// `n` starts from 1 and can be up to 65535.
    ///
    /// This is supported for all Lua versions.
    /// In Lua 5.4 first 7 elements are stored in a most efficient way.
    /// For other Lua versions this functionality is provided using a wrapping table.
    ///
    /// [`set_nth_user_value`]: #method.set_nth_user_value
    pub fn nth_user_value<V: FromLua>(&self, n: usize) -> Result<V> {
        if n < 1 || n > u16::MAX as usize {
            return Err(Error::runtime("user value index out of bounds"));
        }

        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 4)?;

            lua.push_userdata_ref(&self.0)?;

            #[cfg(feature = "lua54")]
            if n < USER_VALUE_MAXSLOT {
                ffi::lua_getiuservalue(state, -1, n as c_int);
                return V::from_lua(lua.pop_value(), lua.lua());
            }

            // Multiple (extra) user values are emulated by storing them in a table
            protect_lua!(state, 1, 1, |state| {
                if getuservalue_table(state, -1) != ffi::LUA_TTABLE {
                    ffi::lua_pushnil(state);
                    return;
                }
                #[cfg(feature = "lua54")]
                ffi::lua_rawgeti(state, -1, (n - USER_VALUE_MAXSLOT + 1) as ffi::lua_Integer);
                #[cfg(not(feature = "lua54"))]
                ffi::lua_rawgeti(state, -1, n as ffi::lua_Integer);
            })?;

            V::from_lua(lua.pop_value(), lua.lua())
        }
    }

    #[doc(hidden)]
    #[deprecated(since = "0.9.0", note = "please use `nth_user_value` instead")]
    pub fn get_nth_user_value<V: FromLua>(&self, n: usize) -> Result<V> {
        self.nth_user_value(n)
    }

    /// Sets an associated value to this `AnyUserData` by name.
    ///
    /// The value can be retrieved with [`named_user_value`].
    ///
    /// [`named_user_value`]: #method.named_user_value
    pub fn set_named_user_value<V: IntoLua>(&self, name: &str, v: V) -> Result<()> {
        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 5)?;

            lua.push_userdata_ref(&self.0)?;
            lua.push(v)?;

            // Multiple (extra) user values are emulated by storing them in a table
            protect_lua!(state, 2, 0, |state| {
                if getuservalue_table(state, -2) != ffi::LUA_TTABLE {
                    // Create a new table to use as uservalue
                    ffi::lua_pop(state, 1);
                    ffi::lua_newtable(state);
                    ffi::lua_pushvalue(state, -1);

                    #[cfg(feature = "lua54")]
                    ffi::lua_setiuservalue(state, -4, USER_VALUE_MAXSLOT as c_int);
                    #[cfg(not(feature = "lua54"))]
                    ffi::lua_setuservalue(state, -4);
                }
                ffi::lua_pushlstring(state, name.as_ptr() as *const c_char, name.len());
                ffi::lua_pushvalue(state, -3);
                ffi::lua_rawset(state, -3);
            })?;

            Ok(())
        }
    }

    /// Returns an associated value by name set by [`set_named_user_value`].
    ///
    /// [`set_named_user_value`]: #method.set_named_user_value
    pub fn named_user_value<V: FromLua>(&self, name: &str) -> Result<V> {
        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 4)?;

            lua.push_userdata_ref(&self.0)?;

            // Multiple (extra) user values are emulated by storing them in a table
            protect_lua!(state, 1, 1, |state| {
                if getuservalue_table(state, -1) != ffi::LUA_TTABLE {
                    ffi::lua_pushnil(state);
                    return;
                }
                ffi::lua_pushlstring(state, name.as_ptr() as *const c_char, name.len());
                ffi::lua_rawget(state, -2);
            })?;

            V::from_lua(lua.pop_value(), lua.lua())
        }
    }

    #[doc(hidden)]
    #[deprecated(since = "0.9.0", note = "please use `named_user_value` instead")]
    pub fn get_named_user_value<V: FromLua>(&self, name: &str) -> Result<V> {
        self.named_user_value(name)
    }

    /// Returns a metatable of this `UserData`.
    ///
    /// Returned [`UserDataMetatable`] object wraps the original metatable and
    /// provides safe access to its methods.
    ///
    /// For `T: 'static` returned metatable is shared among all instances of type `T`.
    ///
    /// [`UserDataMetatable`]: crate::UserDataMetatable
    #[inline]
    pub fn get_metatable(&self) -> Result<UserDataMetatable> {
        self.get_raw_metatable().map(UserDataMetatable)
    }

    fn get_raw_metatable(&self) -> Result<Table> {
        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 3)?;

            lua.push_userdata_ref(&self.0)?;
            ffi::lua_getmetatable(state, -1); // Checked that non-empty on the previous call
            Ok(Table(lua.pop_ref()))
        }
    }

    /// Converts this userdata to a generic C pointer.
    ///
    /// There is no way to convert the pointer back to its original value.
    ///
    /// Typically this function is used only for hashing and debug information.
    #[inline]
    pub fn to_pointer(&self) -> *const c_void {
        self.0.to_pointer()
    }

    #[cfg(feature = "async")]
    #[inline]
    pub(crate) fn type_id(&self) -> Result<Option<TypeId>> {
        let lua = self.0.lua.lock();
        unsafe { lua.get_userdata_ref_type_id(&self.0) }
    }

    /// Returns a type name of this `UserData` (from a metatable field).
    pub(crate) fn type_name(&self) -> Result<Option<StdString>> {
        match self.1 {
            SubtypeId::None => {}
            #[cfg(feature = "luau")]
            SubtypeId::Buffer => return Ok(Some("buffer".to_owned())),
            #[cfg(feature = "luajit")]
            SubtypeId::CData => return Ok(Some("cdata".to_owned())),
        }

        let lua = self.0.lua.lock();
        let state = lua.state();
        unsafe {
            let _sg = StackGuard::new(state);
            check_stack(state, 3)?;

            lua.push_userdata_ref(&self.0)?;
            let protect = !lua.unlikely_memory_error();
            let name_type = if protect {
                protect_lua!(state, 1, 1, |state| {
                    ffi::luaL_getmetafield(state, -1, MetaMethod::Type.as_cstr().as_ptr())
                })?
            } else {
                ffi::luaL_getmetafield(state, -1, MetaMethod::Type.as_cstr().as_ptr())
            };
            match name_type {
                ffi::LUA_TSTRING => Ok(Some(String(lua.pop_ref()).to_str()?.to_owned())),
                _ => Ok(None),
            }
        }
    }

    pub(crate) fn equals<T: AsRef<Self>>(&self, other: T) -> Result<bool> {
        let other = other.as_ref();
        // Uses lua_rawequal() under the hood
        if self == other {
            return Ok(true);
        }

        let mt = self.get_raw_metatable()?;
        if mt != other.get_raw_metatable()? {
            return Ok(false);
        }

        if mt.contains_key("__eq")? {
            return mt.get::<_, Function>("__eq")?.call((self, other));
        }

        Ok(false)
    }

    /// Returns `true` if this `AnyUserData` is serializable (eg. was created using `create_ser_userdata`).
    #[cfg(feature = "serialize")]
    pub(crate) fn is_serializable(&self) -> bool {
        let lua = self.0.lua.lock();
        let is_serializable = || unsafe {
            // Userdata must be registered and not destructed
            let _ = lua.get_userdata_ref_type_id(&self.0)?;

            let ud = &*get_userdata::<UserDataVariant<()>>(lua.ref_thread(), self.0.index);
            match ud {
                UserDataVariant::Serializable(..) => Result::Ok(true),
                _ => Result::Ok(false),
            }
        };
        is_serializable().unwrap_or(false)
    }

    pub(crate) fn inspect<'a, T, F, R>(&'a self, func: F) -> Result<R>
    where
        T: 'static,
        F: FnOnce(&'a UserDataVariant<T>, LuaGuard) -> Result<R>,
    {
        let lua = self.0.lua.lock();
        unsafe {
            let type_id = lua.get_userdata_ref_type_id(&self.0)?;
            match type_id {
                Some(type_id) if type_id == TypeId::of::<T>() => {
                    let ref_thread = lua.ref_thread();
                    let ud = get_userdata::<UserDataVariant<T>>(ref_thread, self.0.index);
                    func(&*ud, lua)
                }
                _ => Err(Error::UserDataTypeMismatch),
            }
        }
    }
}

impl PartialEq for AnyUserData {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl AsRef<AnyUserData> for AnyUserData {
    #[inline]
    fn as_ref(&self) -> &Self {
        self
    }
}

unsafe fn getuservalue_table(state: *mut ffi::lua_State, idx: c_int) -> c_int {
    #[cfg(feature = "lua54")]
    return ffi::lua_getiuservalue(state, idx, USER_VALUE_MAXSLOT as c_int);
    #[cfg(not(feature = "lua54"))]
    return ffi::lua_getuservalue(state, idx);
}

/// Handle to a `UserData` metatable.
#[derive(Clone, Debug)]
pub struct UserDataMetatable(pub(crate) Table);

impl UserDataMetatable {
    /// Gets the value associated to `key` from the metatable.
    ///
    /// If no value is associated to `key`, returns the `Nil` value.
    /// Access to restricted metamethods such as `__gc` or `__metatable` will cause an error.
    pub fn get<V: FromLua>(&self, key: impl AsRef<str>) -> Result<V> {
        self.0.raw_get(MetaMethod::validate(key.as_ref())?)
    }

    /// Sets a key-value pair in the metatable.
    ///
    /// If the value is `Nil`, this will effectively remove the `key`.
    /// Access to restricted metamethods such as `__gc` or `__metatable` will cause an error.
    /// Setting `__index` or `__newindex` metamethods is also restricted because their values are cached
    /// for `mlua` internal usage.
    pub fn set<V: IntoLua>(&self, key: impl AsRef<str>, value: V) -> Result<()> {
        let key = MetaMethod::validate(key.as_ref())?;
        // `__index` and `__newindex` cannot be changed in runtime, because values are cached
        if key == MetaMethod::Index || key == MetaMethod::NewIndex {
            return Err(Error::MetaMethodRestricted(key.to_string()));
        }
        self.0.raw_set(key, value)
    }

    /// Checks whether the metatable contains a non-nil value for `key`.
    pub fn contains(&self, key: impl AsRef<str>) -> Result<bool> {
        self.0.contains_key(MetaMethod::validate(key.as_ref())?)
    }

    /// Consumes this metatable and returns an iterator over the pairs of the metatable.
    ///
    /// The pairs are wrapped in a [`Result`], since they are lazily converted to `V` type.
    ///
    /// [`Result`]: crate::Result
    pub fn pairs<V: FromLua>(self) -> UserDataMetatablePairs<V> {
        UserDataMetatablePairs(self.0.pairs())
    }
}

/// An iterator over the pairs of a [`UserData`] metatable.
///
/// It skips restricted metamethods, such as `__gc` or `__metatable`.
///
/// This struct is created by the [`UserDataMetatable::pairs`] method.
///
/// [`UserData`]: crate::UserData
/// [`UserDataMetatable::pairs`]: crate::UserDataMetatable::method.pairs
pub struct UserDataMetatablePairs<V>(TablePairs<StdString, V>);

impl<V> Iterator for UserDataMetatablePairs<V>
where
    V: FromLua,
{
    type Item = Result<(StdString, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.0.next()? {
                Ok((key, value)) => {
                    // Skip restricted metamethods
                    if MetaMethod::validate(&key).is_ok() {
                        break Some(Ok((key, value)));
                    }
                }
                Err(e) => break Some(Err(e)),
            }
        }
    }
}

#[cfg(feature = "serialize")]
impl Serialize for AnyUserData {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let lua = self.0.lua.lock();

        // Special case for Luau buffer type
        #[cfg(feature = "luau")]
        if self.1 == SubtypeId::Buffer {
            let buf = unsafe {
                let mut size = 0usize;
                let buf = ffi::lua_tobuffer(lua.ref_thread(), self.0.index, &mut size);
                mlua_assert!(!buf.is_null(), "invalid Luau buffer");
                std::slice::from_raw_parts(buf as *const u8, size)
            };
            return serializer.serialize_bytes(buf);
        }

        unsafe {
            let _ = lua
                .get_userdata_ref_type_id(&self.0)
                .map_err(ser::Error::custom)?;
            let ud = &*get_userdata::<UserDataVariant<()>>(lua.ref_thread(), self.0.index);
            ud.serialize(serializer)
        }
    }
}

pub(crate) struct WrappedUserdata<F: FnOnce(&Lua) -> Result<AnyUserData>>(F);

impl AnyUserData {
    /// Wraps any Rust type, returning an opaque type that implements [`IntoLua`] trait.
    ///
    /// This function uses [`Lua::create_any_userdata()`] under the hood.
    pub fn wrap<T: MaybeSend + 'static>(data: T) -> impl IntoLua {
        WrappedUserdata(move |lua| lua.create_any_userdata(data))
    }
}

impl<F> IntoLua for WrappedUserdata<F>
where
    F: for<'l> FnOnce(&'l Lua) -> Result<AnyUserData>,
{
    fn into_lua(self, lua: &Lua) -> Result<Value> {
        (self.0)(lua).map(Value::UserData)
    }
}

mod cell;
mod ext;
mod registry;

// #[cfg(test)]
// mod assertions {
//     use super::*;

//     static_assertions::assert_not_impl_any!(AnyUserData: Send);
// }
