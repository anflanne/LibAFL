//! The high-level hooks
#![allow(clippy::type_complexity)]

use core::{
    ffi::c_void,
    fmt::{self, Debug, Formatter},
    marker::PhantomData,
    mem::transmute,
    ptr::{self, addr_of},
};

use libafl::{
    executors::{inprocess::inprocess_get_state, ExitKind},
    inputs::UsesInput,
};

pub use crate::emu::SyscallHookResult;
use crate::{
    emu::{Emulator, FatPtr, HookId, MemAccessInfo, SKIP_EXEC_HOOK},
    helper::QemuHelperTuple,
    GuestAddr, GuestUsize,
};

/*
// all kinds of hooks
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Hook {
    Function(*const c_void),
    Closure(FatPtr),
    #[cfg(emulation_mode = "usermode")]
    Once(FatPtr),
    Empty,
}
*/

// all kinds of hooks
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HookRepr {
    Function(*const c_void),
    Closure(FatPtr),
    Empty,
}

pub struct HookState<const N: usize> {
    id: HookId,
    gen: HookRepr,
    post_gen: HookRepr,
    execs: [HookRepr; N],
}

pub enum Hook<F, C, R: Clone> {
    Function(F),
    Closure(C),
    Raw(R),
    Empty,
}

impl<F, C, R: Clone> Hook<F, C, R> {
    pub fn is_empty(&self) -> bool {
        matches!(self, Hook::Empty)
    }
}

macro_rules! get_raw_hook {
    ($h:expr, $replacement:expr, $fntype:ty) => {
        match $h {
            Hook::Function(_) | Hook::Closure(_) => Some($replacement as $fntype),
            Hook::Raw(r) => {
                let v: $fntype = transmute(r);
                Some(v)
            }
            Hook::Empty => None,
        }
    };
}

macro_rules! hook_to_repr {
    ($h:expr) => {
        match $h {
            Hook::Function(f) => HookRepr::Function(f as *const libc::c_void),
            Hook::Closure(c) => HookRepr::Closure(transmute(c)),
            Hook::Raw(_) => HookRepr::Empty, // managed by emu
            Hook::Empty => HookRepr::Empty,
        }
    };
}

static mut QEMU_HOOKS_PTR: *const c_void = ptr::null();

#[must_use]
pub unsafe fn get_qemu_hooks<'a, QT, S>() -> &'a mut QemuHooks<QT, S>
where
    S: UsesInput,
    QT: QemuHelperTuple<S>,
{
    (QEMU_HOOKS_PTR as *mut QemuHooks<QT, S>)
        .as_mut()
        .expect("A high-level hook is installed but QemuHooks is not initialized")
}

macro_rules! create_wrapper {
    ($name:ident, ($($param:ident : $param_type:ty),*)) => {
        paste::paste! {
            extern "C" fn [<func_ $name _hook_wrapper>]<QT, S>(hook: &mut c_void, $($param: $param_type),*)
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    let func: fn(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) = transmute(hook as *mut c_void);
                    func(hooks, inprocess_get_state::<S>(), $($param),*);
                }
            }

            extern "C" fn [<closure_ $name _hook_wrapper>]<QT, S>(hook: &mut FatPtr, $($param: $param_type),*)
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    let func: &mut Box<dyn FnMut(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*)> = transmute(hook);
                    func(hooks, inprocess_get_state::<S>(), $($param),*);
                }
            }
        }
    };
    ($name:ident, ($($param:ident : $param_type:ty),*), $ret_type:ty) => {
        paste::paste! {
            extern "C" fn [<func_ $name _hook_wrapper>]<QT, S>(hook: &mut c_void, $($param: $param_type),*) -> $ret_type
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    let func: fn(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) -> $ret_type= transmute(hook as *mut c_void);
                    func(hooks, inprocess_get_state::<S>(), $($param),*)
                }
            }

            extern "C" fn [<closure_ $name _hook_wrapper>]<QT, S>(hook: &mut FatPtr, $($param: $param_type),*) -> $ret_type
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    let func: &mut Box<dyn FnMut(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) -> $ret_type> = transmute(hook);
                    func(hooks, inprocess_get_state::<S>(), $($param),*)
                }
            }
        }
    };
}

