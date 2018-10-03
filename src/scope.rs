use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem;
use std::os::raw::c_void;
use std::rc::Rc;
use std::string::String as StdString;

use error::{Error, Result};
use ffi;
use function::Function;
use lua::Lua;
use types::Callback;
use userdata::{AnyUserData, MetaMethod, UserData, UserDataMethods};
use util::{
    assert_stack, init_userdata_metatable, protect_lua_closure, push_string, push_userdata,
    take_userdata, StackGuard,
};
use value::{FromLuaMulti, MultiValue, ToLuaMulti, Value};

/// Constructed by the [`Lua::scope`] method, allows temporarily passing to Lua userdata that is
/// !Send, and callbacks that are !Send and not 'static.
///
/// See [`Lua::scope`] for more details.
///
/// [`Lua::scope`]: struct.Lua.html#method.scope
pub struct Scope<'scope> {
    lua: &'scope Lua,
    destructors: RefCell<Vec<Box<Fn() -> Box<Any> + 'scope>>>,
    // 'scope lifetime must be invariant
    _scope: PhantomData<&'scope mut &'scope ()>,
}

impl<'scope> Scope<'scope> {
    pub(crate) fn new(lua: &'scope Lua) -> Scope {
        Scope {
            lua,
            destructors: RefCell::new(Vec::new()),
            _scope: PhantomData,
        }
    }

