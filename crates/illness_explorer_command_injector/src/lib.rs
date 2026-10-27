#![cfg(target_os = "windows")]
//! Shell extension COM server backing the "Open with Illness" context menu entry.
//!
//! The DLL is loaded by Explorer through a sparse MSIX package. It exposes an
//! `IExplorerCommand` implementation whose target executable is resolved relative to the
//! DLL's own module path, so no registry or environment lookups are required.

#![allow(unsafe_code)]

use std::{
    os::windows::{ffi::OsStringExt, process::CommandExt},
    path::PathBuf,
};

use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER,
            ERROR_INSUFFICIENT_BUFFER, GetLastError, HINSTANCE, MAX_PATH,
        },
        Globalization::u_strlen,
        System::{
            Com::{IBindCtx, IClassFactory, IClassFactory_Impl},
            LibraryLoader::GetModuleFileNameW,
            SystemServices::DLL_PROCESS_ATTACH,
        },
        UI::Shell::{
            ECF_DEFAULT, ECS_ENABLED, IEnumExplorerCommand, IExplorerCommand,
            IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
        },
    },
    core::{BOOL, GUID, HRESULT, HSTRING, Interface, Ref, Result, implement},
};

/// Menu entry label shown in the Explorer context menu.
const MENU_TITLE: &str = "Open with Illness";

/// Executable name resolved next to the install directory that contains this DLL.
const EXECUTABLE_NAME: &str = "illness.exe";

/// `CREATE_NO_WINDOW`: keep the spawned process from allocating a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static mut DLL_INSTANCE: HINSTANCE = HINSTANCE(std::ptr::null_mut());

#[unsafe(no_mangle)]
extern "system" fn DllMain(
    hinstdll: HINSTANCE,
    fdwreason: u32,
    _lpvreserved: *mut core::ffi::c_void,
) -> bool {
    if fdwreason == DLL_PROCESS_ATTACH {
        unsafe { DLL_INSTANCE = hinstdll };
    }

    true
}

#[implement(IExplorerCommand)]
struct ExplorerCommandInjector;

#[allow(non_snake_case)]
impl IExplorerCommand_Impl for ExplorerCommandInjector_Impl {
    fn GetTitle(&self, _: Ref<IShellItemArray>) -> Result<windows_core::PWSTR> {
        unsafe { SHStrDupW(&HSTRING::from(MENU_TITLE)) }
    }

    fn GetIcon(&self, _: Ref<IShellItemArray>) -> Result<windows_core::PWSTR> {
        let Some(exe_path) = illness_exe_path() else {
            return Err(E_FAIL.into());
        };
        unsafe { SHStrDupW(&HSTRING::from(exe_path)) }
    }

    fn GetToolTip(&self, _: Ref<IShellItemArray>) -> Result<windows_core::PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<windows_core::GUID> {
        Ok(GUID::zeroed())
    }

    fn GetState(&self, _: Ref<IShellItemArray>, _: BOOL) -> Result<u32> {
        Ok(ECS_ENABLED.0 as _)
    }

    fn Invoke(&self, psiitemarray: Ref<IShellItemArray>, _: Ref<IBindCtx>) -> Result<()> {
        let Ok(items) = psiitemarray.ok() else {
            return Ok(());
        };
        let Some(exe_path) = illness_exe_path() else {
            return Ok(());
        };

        let count = unsafe { items.GetCount()? };
        for idx in 0..count {
            let item = unsafe { items.GetItemAt(idx)? };
            let item_path = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)?.to_string()? };
            // A failing spawn is not actionable from the menu; drop the error.
            let _ = std::process::Command::new(&exe_path)
                .arg(&item_path)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }

        Ok(())
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as _)
    }

    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct ExplorerCommandInjectorFactory;

impl IClassFactory_Impl for ExplorerCommandInjectorFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<windows_core::IUnknown>,
        riid: *const windows_core::GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }

        unsafe {
            *ppvobject = std::ptr::null_mut();
        }

        if punkouter.is_none() {
            let factory: IExplorerCommand = ExplorerCommandInjector {}.into();
            unsafe { factory.query(riid, ppvobject).ok() }
        } else {
            Err(E_INVALIDARG.into())
        }
    }

    fn LockServer(&self, _: BOOL) -> Result<()> {
        Ok(())
    }
}

/// CLSID registered by the sparse MSIX package manifest.
const MODULE_ID: GUID = GUID::from_u128(0x8c4f2b16_7d3a_4e58_9b71_2f6c9d0a5e43);

#[unsafe(no_mangle)]
extern "system" fn DllGetClassObject(
    class_id: *const GUID,
    iid: *const GUID,
    out: *mut *mut std::ffi::c_void,
) -> HRESULT {
    if out.is_null() || class_id.is_null() || iid.is_null() {
        return E_INVALIDARG;
    }

    unsafe {
        *out = std::ptr::null_mut();
    }
    let class_id = unsafe { *class_id };
    if class_id == MODULE_ID {
        let instance: IClassFactory = ExplorerCommandInjectorFactory {}.into();
        unsafe { instance.query(iid, out) }
    } else {
        CLASS_E_CLASSNOTAVAILABLE
    }
}

/// Directory that contains `illness.exe`, i.e. two levels above the `appx` folder holding
/// this DLL.
fn install_folder() -> Option<PathBuf> {
    let mut buf = vec![0u16; MAX_PATH as usize];
    unsafe { GetModuleFileNameW(Some(DLL_INSTANCE.into()), &mut buf) };

    while unsafe { GetLastError() } == ERROR_INSUFFICIENT_BUFFER {
        buf = vec![0u16; buf.len() * 2];
        unsafe { GetModuleFileNameW(Some(DLL_INSTANCE.into()), &mut buf) };
    }

    let len = unsafe { u_strlen(buf.as_ptr()) } as usize;
    if len == 0 {
        return None;
    }
    let module_path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..len]));
    let install_folder = module_path.parent()?.parent()?;
    Some(install_folder.to_path_buf())
}

/// Absolute path of `illness.exe` next to the install folder.
fn illness_exe_path() -> Option<String> {
    let exe_path = install_folder()?.join(EXECUTABLE_NAME);
    Some(exe_path.to_string_lossy().into_owned())
}