macro_rules! create_gen_wrapper {
    ($name:ident, ($($param:ident : $param_type:ty),*), $ret_type:ty, $execs:literal) => {
        paste::paste! {
            extern "C" fn [<$name _gen_hook_wrapper>]<QT, S>(hook: &mut HookState<{ $execs }>, $($param: $param_type),*) -> $ret_type
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    match &mut hook.gen {
                        HookRepr::Function(ptr) => {
                            let func: fn(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) -> Option<$ret_type> =
                                transmute(*ptr);
                            func(hooks, inprocess_get_state::<S>(), $($param),*).map_or(SKIP_EXEC_HOOK, |id| id)
                        }
                        HookRepr::Closure(ptr) => {
                            let func: &mut Box<
                                dyn FnMut(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) -> Option<$ret_type>,
                            > = transmute(ptr);
                            func(hooks, inprocess_get_state::<S>(), $($param),*).map_or(SKIP_EXEC_HOOK, |id| id)
                        }
                        _ => 0,
                    }
                }
            }
        }
    }
}

macro_rules! create_post_gen_wrapper {
    ($name:ident, ($($param:ident : $param_type:ty),*), $execs:literal) => {
        paste::paste! {
            extern "C" fn [<$name _post_gen_hook_wrapper>]<QT, S>(hook: &mut HookState<{ $execs }>, $($param: $param_type),*)
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    match &mut hook.post_gen {
                        HookRepr::Function(ptr) => {
                            let func: fn(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) =
                                transmute(*ptr);
                            func(hooks, inprocess_get_state::<S>(), $($param),*);
                        }
                        HookRepr::Closure(ptr) => {
                            let func: &mut Box<
                                dyn FnMut(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*),
                            > = transmute(ptr);
                            func(hooks, inprocess_get_state::<S>(), $($param),*);
                        }
                        _ => (),
                    }
                }
            }
        }
    }
}

macro_rules! create_exec_wrapper {
    ($name:ident, ($($param:ident : $param_type:ty),*), $execidx:literal, $execs:literal) => {
        paste::paste! {
            extern "C" fn [<$name _ $execidx _exec_hook_wrapper>]<QT, S>(hook: &mut HookState<{ $execs }>, $($param: $param_type),*)
            where
                S: UsesInput,
                QT: QemuHelperTuple<S>,
            {
                unsafe {
                    let hooks = get_qemu_hooks::<QT, S>();
                    match &mut hook.execs[$execidx] {
                        HookRepr::Function(ptr) => {
                            let func: fn(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*) = transmute(*ptr);
                            func(hooks, inprocess_get_state::<S>(), $($param),*);
                        }
                        HookRepr::Closure(ptr) => {
                            let func: &mut Box<dyn FnMut(&mut QemuHooks<QT, S>, Option<&mut S>, $($param_type),*)> =
                                transmute(ptr);
                            func(hooks, inprocess_get_state::<S>(), $($param),*);
                        }
                        _ => (),
                    }
                }
            }
        }
    }
}

static mut GENERIC_HOOKS: Vec<(HookId, FatPtr)> = vec![];
create_wrapper!(generic, (pc: GuestAddr));
static mut BACKDOOR_HOOKS: Vec<(HookId, FatPtr)> = vec![];
create_wrapper!(backdoor, (pc: GuestAddr));

#[cfg(emulation_mode = "usermode")]
static mut PRE_SYSCALL_HOOKS: Vec<(HookId, FatPtr)> = vec![];
#[cfg(emulation_mode = "usermode")]
create_wrapper!(pre_syscall, (sys_num: i32,
    a0: GuestAddr,
    a1: GuestAddr,
    a2: GuestAddr,
    a3: GuestAddr,
    a4: GuestAddr,
    a5: GuestAddr,
    a6: GuestAddr,
    a7: GuestAddr), SyscallHookResult);
#[cfg(emulation_mode = "usermode")]
static mut POST_SYSCALL_HOOKS: Vec<(HookId, FatPtr)> = vec![];
#[cfg(emulation_mode = "usermode")]
create_wrapper!(post_syscall, (res: GuestAddr, sys_num: i32,
    a0: GuestAddr,
    a1: GuestAddr,
    a2: GuestAddr,
    a3: GuestAddr,
    a4: GuestAddr,
    a5: GuestAddr,
    a6: GuestAddr,
    a7: GuestAddr), GuestAddr);
#[cfg(emulation_mode = "usermode")]
static mut NEW_THREAD_HOOKS: Vec<(HookId, FatPtr)> = vec![];
#[cfg(emulation_mode = "usermode")]
create_wrapper!(new_thread, (tid: u32), bool);