    /// Wraps a Rust function or closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`Lua::create_function`] that creates a callback which expires on scope
    /// drop.  See [`Lua::scope`] for more details.
    ///
    /// [`Lua::create_function`]: struct.Lua.html#method.create_function
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    pub fn create_function<'lua, A, R, F>(&'lua self, func: F) -> Result<Function<'lua>>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'scope + Fn(&'lua Lua, A) -> Result<R>,
    {
        // Safe, because 'scope must outlive 'lua (due to Self containing 'scope), however the
        // callback itself must be 'scope lifetime, so the function should not be able to capture
        // anything of 'lua lifetime.  'scope can't be shortened due to being invariant, and the
        // 'lua lifetime here can't be enlarged due to coming from a universal quantification in
        // Lua::scope.
        //
        // I hope I got this explanation right, but in any case this is tested with compiletest_rs
        // to make sure callbacks can't capture handles with lifetimes outside the scope, inside the
        // scope, and owned inside the callback itself.
        unsafe {
            self.create_callback(Box::new(move |lua, args| {
                func(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            }))
        }
    }

    /// Wraps a Rust mutable closure, creating a callable Lua function handle to it.
    ///
    /// This is a version of [`Lua::create_function_mut`] that creates a callback which expires on
    /// scope drop.  See [`Lua::scope`] and [`Scope::create_function`] for more details.
    ///
    /// [`Lua::create_function_mut`]: struct.Lua.html#method.create_function_mut
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    /// [`Scope::create_function`]: #method.create_function
    pub fn create_function_mut<'lua, A, R, F>(&'lua self, func: F) -> Result<Function<'lua>>
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'scope + FnMut(&'lua Lua, A) -> Result<R>,
    {
        let func = RefCell::new(func);
        self.create_function(move |lua, args| {
            (&mut *func
                .try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?)(lua, args)
        })
    }

    /// Create a Lua userdata object from a custom userdata type.
    ///
    /// This is a version of [`Lua::create_userdata`] that creates a userdata which expires on scope
    /// drop, and does not require that the userdata type be Send (but still requires that the
    /// UserData be 'static).  See [`Lua::scope`] for more details.
    ///
    /// [`Lua::create_userdata`]: struct.Lua.html#method.create_userdata
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    pub fn create_static_userdata<'lua, T>(&'lua self, data: T) -> Result<AnyUserData<'lua>>
    where
        T: 'static + UserData,
    {
        // Safe even though T may not be Send, because the parent Lua cannot be sent to another
        // thread while the Scope is alive (or the returned AnyUserData handle even).
        unsafe {
            let u = self.lua.make_userdata(data)?;
            let mut destructors = self.destructors.borrow_mut();
            let u_destruct = u.0.clone();
            destructors.push(Box::new(move || {
                let state = u_destruct.lua.state;
                let _sg = StackGuard::new(state);
                assert_stack(state, 1);
                u_destruct.lua.push_ref(&u_destruct);
                Box::new(take_userdata::<RefCell<T>>(state))
            }));
            Ok(u)
        }
    }

    /// Create a Lua userdata object from a custom userdata type.
    ///
    /// This is a version of [`Lua::create_userdata`] that creates a userdata which expires on scope
    /// drop, and does not require that the userdata type be Send or 'static. See [`Lua::scope`] for
    /// more details.
    ///
    /// Lifting the requirement that the UserData type be 'static comes with some important
    /// limitations, so if you only need to eliminate the Send requirement, it is probably better to
    /// use [`Scope::create_static_userdata`] instead.
    ///
    /// The main limitation that comes from using non-'static userdata is that the produced userdata
    /// will no longer have a `TypeId` associated with it, becuase `TypeId` can only work for
    /// 'static types.  This means that it is impossible, once the userdata is created, to get a
    /// reference to it back *out* of an `AnyUserData` handle.  This also implies that the
    /// "function" type methods that can be added via [`UserDataMethods`] (the ones that accept
    /// `AnyUserData` as a first parameter) are vastly less useful.  Also, there is no way to re-use
    /// a single metatable for multiple non-'static types, so there is a higher cost associated with
    /// creating the userdata metatable each time a new userdata is created.
    ///
    /// [`create_static_userdata`]: #method.create_static_userdata
    /// [`Lua::create_userdata`]: struct.Lua.html#method.create_userdata
    /// [`Lua::scope`]: struct.Lua.html#method.scope
    /// [`UserDataMethods`]: trait.UserDataMethods.html
    pub fn create_nonstatic_userdata<'lua, T>(&'lua self, data: T) -> Result<AnyUserData<'lua>>
    where
        T: 'scope + UserData,
    {
        let data = Rc::new(RefCell::new(data));

        // 'callback outliving 'scope is a lie to make the types work out, required due to the
        // inability to work with the "correct" universally quantified callback type.  This is safe
        // though, because actual method callbacks are all 'static so they can't capture 'callback
        // handles anyway.
        fn wrap_method<'scope, 'lua, 'callback: 'scope, T: 'scope>(
            scope: &'lua Scope<'scope>,
            data: Rc<RefCell<T>>,
            method: NonStaticMethod<'callback, T>,
        ) -> Result<Function<'lua>> {
            // On methods that actually receive the userdata, we fake a type check on the passed in
            // userdata, where we pretend there is a unique type per call to
            // Scope::create_nonstatic_userdata.  You can grab a method from a userdata and call it
            // on a mismatched userdata type, which when using normal 'static userdata will fail
            // with a type mismatch, but here without this check would proceed as though you had
            // called the method on the original value (since we otherwise completely ignore the
            // first argument).
            let check_data = data.clone();
            let check_ud_type = move |lua: &Lua, value| {
                if let Some(value) = value {
                    if let Value::UserData(u) = value {
                        unsafe {
                            let _sg = StackGuard::new(lua.state);
                            assert_stack(lua.state, 1);
                            lua.push_ref(&u.0);
                            ffi::lua_getuservalue(lua.state, -1);
                            return ffi::lua_touserdata(lua.state, -1)
                                == check_data.as_ptr() as *mut c_void;
                        }
                    }
                }

                false
            };

            match method {
                NonStaticMethod::Method(method) => {
                    let method_data = data.clone();
                    let f = Box::new(move |lua, mut args: MultiValue<'callback>| {
                        if !check_ud_type(lua, args.pop_front()) {
                            return Err(Error::UserDataTypeMismatch);
                        }
                        let data = method_data
                            .try_borrow()
                            .map_err(|_| Error::UserDataBorrowError)?;
                        method(lua, &*data, args)
                    });
                    unsafe { scope.create_callback(f) }
                }
                NonStaticMethod::MethodMut(method) => {
                    let method = RefCell::new(method);
                    let method_data = data.clone();
                    let f = Box::new(move |lua, mut args: MultiValue<'callback>| {
                        if !check_ud_type(lua, args.pop_front()) {
                            return Err(Error::UserDataTypeMismatch);
                        }
                        let mut method = method
                            .try_borrow_mut()
                            .map_err(|_| Error::RecursiveMutCallback)?;
                        let mut data = method_data
                            .try_borrow_mut()
                            .map_err(|_| Error::UserDataBorrowMutError)?;
                        (&mut *method)(lua, &mut *data, args)
                    });
                    unsafe { scope.create_callback(f) }
                }
                NonStaticMethod::Function(function) => unsafe { scope.create_callback(function) },
                NonStaticMethod::FunctionMut(function) => {
                    let function = RefCell::new(function);
                    let f = Box::new(move |lua, args| {
                        (&mut *function
                            .try_borrow_mut()
                            .map_err(|_| Error::RecursiveMutCallback)?)(
                            lua, args
                        )
                    });
                    unsafe { scope.create_callback(f) }
                }
            }
        }

        let mut ud_methods = NonStaticUserDataMethods::default();
        T::add_methods(&mut ud_methods);

        unsafe {
            let lua = self.lua;
            let _sg = StackGuard::new(lua.state);
            assert_stack(lua.state, 6);

            push_userdata(lua.state, ())?;
            ffi::lua_pushlightuserdata(lua.state, data.as_ptr() as *mut c_void);
            ffi::lua_setuservalue(lua.state, -2);

            protect_lua_closure(lua.state, 0, 1, move |state| {
                ffi::lua_newtable(state);
            })?;

            for (k, m) in ud_methods.meta_methods {
                push_string(lua.state, k.name())?;
                lua.push_value(Value::Function(wrap_method(self, data.clone(), m)?));

                protect_lua_closure(lua.state, 3, 1, |state| {
                    ffi::lua_rawset(state, -3);
                })?;
            }

            if ud_methods.methods.is_empty() {
                init_userdata_metatable::<()>(lua.state, -1, None)?;
            } else {
                protect_lua_closure(lua.state, 0, 1, |state| {
                    ffi::lua_newtable(state);
                })?;
                for (k, m) in ud_methods.methods {
                    push_string(lua.state, &k)?;
                    lua.push_value(Value::Function(wrap_method(self, data.clone(), m)?));
                    protect_lua_closure(lua.state, 3, 1, |state| {
                        ffi::lua_rawset(state, -3);
                    })?;
                }

                init_userdata_metatable::<()>(lua.state, -2, Some(-1))?;
                ffi::lua_pop(lua.state, 1);
            }

            ffi::lua_setmetatable(lua.state, -2);

            Ok(AnyUserData(lua.pop_ref()))
        }
    }

    // Unsafe, because the callback (since it is non-'static) can capture any value with 'callback
    // scope, such as improperly holding onto an argument. So in order for this to be safe, the
    // callback must NOT capture any arguments.
    unsafe fn create_callback<'lua, 'callback>(
        &'lua self,
        f: Callback<'callback, 'scope>,
    ) -> Result<Function<'lua>> {
        let f = mem::transmute::<Callback<'callback, 'scope>, Callback<'callback, 'static>>(f);
        let f = self.lua.create_callback(f)?;

        let mut destructors = self.destructors.borrow_mut();
        let f_destruct = f.0.clone();
        destructors.push(Box::new(move || {
            let state = f_destruct.lua.state;
            let _sg = StackGuard::new(state);
            assert_stack(state, 2);
            f_destruct.lua.push_ref(&f_destruct);

            ffi::lua_getupvalue(state, -1, 1);
            let ud = take_userdata::<Callback>(state);

            ffi::lua_pushnil(state);
            ffi::lua_setupvalue(state, -2, 1);

            ffi::lua_pop(state, 1);
            Box::new(ud)
        }));
        Ok(f)
    }
}

