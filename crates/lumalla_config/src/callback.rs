//! Module responsible for handling and managing lua callbacks.
use std::{cell::RefCell, collections::HashMap, rc::Rc};

use anyhow::bail;
use lumalla_shared::CallbackRef;
use mlua::Function as LuaFunction;

/// Container for all lua callbacks that are registered. Can be cloned.
#[derive(Clone)]
pub struct CallbackState {
    inner: Rc<RefCell<CallbackStateInner>>,
}

impl Default for CallbackState {
    fn default() -> Self {
        Self::new()
    }
}

struct CallbackStateInner {
    callbacks: HashMap<CallbackRef, LuaFunction>,
    callback_counter: usize,
}

impl CallbackState {
    /// Create a new instance of the callback state.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(CallbackStateInner {
                callbacks: HashMap::new(),
                callback_counter: 1,
            })),
        }
    }

    /// Register a new callback, and return the callback reference with which it can be called.
    ///
    /// # Example
    /// ```
    /// # use lumalla_config::CallbackState;
    /// # let mut callback_state = CallbackState::new();
    /// # let lua = mlua::Lua::new();
    /// let callback = lua.create_function(|_, ()| Ok(())).expect("Failed to create callback");
    /// let callback_ref = callback_state.register_callback(callback.clone());
    /// assert_eq!(callback_ref.callback_id, 1);
    /// let callback_ref = callback_state.register_callback(callback);
    /// assert_eq!(callback_ref.callback_id, 2);
    /// ```
    pub fn register_callback(&self, callback: LuaFunction) -> CallbackRef {
        let mut inner = self.inner.borrow_mut();
        let callback_ref = CallbackRef {
            callback_id: inner.callback_counter,
        };
        inner.callback_counter += 1;
        inner.callbacks.insert(callback_ref, callback);
        callback_ref
    }

    /// Run a callback with the given callback reference. It propagates any errors that occur during
    /// the callback execution.
    ///
    /// # Examples
    /// ```
    /// # use lumalla_config::CallbackState;
    /// # let mut callback_state = CallbackState::new();
    /// # let lua = mlua::Lua::new();
    /// let callback = lua.create_function(|_, ()| Ok(())).expect("Failed to create callback");
    /// let callback_ref = callback_state.register_callback(callback);
    /// let result: anyhow::Result<()> = callback_state.run_callback(callback_ref, ());
    /// assert!(result.is_ok());
    /// ```
    pub fn run_callback<ARGS, RESULT>(
        &self,
        callback_ref: CallbackRef,
        args: ARGS,
    ) -> anyhow::Result<RESULT>
    where
        ARGS: mlua::IntoLuaMulti,
        RESULT: mlua::FromLuaMulti,
    {
        let Some(callback) = self.inner.borrow().callbacks.get(&callback_ref).cloned() else {
            bail!(
                "Tried to run callback that does not exist: callback: {}",
                callback_ref
            );
        };
        callback
            .call::<RESULT>(args)
            .map_err(|err| anyhow::anyhow!("Error while running lua callback: {err}"))
    }

    /// Get the callback with the given callback reference
    ///
    /// # Example
    /// ```
    /// # use lumalla_config::CallbackState;
    /// # let mut callback_state = CallbackState::new();
    /// # let lua = mlua::Lua::new();
    /// let callback = lua.create_function(|_, ()| Ok(())).expect("Failed to create callback");
    /// let callback_ref = callback_state.register_callback(callback.clone());
    /// assert_eq!(callback_state.get_callback(callback_ref), Some(callback));
    /// ```
    pub fn get_callback(&self, callback_ref: CallbackRef) -> Option<LuaFunction> {
        self.inner.borrow().callbacks.get(&callback_ref).cloned()
    }

    /// Forgets the given callback
    ///
    /// # Example
    /// ```
    /// # use lumalla_config::CallbackState;
    /// # let mut callback_state = CallbackState::new();
    /// # let lua = mlua::Lua::new();
    /// let callback = lua.create_function(|_, ()| Ok(())).expect("Failed to create callback");
    /// let callback_ref = callback_state.register_callback(callback);
    /// let result: anyhow::Result<()> = callback_state.run_callback(callback_ref, ());
    /// assert!(result.is_ok());
    /// callback_state.forget_callback(callback_ref);
    /// let result: anyhow::Result<()> = callback_state.run_callback(callback_ref, ());
    /// assert!(result.is_err());
    /// ```
    pub fn forget_callback(&self, callback_ref: CallbackRef) {
        self.inner.borrow_mut().callbacks.remove(&callback_ref);
    }
}