static mut EDGE_HOOKS: Vec<HookState<1>> = vec![];
create_gen_wrapper!(edge, (src: GuestAddr, dest: GuestAddr), u64, 1);
create_exec_wrapper!(edge, (id: u64), 0, 1);

static mut BLOCK_HOOKS: Vec<HookState<1>> = vec![];
create_gen_wrapper!(block, (addr: GuestAddr), u64, 1);
create_post_gen_wrapper!(block, (addr: GuestAddr, len: GuestUsize), 1);
create_exec_wrapper!(block, (id: u64), 0, 1);

static mut READ_HOOKS: Vec<HookState<5>> = vec![];
create_gen_wrapper!(read, (pc: GuestAddr, info: MemAccessInfo), u64, 5);
create_exec_wrapper!(read, (id: u64, addr: GuestAddr), 0, 5);
create_exec_wrapper!(read, (id: u64, addr: GuestAddr), 1, 5);
create_exec_wrapper!(read, (id: u64, addr: GuestAddr), 2, 5);
create_exec_wrapper!(read, (id: u64, addr: GuestAddr), 3, 5);
create_exec_wrapper!(read, (id: u64, addr: GuestAddr, size: usize), 4, 5);

static mut WRITE_HOOKS: Vec<HookState<5>> = vec![];
create_gen_wrapper!(write, (pc: GuestAddr, info: MemAccessInfo), u64, 5);
create_exec_wrapper!(write, (id: u64, addr: GuestAddr), 0, 5);
create_exec_wrapper!(write, (id: u64, addr: GuestAddr), 1, 5);
create_exec_wrapper!(write, (id: u64, addr: GuestAddr), 2, 5);
create_exec_wrapper!(write, (id: u64, addr: GuestAddr), 3, 5);
create_exec_wrapper!(write, (id: u64, addr: GuestAddr, size: usize), 4, 5);

static mut CMP_HOOKS: Vec<HookState<4>> = vec![];
create_gen_wrapper!(cmp, (pc: GuestAddr, size: usize), u64, 4);
create_exec_wrapper!(cmp, (id: u64, v0: u8, v1: u8), 0, 4);
create_exec_wrapper!(cmp, (id: u64, v0: u16, v1: u16), 1, 4);
create_exec_wrapper!(cmp, (id: u64, v0: u32, v1: u32), 2, 4);
create_exec_wrapper!(cmp, (id: u64, v0: u64, v1: u64), 3, 4);

#[cfg(emulation_mode = "usermode")]
static mut CRASH_HOOKS: Vec<HookRepr> = vec![];

#[cfg(emulation_mode = "usermode")]
extern "C" fn crash_hook_wrapper<QT, S>(target_sig: i32)
where
    S: UsesInput,
    QT: QemuHelperTuple<S>,
{
    unsafe {
        let hooks = get_qemu_hooks::<QT, S>();
        for hook in &mut CRASH_HOOKS {
            match hook {
                HookRepr::Function(ptr) => {
                    let func: fn(&mut QemuHooks<QT, S>, i32) = transmute(*ptr);
                    func(hooks, target_sig);
                }
                HookRepr::Closure(ptr) => {
                    let func: &mut Box<dyn FnMut(&mut QemuHooks<QT, S>, i32)> = transmute(ptr);
                    func(hooks, target_sig);
                }
                HookRepr::Empty => (),
            }
        }
    }
}

static mut HOOKS_IS_INITIALIZED: bool = false;
static mut FIRST_EXEC: bool = true;

pub struct QemuHooks<QT, S>
where
    QT: QemuHelperTuple<S>,
    S: UsesInput,
{
    helpers: QT,
    emulator: Emulator,
    phantom: PhantomData<S>,
}

impl<QT, S> Debug for QemuHooks<QT, S>
where
    S: UsesInput,
    QT: QemuHelperTuple<S> + Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("QemuHooks")
            .field("helpers", &self.helpers)
            .field("emulator", &self.emulator)
            .finish()
    }
}