impl<'scope> Drop for Scope<'scope> {
    fn drop(&mut self) {
        // We separate the action of invalidating the userdata in Lua and actually dropping the
        // userdata type into two phases.  This is so that, in the event a userdata drop panics, we
        // can be sure that all of the userdata in Lua is actually invalidated.

        let to_drop = self
            .destructors
            .get_mut()
            .drain(..)
            .map(|destructor| destructor())
            .collect::<Vec<_>>();
        drop(to_drop);
    }
}

enum NonStaticMethod<'lua, T> {
    Method(Box<Fn(&'lua Lua, &T, MultiValue<'lua>) -> Result<MultiValue<'lua>>>),
    MethodMut(Box<FnMut(&'lua Lua, &mut T, MultiValue<'lua>) -> Result<MultiValue<'lua>>>),
    Function(Box<Fn(&'lua Lua, MultiValue<'lua>) -> Result<MultiValue<'lua>>>),
    FunctionMut(Box<FnMut(&'lua Lua, MultiValue<'lua>) -> Result<MultiValue<'lua>>>),
}

struct NonStaticUserDataMethods<'lua, T: UserData> {
    methods: HashMap<StdString, NonStaticMethod<'lua, T>>,
    meta_methods: HashMap<MetaMethod, NonStaticMethod<'lua, T>>,
}

impl<'lua, T: UserData> Default for NonStaticUserDataMethods<'lua, T> {
    fn default() -> NonStaticUserDataMethods<'lua, T> {
        NonStaticUserDataMethods {
            methods: HashMap::new(),
            meta_methods: HashMap::new(),
        }
    }
}

impl<'lua, T: UserData> UserDataMethods<'lua, T> for NonStaticUserDataMethods<'lua, T> {
    fn add_method<A, R, M>(&mut self, name: &str, method: M)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + Send + Fn(&'lua Lua, &T, A) -> Result<R>,
    {
        self.methods.insert(
            name.to_owned(),
            NonStaticMethod::Method(Box::new(move |lua, ud, args| {
                method(lua, ud, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_method_mut<A, R, M>(&mut self, name: &str, mut method: M)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + Send + FnMut(&'lua Lua, &mut T, A) -> Result<R>,
    {
        self.methods.insert(
            name.to_owned(),
            NonStaticMethod::MethodMut(Box::new(move |lua, ud, args| {
                method(lua, ud, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_function<A, R, F>(&mut self, name: &str, function: F)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + Send + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.methods.insert(
            name.to_owned(),
            NonStaticMethod::Function(Box::new(move |lua, args| {
                function(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_function_mut<A, R, F>(&mut self, name: &str, mut function: F)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + Send + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.methods.insert(
            name.to_owned(),
            NonStaticMethod::FunctionMut(Box::new(move |lua, args| {
                function(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_meta_method<A, R, M>(&mut self, meta: MetaMethod, method: M)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + Send + Fn(&'lua Lua, &T, A) -> Result<R>,
    {
        self.meta_methods.insert(
            meta,
            NonStaticMethod::Method(Box::new(move |lua, ud, args| {
                method(lua, ud, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_meta_method_mut<A, R, M>(&mut self, meta: MetaMethod, mut method: M)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        M: 'static + Send + FnMut(&'lua Lua, &mut T, A) -> Result<R>,
    {
        self.meta_methods.insert(
            meta,
            NonStaticMethod::MethodMut(Box::new(move |lua, ud, args| {
                method(lua, ud, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_meta_function<A, R, F>(&mut self, meta: MetaMethod, function: F)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + Send + Fn(&'lua Lua, A) -> Result<R>,
    {
        self.meta_methods.insert(
            meta,
            NonStaticMethod::Function(Box::new(move |lua, args| {
                function(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }

    fn add_meta_function_mut<A, R, F>(&mut self, meta: MetaMethod, mut function: F)
    where
        A: FromLuaMulti<'lua>,
        R: ToLuaMulti<'lua>,
        F: 'static + Send + FnMut(&'lua Lua, A) -> Result<R>,
    {
        self.meta_methods.insert(
            meta,
            NonStaticMethod::FunctionMut(Box::new(move |lua, args| {
                function(lua, A::from_lua_multi(args, lua)?)?.to_lua_multi(lua)
            })),
        );
    }
}