impl<QT, S> QemuHooks<QT, S>
where
    QT: QemuHelperTuple<S>,
    S: UsesInput,
{
    pub fn new(emulator: Emulator, helpers: QT) -> Box<Self> {
        unsafe {
            assert!(
                !HOOKS_IS_INITIALIZED,
                "Only an instance of QemuHooks is permitted"
            );
            HOOKS_IS_INITIALIZED = true;
        }
        // re-translate blocks with hooks
        emulator.flush_jit();
        let slf = Box::new(Self {
            emulator,
            helpers,
            phantom: PhantomData,
        });
        slf.helpers.init_hooks_all(&slf);
        unsafe {
            QEMU_HOOKS_PTR = addr_of!(*slf) as *const c_void;
        }
        slf
    }

    #[must_use]
    pub fn match_helper<T>(&self) -> Option<&T>
    where
        T: 'static,
    {
        self.helpers.match_first_type::<T>()
    }

    #[must_use]
    pub fn match_helper_mut<T>(&mut self) -> Option<&mut T>
    where
        T: 'static,
    {
        self.helpers.match_first_type_mut::<T>()
    }

    pub fn emulator(&self) -> &Emulator {
        &self.emulator
    }

    pub fn helpers(&self) -> &QT {
        &self.helpers
    }

    pub fn helpers_mut(&mut self) -> &mut QT {
        &mut self.helpers
    }

    pub fn instruction(
        &self,
        addr: GuestAddr,
        hook: Hook<
            fn(&mut Self, Option<&mut S>, GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, GuestAddr)>,
            extern "C" fn(*const (), pc: GuestAddr),
        >,
        invalidate_block: bool,
    ) -> HookId {
        match hook {
            Hook::Function(f) => self.instruction_function(addr, f, invalidate_block),
            Hook::Closure(c) => self.instruction_closure(addr, c, invalidate_block),
            Hook::Raw(r) => {
                let z: *const () = ptr::null::<()>();
                self.emulator.set_hook(z, addr, r, invalidate_block)
            }
            Hook::Empty => HookId(0), // TODO error type
        }
    }

    pub fn instruction_function(
        &self,
        addr: GuestAddr,
        hook: fn(&mut Self, Option<&mut S>, GuestAddr),
        invalidate_block: bool,
    ) -> HookId {
        unsafe {
            self.emulator.set_hook(
                transmute(hook),
                addr,
                func_generic_hook_wrapper::<QT, S>,
                invalidate_block,
            )
        }
    }

    pub fn instruction_closure(
        &self,
        addr: GuestAddr,
        hook: Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, GuestAddr)>,
        invalidate_block: bool,
    ) -> HookId {
        unsafe {
            let fat: FatPtr = transmute(hook);
            GENERIC_HOOKS.push((HookId(0), fat));
            let id = self.emulator.set_hook(
                &mut GENERIC_HOOKS.last_mut().unwrap().1,
                addr,
                closure_generic_hook_wrapper::<QT, S>,
                invalidate_block,
            );
            GENERIC_HOOKS.last_mut().unwrap().0 = id;
            id
        }
    }

    pub fn edges(
        &self,
        generation_hook: Hook<
            fn(&mut Self, Option<&mut S>, src: GuestAddr, dest: GuestAddr) -> Option<u64>,
            Box<
                dyn for<'a> FnMut(
                    &'a mut Self,
                    Option<&'a mut S>,
                    GuestAddr,
                    GuestAddr,
                ) -> Option<u64>,
            >,
            extern "C" fn(*const (), src: GuestAddr, dest: GuestAddr) -> u64,
        >,
        execution_hook: Hook<
            fn(&mut Self, Option<&mut S>, id: u64),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64)>,
            extern "C" fn(*const (), id: u64),
        >,
    ) -> HookId {
        unsafe {
            let gen = get_raw_hook!(
                generation_hook,
                edge_gen_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<1>, src: GuestAddr, dest: GuestAddr) -> u64
            );
            let exec = get_raw_hook!(
                execution_hook,
                edge_0_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<1>, id: u64)
            );
            EDGE_HOOKS.push(HookState {
                id: HookId(0),
                gen: hook_to_repr!(generation_hook),
                post_gen: HookRepr::Empty,
                execs: [hook_to_repr!(execution_hook)],
            });
            let id = self
                .emulator
                .add_edge_hooks(EDGE_HOOKS.last_mut().unwrap(), gen, exec);
            EDGE_HOOKS.last_mut().unwrap().id = id;
            id
        }
    }

    pub fn blocks(
        &self,
        generation_hook: Hook<
            fn(&mut Self, Option<&mut S>, pc: GuestAddr) -> Option<u64>,
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, GuestAddr) -> Option<u64>>,
            extern "C" fn(*const (), pc: GuestAddr) -> u64,
        >,
        post_generation_hook: Hook<
            fn(&mut Self, Option<&mut S>, pc: GuestAddr, block_length: GuestUsize),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&mut S>, GuestAddr, GuestUsize)>,
            extern "C" fn(*const (), pc: GuestAddr, block_length: GuestUsize),
        >,
        execution_hook: Hook<
            fn(&mut Self, Option<&mut S>, id: u64),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64)>,
            extern "C" fn(*const (), id: u64),
        >,
    ) -> HookId {
        unsafe {
            let gen = get_raw_hook!(
                generation_hook,
                block_gen_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<1>, pc: GuestAddr) -> u64
            );
            let postgen = get_raw_hook!(
                post_generation_hook,
                block_post_gen_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<1>, pc: GuestAddr, block_length: GuestUsize)
            );
            let exec = get_raw_hook!(
                execution_hook,
                block_0_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<1>, id: u64)
            );
            BLOCK_HOOKS.push(HookState {
                id: HookId(0),
                gen: hook_to_repr!(generation_hook),
                post_gen: hook_to_repr!(post_generation_hook),
                execs: [hook_to_repr!(execution_hook)],
            });
            let id =
                self.emulator
                    .add_block_hooks(BLOCK_HOOKS.last_mut().unwrap(), gen, postgen, exec);
            BLOCK_HOOKS.last_mut().unwrap().id = id;
            id
        }
    }

    #[allow(clippy::similar_names)]
    pub fn reads(
        &self,
        generation_hook: Hook<
            fn(&mut Self, Option<&mut S>, pc: GuestAddr, info: MemAccessInfo) -> Option<u64>,
            Box<
                dyn for<'a> FnMut(
                    &'a mut Self,
                    Option<&'a mut S>,
                    GuestAddr,
                    MemAccessInfo,
                ) -> Option<u64>,
            >,
            extern "C" fn(*const (), pc: GuestAddr, info: MemAccessInfo) -> u64,
        >,
        execution_hook_1: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_2: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_4: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_8: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_n: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr, size: usize),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr, usize)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr, size: usize),
        >,
    ) -> HookId {
        unsafe {
            let gen = get_raw_hook!(
                generation_hook,
                read_gen_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, pc: GuestAddr, info: MemAccessInfo) -> u64
            );
            let exec1 = get_raw_hook!(
                execution_hook_1,
                read_0_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let exec2 = get_raw_hook!(
                execution_hook_2,
                read_1_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let exec4 = get_raw_hook!(
                execution_hook_4,
                read_2_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let exec8 = get_raw_hook!(
                execution_hook_8,
                read_3_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let execn = get_raw_hook!(
                execution_hook_n,
                read_4_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr, size: usize)
            );
            READ_HOOKS.push(HookState {
                id: HookId(0),
                gen: hook_to_repr!(generation_hook),
                post_gen: HookRepr::Empty,
                execs: [
                    hook_to_repr!(execution_hook_1),
                    hook_to_repr!(execution_hook_2),
                    hook_to_repr!(execution_hook_4),
                    hook_to_repr!(execution_hook_8),
                    hook_to_repr!(execution_hook_n),
                ],
            });
            let id = self.emulator.add_read_hooks(
                READ_HOOKS.last_mut().unwrap(),
                gen,
                exec1,
                exec2,
                exec4,
                exec8,
                execn,
            );
            READ_HOOKS.last_mut().unwrap().id = id;
            id
        }
    }

    pub fn writes(
        &self,
        generation_hook: Hook<
            fn(&mut Self, Option<&mut S>, pc: GuestAddr, info: MemAccessInfo) -> Option<u64>,
            Box<
                dyn for<'a> FnMut(
                    &'a mut Self,
                    Option<&'a mut S>,
                    GuestAddr,
                    MemAccessInfo,
                ) -> Option<u64>,
            >,
            extern "C" fn(*const (), pc: GuestAddr, info: MemAccessInfo) -> u64,
        >,
        execution_hook_1: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_2: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_4: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_8: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr),
        >,
        execution_hook_n: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, addr: GuestAddr, size: usize),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, GuestAddr, usize)>,
            extern "C" fn(*const (), id: u64, addr: GuestAddr, size: usize),
        >,
    ) -> HookId {
        unsafe {
            let gen = get_raw_hook!(
                generation_hook,
                write_gen_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, pc: GuestAddr, info: MemAccessInfo) -> u64
            );
            let exec1 = get_raw_hook!(
                execution_hook_1,
                write_0_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let exec2 = get_raw_hook!(
                execution_hook_2,
                write_1_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let exec4 = get_raw_hook!(
                execution_hook_4,
                write_2_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            let exec8 = get_raw_hook!(
                execution_hook_8,
                write_3_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr)
            );
            #[allow(clippy::similar_names)]
            let execn = get_raw_hook!(
                execution_hook_n,
                write_4_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<5>, id: u64, addr: GuestAddr, size: usize)
            );
            WRITE_HOOKS.push(HookState {
                id: HookId(0),
                gen: hook_to_repr!(generation_hook),
                post_gen: HookRepr::Empty,
                execs: [
                    hook_to_repr!(execution_hook_1),
                    hook_to_repr!(execution_hook_2),
                    hook_to_repr!(execution_hook_4),
                    hook_to_repr!(execution_hook_8),
                    hook_to_repr!(execution_hook_n),
                ],
            });
            let id = self.emulator.add_write_hooks(
                WRITE_HOOKS.last_mut().unwrap(),
                gen,
                exec1,
                exec2,
                exec4,
                exec8,
                execn,
            );
            WRITE_HOOKS.last_mut().unwrap().id = id;
            id
        }
    }

    pub fn cmps(
        &self,
        generation_hook: Hook<
            fn(&mut Self, Option<&mut S>, pc: GuestAddr, size: usize) -> Option<u64>,
            Box<
                dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, GuestAddr, usize) -> Option<u64>,
            >,
            extern "C" fn(*const (), pc: GuestAddr, size: usize) -> u64,
        >,
        execution_hook_1: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, v0: u8, v1: u8),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, u8, u8)>,
            extern "C" fn(*const (), id: u64, v0: u8, v1: u8),
        >,
        execution_hook_2: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, v0: u16, v1: u16),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, u16, u16)>,
            extern "C" fn(*const (), id: u64, v0: u16, v1: u16),
        >,
        execution_hook_4: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, v0: u32, v1: u32),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, u32, u32)>,
            extern "C" fn(*const (), id: u64, v0: u32, v1: u32),
        >,
        execution_hook_8: Hook<
            fn(&mut Self, Option<&mut S>, id: u64, v0: u64, v1: u64),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u64, u64, u64)>,
            extern "C" fn(*const (), id: u64, v0: u64, v1: u64),
        >,
    ) -> HookId {
        unsafe {
            let gen = get_raw_hook!(
                generation_hook,
                cmp_gen_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<4>, pc: GuestAddr, size: usize) -> u64
            );
            let exec1 = get_raw_hook!(
                execution_hook_1,
                cmp_0_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<4>, id: u64, v0: u8, v1: u8)
            );
            let exec2 = get_raw_hook!(
                execution_hook_2,
                cmp_1_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<4>, id: u64, v0: u16, v1: u16)
            );
            let exec4 = get_raw_hook!(
                execution_hook_4,
                cmp_2_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<4>, id: u64, v0: u32, v1: u32)
            );
            let exec8 = get_raw_hook!(
                execution_hook_8,
                cmp_3_exec_hook_wrapper::<QT, S>,
                extern "C" fn(&mut HookState<4>, id: u64, v0: u64, v1: u64)
            );
            CMP_HOOKS.push(HookState {
                id: HookId(0),
                gen: hook_to_repr!(generation_hook),
                post_gen: HookRepr::Empty,
                execs: [
                    hook_to_repr!(execution_hook_1),
                    hook_to_repr!(execution_hook_2),
                    hook_to_repr!(execution_hook_4),
                    hook_to_repr!(execution_hook_8),
                ],
            });
            let id = self.emulator.add_cmp_hooks(
                CMP_HOOKS.last_mut().unwrap(),
                gen,
                exec1,
                exec2,
                exec4,
                exec8,
            );
            CMP_HOOKS.last_mut().unwrap().id = id;
            id
        }
    }

    pub fn backdoor(
        &self,
        hook: Hook<
            fn(&mut Self, Option<&mut S>, GuestAddr),
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, GuestAddr)>,
            extern "C" fn(*const (), pc: GuestAddr),
        >,
    ) -> HookId {
        match hook {
            Hook::Function(f) => self.backdoor_function(f),
            Hook::Closure(c) => self.backdoor_closure(c),
            Hook::Raw(r) => {
                let z: *const () = ptr::null::<()>();
                self.emulator.add_backdoor_hook(z, r)
            }
            Hook::Empty => HookId(0), // TODO error type
        }
    }

    pub fn backdoor_function(&self, hook: fn(&mut Self, Option<&mut S>, pc: GuestAddr)) -> HookId {
        unsafe {
            self.emulator
                .add_backdoor_hook(transmute(hook), func_backdoor_hook_wrapper::<QT, S>)
        }
    }

    pub fn backdoor_closure(
        &self,
        hook: Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, GuestAddr)>,
    ) -> HookId {
        unsafe {
            let fat: FatPtr = transmute(hook);
            BACKDOOR_HOOKS.push((HookId(0), fat));
            let id = self.emulator.add_backdoor_hook(
                &mut BACKDOOR_HOOKS.last_mut().unwrap().1,
                closure_backdoor_hook_wrapper::<QT, S>,
            );
            BACKDOOR_HOOKS.last_mut().unwrap().0 = id;
            id
        }
    }

    #[cfg(emulation_mode = "usermode")]
    #[allow(clippy::type_complexity)]
    pub fn syscalls(
        &self,
        hook: Hook<
            fn(
                &mut Self,
                Option<&mut S>,
                sys_num: i32,
                a0: GuestAddr,
                a1: GuestAddr,
                a2: GuestAddr,
                a3: GuestAddr,
                a4: GuestAddr,
                a5: GuestAddr,
                a6: GuestAddr,
                a7: GuestAddr,
            ) -> SyscallHookResult,
            Box<
                dyn for<'a> FnMut(
                    &'a mut Self,
                    Option<&'a mut S>,
                    i32,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                ) -> SyscallHookResult,
            >,
            extern "C" fn(
                *const (),
                i32,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
            ) -> SyscallHookResult,
        >,
    ) -> HookId {
        match hook {
            Hook::Function(f) => self.syscalls_function(f),
            Hook::Closure(c) => self.syscalls_closure(c),
            Hook::Raw(r) => {
                let z: *const () = ptr::null::<()>();
                self.emulator.add_pre_syscall_hook(z, r)
            }
            Hook::Empty => HookId(0), // TODO error type
        }
    }

    #[cfg(emulation_mode = "usermode")]
    #[allow(clippy::type_complexity)]
    pub fn syscalls_function(
        &self,
        hook: fn(
            &mut Self,
            Option<&mut S>,
            sys_num: i32,
            a0: GuestAddr,
            a1: GuestAddr,
            a2: GuestAddr,
            a3: GuestAddr,
            a4: GuestAddr,
            a5: GuestAddr,
            a6: GuestAddr,
            a7: GuestAddr,
        ) -> SyscallHookResult,
    ) -> HookId {
        unsafe {
            self.emulator
                .add_pre_syscall_hook(transmute(hook), func_pre_syscall_hook_wrapper::<QT, S>)
        }
    }

    #[cfg(emulation_mode = "usermode")]
    #[allow(clippy::type_complexity)]
    pub fn syscalls_closure(
        &self,
        hook: Box<
            dyn for<'a> FnMut(
                &'a mut Self,
                Option<&'a mut S>,
                i32,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
            ) -> SyscallHookResult,
        >,
    ) -> HookId {
        unsafe {
            let fat: FatPtr = transmute(hook);
            PRE_SYSCALL_HOOKS.push((HookId(0), fat));
            let id = self.emulator.add_pre_syscall_hook(
                &mut PRE_SYSCALL_HOOKS.last_mut().unwrap().1,
                closure_pre_syscall_hook_wrapper::<QT, S>,
            );
            PRE_SYSCALL_HOOKS.last_mut().unwrap().0 = id;
            id
        }
    }

    #[cfg(emulation_mode = "usermode")]
    #[allow(clippy::type_complexity)]
    pub fn after_syscalls(
        &self,
        hook: Hook<
            fn(
                &mut Self,
                Option<&mut S>,
                res: GuestAddr,
                sys_num: i32,
                a0: GuestAddr,
                a1: GuestAddr,
                a2: GuestAddr,
                a3: GuestAddr,
                a4: GuestAddr,
                a5: GuestAddr,
                a6: GuestAddr,
                a7: GuestAddr,
            ) -> GuestAddr,
            Box<
                dyn for<'a> FnMut(
                    &'a mut Self,
                    Option<&mut S>,
                    GuestAddr,
                    i32,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                    GuestAddr,
                ) -> GuestAddr,
            >,
            extern "C" fn(
                *const (),
                GuestAddr,
                i32,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
            ) -> GuestAddr,
        >,
    ) -> HookId {
        match hook {
            Hook::Function(f) => self.after_syscalls_function(f),
            Hook::Closure(c) => self.after_syscalls_closure(c),
            Hook::Raw(r) => {
                let z: *const () = ptr::null::<()>();
                self.emulator.add_post_syscall_hook(z, r)
            }
            Hook::Empty => HookId(0), // TODO error type
        }
    }

    #[cfg(emulation_mode = "usermode")]
    #[allow(clippy::type_complexity)]
    pub fn after_syscalls_function(
        &self,
        hook: fn(
            &mut Self,
            Option<&mut S>,
            res: GuestAddr,
            sys_num: i32,
            a0: GuestAddr,
            a1: GuestAddr,
            a2: GuestAddr,
            a3: GuestAddr,
            a4: GuestAddr,
            a5: GuestAddr,
            a6: GuestAddr,
            a7: GuestAddr,
        ) -> GuestAddr,
    ) -> HookId {
        unsafe {
            self.emulator
                .add_post_syscall_hook(transmute(hook), func_post_syscall_hook_wrapper::<QT, S>)
        }
    }

    #[cfg(emulation_mode = "usermode")]
    #[allow(clippy::type_complexity)]
    pub fn after_syscalls_closure(
        &self,
        hook: Box<
            dyn for<'a> FnMut(
                &'a mut Self,
                Option<&mut S>,
                GuestAddr,
                i32,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
                GuestAddr,
            ) -> GuestAddr,
        >,
    ) -> HookId {
        unsafe {
            let fat: FatPtr = transmute(hook);
            POST_SYSCALL_HOOKS.push((HookId(0), fat));
            let id = self.emulator.add_post_syscall_hook(
                &mut POST_SYSCALL_HOOKS.last_mut().unwrap().1,
                closure_post_syscall_hook_wrapper::<QT, S>,
            );
            POST_SYSCALL_HOOKS.last_mut().unwrap().0 = id;
            id
        }
    }

    #[cfg(emulation_mode = "usermode")]
    pub fn thread_creation(
        &self,
        hook: Hook<
            fn(&mut Self, Option<&mut S>, tid: u32) -> bool,
            Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u32) -> bool>,
            extern "C" fn(*const (), tid: u32) -> bool,
        >,
    ) -> HookId {
        match hook {
            Hook::Function(f) => self.thread_creation_function(f),
            Hook::Closure(c) => self.thread_creation_closure(c),
            Hook::Raw(r) => {
                let z: *const () = ptr::null::<()>();
                self.emulator.add_new_thread_hook(z, r)
            }
            Hook::Empty => HookId(0), // TODO error type
        }
    }

    #[cfg(emulation_mode = "usermode")]
    pub fn thread_creation_function(
        &self,
        hook: fn(&mut Self, Option<&mut S>, tid: u32) -> bool,
    ) -> HookId {
        unsafe {
            self.emulator
                .add_new_thread_hook(transmute(hook), func_new_thread_hook_wrapper::<QT, S>)
        }
    }

    #[cfg(emulation_mode = "usermode")]
    pub fn thread_creation_closure(
        &self,
        hook: Box<dyn for<'a> FnMut(&'a mut Self, Option<&'a mut S>, u32) -> bool>,
    ) -> HookId {
        unsafe {
            let fat: FatPtr = transmute(hook);
            NEW_THREAD_HOOKS.push((HookId(0), fat));
            let id = self.emulator.add_new_thread_hook(
                &mut NEW_THREAD_HOOKS.last_mut().unwrap().1,
                closure_new_thread_hook_wrapper::<QT, S>,
            );
            NEW_THREAD_HOOKS.last_mut().unwrap().0 = id;
            id
        }
    }

    #[cfg(emulation_mode = "usermode")]
    pub fn crash_function(&self, hook: fn(&mut Self, target_signal: i32)) {
        unsafe {
            self.emulator.set_crash_hook(crash_hook_wrapper::<QT, S>);
            CRASH_HOOKS.push(HookRepr::Function(hook as *const libc::c_void));
        }
    }

    #[cfg(emulation_mode = "usermode")]
    pub fn crash_closure(&self, hook: Box<dyn FnMut(&mut Self, i32)>) {
        unsafe {
            self.emulator.set_crash_hook(crash_hook_wrapper::<QT, S>);
            CRASH_HOOKS.push(HookRepr::Closure(transmute(hook)));
        }
    }
}
