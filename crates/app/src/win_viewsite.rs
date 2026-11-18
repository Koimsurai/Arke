//! Opening a file "from a folder view", so the program that opens it can step through the folder.
//!
//! File Explorer opens a file through its view's context menu with the view as the site. When the
//! file's handler is an app activated with a neighboring files query (Photos, Media Player), the
//! activation asks that site for the folder view (`SID_SFolderView`, `IFolderView2`) and builds the
//! query from the view's folder (`IPersistIDList`), sort columns, grouping and visible columns
//! (`IColumnManager`). Photos' previous and next arrows walk that query. A plain `ShellExecuteEx`
//! has no site and so no query: opened from Arke, Photos showed one picture and nothing else.
//!
//! [`site`] builds a stand-in view of the file's folder in Arke's sort order, handed to
//! `ShellExecuteEx` with `SEE_MASK_FLAG_HINST_IS_SITE`. What the activation reads is real; the
//! rest answers like a view whose only item, selected and focused, is the opened file.
//!
//! A folder's empty-space menu gets a site too ([`background_site`]): a view standing for the
//! folder. Windows' Sharing extension asks it for the selection when "Give access to" is filled,
//! and leaves the submenu empty without an answer.

use std::ffi::c_void;
use std::path::Path;

use arke_core::{SortColumn, SortSpec};
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, POINT, PROPERTYKEY, S_FALSE,
};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_DateCreated, PKEY_DateModified, PKEY_FileAttributes, PKEY_ItemNameDisplay,
    PKEY_ItemTypeText, PKEY_Size,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoTaskMemFree, IPersist_Impl, IServiceProvider, IServiceProvider_Impl,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    _SVGIO, BHID_SFObject, BHID_SFUIObject, CM_COLUMNINFO, CM_ENUM_FLAGS, FOLDERVIEWMODE,
    FVM_DETAILS, FVTEXTTYPE, IColumnManager, IColumnManager_Impl, IFolderView, IFolderView_Impl,
    IFolderView2, IFolderView2_Impl, ILClone, ILFindLastID, ILIsEqual, IPersistIDList,
    IPersistIDList_Impl, IShellItem, IShellItemArray, SHCreateItemFromIDList,
    SHCreateShellItemArrayFromShellItem, SHParseDisplayName, SORT_ASCENDING, SORT_DESCENDING,
    SORTCOLUMN, SVGIO_BACKGROUND, SVGIO_SELECTION, SVGIO_TYPE_MASK, SVSI_FOCUSED, SVSI_SELECT,
};
use windows::core::{BOOL, GUID, Interface, PCSTR, PCWSTR, implement};

/// The shell property a column sorts by, as the neighboring files query understands it.
/// `Extension` and `Type` both use the type text (Explorer's Type column): the query rejects
/// `System.FileExtension`, and Photos then gets no neighbors at all.
const fn property(column: SortColumn) -> PROPERTYKEY {
    match column {
        SortColumn::Name => PKEY_ItemNameDisplay,
        SortColumn::Modified => PKEY_DateModified,
        SortColumn::Created => PKEY_DateCreated,
        SortColumn::Size => PKEY_Size,
        SortColumn::Type | SortColumn::Extension => PKEY_ItemTypeText,
        SortColumn::Attributes => PKEY_FileAttributes,
    }
}

fn same_key(a: &PROPERTYKEY, b: &PROPERTYKEY) -> bool {
    a.fmtid == b.fmtid && a.pid == b.pid
}

/// `spec` as the view's sort columns: its keys in order, each property once, ending with the name
/// (Arke breaks ties by name).
pub(crate) fn sort_columns(spec: &SortSpec) -> Vec<SORTCOLUMN> {
    let mut out: Vec<SORTCOLUMN> = Vec::new();
    let keys = spec
        .keys()
        .map(|k| (property(k.column), k.ascending))
        .chain(std::iter::once((PKEY_ItemNameDisplay, true)));
    for (propkey, ascending) in keys {
        if out.iter().any(|c| same_key(&c.propkey, &propkey)) {
            continue;
        }
        out.push(SORTCOLUMN {
            propkey,
            direction: if ascending {
                SORT_ASCENDING
            } else {
                SORT_DESCENDING
            },
        });
    }
    out
}

/// An absolute item id list, freed on drop.
struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    fn parse(path: &Path) -> windows::core::Result<Self> {
        let name = crate::win_fileops::wide(path.as_os_str());
        let mut raw = std::ptr::null_mut();
        // SAFETY: name is NUL-terminated; raw receives a list owned by the returned Pidl.
        unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &raw mut raw, 0, None) }?;
        Ok(Self(raw))
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: allocated by the shell with the COM task allocator.
        unsafe { CoTaskMemFree(Some(self.0.cast_const().cast())) };
    }
}

/// The site for opening `file` from a view sorted by `spec`: answers `SID_SFolderView` with a
/// stand-in view of the file's folder. `None` when the file or its folder can't be parsed.
pub(crate) fn site(file: &Path, spec: &SortSpec) -> Option<IServiceProvider> {
    let folder = Pidl::parse(file.parent()?).ok()?;
    let file = Pidl::parse(file).ok()?;
    let sort = sort_columns(spec);
    let mut columns: Vec<PROPERTYKEY> = vec![PKEY_ItemNameDisplay];
    for c in &sort {
        if !columns.iter().any(|k| same_key(k, &c.propkey)) {
            columns.push(c.propkey);
        }
    }
    let view: IFolderView2 = com::View {
        folder,
        file,
        sort,
        columns,
    }
    .into();
    Some(
        com::Site {
            view: (*view).clone(),
        }
        .into(),
    )
}

/// The site for `folder`'s empty-space context menu: answers `SID_SFolderView` with a view that
/// stands for the folder (File Explorer's view is that menu's site).
pub(crate) fn background_site(folder: &Path) -> Option<IServiceProvider> {
    let view: IFolderView = com::BackgroundView {
        folder: Pidl::parse(folder).ok()?,
    }
    .into();
    Some(com::Site { view }.into())
}

#[allow(
    clippy::inline_always,
    clippy::ref_as_ptr,
    clippy::wildcard_imports,
    clippy::unused_self,
    clippy::unnecessary_wraps,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "The macro-generated code and the shell's COM signatures trip several pedantic lints"
)]
mod com {
    use super::*;

    type Res<T> = windows::core::Result<T>;

    /// What a caller of `ShellExecuteEx` is asked for: only the folder view.
    #[implement(IServiceProvider)]
    pub(super) struct Site {
        pub(super) view: IFolderView,
    }

    impl IServiceProvider_Impl for Site_Impl {
        fn QueryService(
            &self,
            service: *const GUID,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> Res<()> {
            if service.is_null() || riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // SAFETY: checked non-null above. SID_SFolderView is IID_IFolderView.
            if unsafe { *service } != IFolderView::IID {
                return Err(E_NOINTERFACE.into());
            }
            // SAFETY: riid and ppv are valid; QueryInterface fills ppv on success.
            unsafe { self.view.query(riid, ppv) }.ok()
        }
    }

    /// A view of `folder` for its empty-space menu, standing for the folder itself.
    #[implement(IFolderView)]
    pub(super) struct BackgroundView {
        pub(super) folder: Pidl,
    }

    impl BackgroundView_Impl {
        /// The folder as `riid`: the item itself, or what it binds to as an item of its parent
        /// (`ui`: `IDataObject`, ...) or as a folder (`IShellFolder`).
        fn folder_as(&self, riid: *const GUID, ppv: *mut *mut c_void, ui: bool) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // SAFETY: the list is valid for the object's lifetime.
            let folder: IShellItem = unsafe { SHCreateItemFromIDList(self.folder.0) }?;
            // SAFETY: riid and ppv are valid.
            if unsafe { folder.query(riid, ppv) }.is_ok() {
                return Ok(());
            }
            // SAFETY: riid is non-null.
            if unsafe { *riid } == IShellItemArray::IID {
                // SAFETY: plain shell call on a valid item.
                let array: IShellItemArray =
                    unsafe { SHCreateShellItemArrayFromShellItem(&folder) }?;
                // SAFETY: riid and ppv are valid.
                return unsafe { array.query(riid, ppv) }.ok();
            }
            let bind = if ui { &BHID_SFUIObject } else { &BHID_SFObject };
            // SAFETY: as above; BindToHandler fills ppv on success.
            unsafe {
                (Interface::vtable(&folder).BindToHandler)(
                    folder.as_raw(),
                    std::ptr::null_mut(),
                    bind,
                    riid,
                    ppv,
                )
            }
            .ok()
        }
    }

    impl IFolderView_Impl for BackgroundView_Impl {
        fn GetCurrentViewMode(&self) -> Res<u32> {
            Ok(FVM_DETAILS.0 as u32)
        }
        fn SetCurrentViewMode(&self, _: u32) -> Res<()> {
            Ok(())
        }
        fn GetFolder(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            self.folder_as(riid, ppv, false)
        }
        fn Item(&self, _: i32) -> Res<*mut ITEMIDLIST> {
            Err(E_INVALIDARG.into())
        }
        /// The "selection" is the folder itself (see `Items`), one item; nothing else is listed.
        fn ItemCount(&self, flags: &_SVGIO) -> Res<i32> {
            Ok(i32::from(flags.0 & SVGIO_TYPE_MASK.0 == SVGIO_SELECTION.0))
        }
        /// What the empty-space menu is for, the background or the "selection", is the folder
        /// itself. Sharing asks for the selection and fills "Give access to" for the folder, the
        /// same items as on the folder's own menu; with an empty selection it leaves it empty
        /// (checked by hand with `give_access_to_lists_its_items`). The folder's items aren't
        /// listed.
        fn Items(&self, flags: &_SVGIO, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            let kind = flags.0 & SVGIO_TYPE_MASK.0;
            if kind == SVGIO_BACKGROUND.0 || kind == SVGIO_SELECTION.0 {
                return self.folder_as(riid, ppv, true);
            }
            if !ppv.is_null() {
                // SAFETY: ppv is a valid out pointer supplied by the caller.
                unsafe { *ppv = std::ptr::null_mut() };
            }
            Err(E_FAIL.into())
        }
        fn GetSelectionMarkedItem(&self) -> Res<i32> {
            Err(E_FAIL.into())
        }
        fn GetFocusedItem(&self) -> Res<i32> {
            Err(E_FAIL.into())
        }
        fn GetItemPosition(&self, _: *const ITEMIDLIST) -> Res<POINT> {
            Err(E_NOTIMPL.into())
        }
        fn GetSpacing(&self, _: *mut POINT) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetDefaultSpacing(&self) -> Res<POINT> {
            Err(E_NOTIMPL.into())
        }
        fn GetAutoArrange(&self) -> Res<()> {
            Err(S_FALSE.into())
        }
        fn SelectItem(&self, _: i32, _: u32) -> Res<()> {
            Ok(())
        }
        fn SelectAndPositionItems(
            &self,
            _: u32,
            _: *const *const ITEMIDLIST,
            _: *const POINT,
            _: u32,
        ) -> Res<()> {
            Ok(())
        }
    }

    /// A view of `folder` sorted by `sort`, whose only item, selected and focused, is `file`.
    #[implement(IFolderView2, IPersistIDList, IColumnManager)]
    pub(super) struct View {
        pub(super) folder: Pidl,
        pub(super) file: Pidl,
        pub(super) sort: Vec<SORTCOLUMN>,
        pub(super) columns: Vec<PROPERTYKEY>,
    }

    impl View_Impl {
        fn file_item(&self) -> Res<IShellItem> {
            // SAFETY: the list is valid for the object's lifetime.
            unsafe { SHCreateItemFromIDList(self.file.0) }
        }

        /// The file as `riid`: the item itself, or what it binds to (`IDataObject`, ...).
        fn file_as(&self, riid: *const GUID, ppv: *mut *mut c_void, ui: bool) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            let item = self.file_item()?;
            // SAFETY: riid and ppv are valid.
            if unsafe { item.query(riid, ppv) }.is_ok() {
                return Ok(());
            }
            if !ui {
                return Err(E_NOINTERFACE.into());
            }
            // SAFETY: as above; BindToHandler fills ppv on success.
            unsafe {
                (Interface::vtable(&item).BindToHandler)(
                    item.as_raw(),
                    std::ptr::null_mut(),
                    &BHID_SFUIObject,
                    riid,
                    ppv,
                )
            }
            .ok()
        }
    }

    impl IFolderView_Impl for View_Impl {
        fn GetCurrentViewMode(&self) -> Res<u32> {
            Ok(FVM_DETAILS.0 as u32)
        }
        fn SetCurrentViewMode(&self, _: u32) -> Res<()> {
            Ok(())
        }
        fn GetFolder(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // SAFETY: the list is valid for the object's lifetime.
            let folder: IShellItem = unsafe { SHCreateItemFromIDList(self.folder.0) }?;
            // SAFETY: riid and ppv are valid.
            if unsafe { folder.query(riid, ppv) }.is_ok() {
                return Ok(());
            }
            // IShellFolder and friends.
            // SAFETY: as above; BindToHandler fills ppv on success.
            unsafe {
                (Interface::vtable(&folder).BindToHandler)(
                    folder.as_raw(),
                    std::ptr::null_mut(),
                    &BHID_SFObject,
                    riid,
                    ppv,
                )
            }
            .ok()
        }
        fn Item(&self, index: i32) -> Res<*mut ITEMIDLIST> {
            if index != 0 {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: the file's list is valid; the caller frees the copy of its last id.
            let child = unsafe { ILClone(ILFindLastID(self.file.0)) };
            if child.is_null() {
                return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
            }
            Ok(child)
        }
        fn ItemCount(&self, _: &_SVGIO) -> Res<i32> {
            Ok(1)
        }
        /// The selection and the whole view are the same single file.
        fn Items(&self, _: &_SVGIO, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            if riid.is_null() || ppv.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: ppv is a valid out pointer supplied by the caller.
            unsafe { *ppv = std::ptr::null_mut() };
            // SAFETY: riid is non-null.
            if unsafe { *riid } == IShellItemArray::IID {
                let array: IShellItemArray =
                    // SAFETY: plain shell call on a valid item.
                    unsafe { SHCreateShellItemArrayFromShellItem(&self.file_item()?) }?;
                // SAFETY: riid and ppv are valid.
                return unsafe { array.query(riid, ppv) }.ok();
            }
            self.file_as(riid, ppv, true)
        }
        fn GetSelectionMarkedItem(&self) -> Res<i32> {
            Ok(0)
        }
        fn GetFocusedItem(&self) -> Res<i32> {
            Ok(0)
        }
        fn GetItemPosition(&self, _: *const ITEMIDLIST) -> Res<POINT> {
            Err(E_NOTIMPL.into())
        }
        fn GetSpacing(&self, _: *mut POINT) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetDefaultSpacing(&self) -> Res<POINT> {
            Err(E_NOTIMPL.into())
        }
        fn GetAutoArrange(&self) -> Res<()> {
            Err(S_FALSE.into())
        }
        fn SelectItem(&self, _: i32, _: u32) -> Res<()> {
            Ok(())
        }
        fn SelectAndPositionItems(
            &self,
            _: u32,
            _: *const *const ITEMIDLIST,
            _: *const POINT,
            _: u32,
        ) -> Res<()> {
            Ok(())
        }
    }

    impl IFolderView2_Impl for View_Impl {
        fn SetGroupBy(&self, _: *const PROPERTYKEY, _: BOOL) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetGroupBy(&self, key: *mut PROPERTYKEY, ascending: *mut BOOL) -> Res<()> {
            // Not grouped: S_FALSE with an empty key, as Explorer answers.
            // SAFETY: the caller's out pointers, when given, are valid.
            unsafe {
                if !key.is_null() {
                    *key = PROPERTYKEY::default();
                }
                if !ascending.is_null() {
                    *ascending = true.into();
                }
            }
            Err(S_FALSE.into())
        }
        fn SetViewProperty(
            &self,
            _: *const ITEMIDLIST,
            _: *const PROPERTYKEY,
            _: *const PROPVARIANT,
        ) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetViewProperty(&self, _: *const ITEMIDLIST, _: *const PROPERTYKEY) -> Res<PROPVARIANT> {
            Err(E_NOTIMPL.into())
        }
        fn SetTileViewProperties(&self, _: *const ITEMIDLIST, _: &PCWSTR) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SetExtendedTileViewProperties(&self, _: *const ITEMIDLIST, _: &PCWSTR) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SetText(&self, _: FVTEXTTYPE, _: &PCWSTR) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SetCurrentFolderFlags(&self, _: u32, _: u32) -> Res<()> {
            Ok(())
        }
        fn GetCurrentFolderFlags(&self) -> Res<u32> {
            Ok(0)
        }
        fn GetSortColumnCount(&self) -> Res<i32> {
            Ok(self.sort.len() as i32)
        }
        fn SetSortColumns(&self, _: *const SORTCOLUMN, _: i32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetSortColumns(&self, out: *mut SORTCOLUMN, count: i32) -> Res<()> {
            if out.is_null() {
                return Err(E_POINTER.into());
            }
            let n = usize::try_from(count).unwrap_or(0).min(self.sort.len());
            // SAFETY: the caller's array holds `count` columns; at most that many are written.
            unsafe { std::ptr::copy_nonoverlapping(self.sort.as_ptr(), out, n) };
            Ok(())
        }
        fn GetItem(&self, index: i32, riid: *const GUID, ppv: *mut *mut c_void) -> Res<()> {
            if index != 0 {
                return Err(E_INVALIDARG.into());
            }
            self.file_as(riid, ppv, false)
        }
        fn GetVisibleItem(&self, _: i32, _: BOOL) -> Res<i32> {
            Err(S_FALSE.into())
        }
        fn GetSelectedItem(&self, start: i32) -> Res<i32> {
            if start <= 0 {
                Ok(0)
            } else {
                Err(S_FALSE.into())
            }
        }
        fn GetSelection(&self, _: BOOL) -> Res<IShellItemArray> {
            // SAFETY: plain shell call on a valid item.
            unsafe { SHCreateShellItemArrayFromShellItem(&self.file_item()?) }
        }
        fn GetSelectionState(&self, pidl: *const ITEMIDLIST) -> Res<u32> {
            // SAFETY: the file's list is valid; ILFindLastID and ILIsEqual only read.
            let selected =
                !pidl.is_null() && unsafe { ILIsEqual(ILFindLastID(self.file.0), pidl) }.as_bool();
            Ok(if selected {
                (SVSI_SELECT.0 | SVSI_FOCUSED.0) as u32
            } else {
                0
            })
        }
        fn InvokeVerbOnSelection(&self, _: &PCSTR) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn SetViewModeAndIconSize(&self, _: FOLDERVIEWMODE, _: i32) -> Res<()> {
            Ok(())
        }
        fn GetViewModeAndIconSize(&self, mode: *mut FOLDERVIEWMODE, size: *mut i32) -> Res<()> {
            // SAFETY: the caller's out pointers, when given, are valid.
            unsafe {
                if !mode.is_null() {
                    *mode = FVM_DETAILS;
                }
                if !size.is_null() {
                    *size = 16;
                }
            }
            Ok(())
        }
        fn SetGroupSubsetCount(&self, _: u32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetGroupSubsetCount(&self) -> Res<u32> {
            Err(E_NOTIMPL.into())
        }
        fn SetRedraw(&self, _: BOOL) -> Res<()> {
            Ok(())
        }
        fn IsMoveInSameFolder(&self) -> Res<()> {
            Err(S_FALSE.into())
        }
        fn DoRename(&self) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IPersist_Impl for View_Impl {
        fn GetClassID(&self) -> Res<GUID> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IPersistIDList_Impl for View_Impl {
        fn SetIDList(&self, _: *const ITEMIDLIST) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetIDList(&self) -> Res<*mut ITEMIDLIST> {
            // SAFETY: the folder's list is valid; the caller frees the copy.
            let copy = unsafe { ILClone(self.folder.0) };
            if copy.is_null() {
                return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
            }
            Ok(copy)
        }
    }

    impl IColumnManager_Impl for View_Impl {
        fn SetColumnInfo(&self, _: *const PROPERTYKEY, _: *const CM_COLUMNINFO) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetColumnInfo(&self, _: *const PROPERTYKEY, _: *mut CM_COLUMNINFO) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
        fn GetColumnCount(&self, _: CM_ENUM_FLAGS) -> Res<u32> {
            Ok(self.columns.len() as u32)
        }
        fn GetColumns(&self, _: CM_ENUM_FLAGS, out: *mut PROPERTYKEY, count: u32) -> Res<()> {
            if out.is_null() {
                return Err(E_POINTER.into());
            }
            let n = (count as usize).min(self.columns.len());
            // SAFETY: the caller's array holds `count` keys; at most that many are written.
            unsafe { std::ptr::copy_nonoverlapping(self.columns.as_ptr(), out, n) };
            Ok(())
        }
        fn SetColumns(&self, _: *const PROPERTYKEY, _: u32) -> Res<()> {
            Err(E_NOTIMPL.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arke_core::SortKey;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::UI::Shell::{GPFIDL_DEFAULT, SHGetPathFromIDListEx};

    fn key(c: &SORTCOLUMN) -> (u32, i32) {
        assert_eq!(
            c.propkey.fmtid, PKEY_ItemNameDisplay.fmtid,
            "all used keys share this set"
        );
        (c.propkey.pid, c.direction.0)
    }

    #[test]
    fn sort_columns_follow_the_spec_and_end_with_the_name() {
        let by_date = SortSpec::by(SortColumn::Modified, false, false);
        assert_eq!(
            sort_columns(&by_date).iter().map(key).collect::<Vec<_>>(),
            [(PKEY_DateModified.pid, -1), (PKEY_ItemNameDisplay.pid, 1)]
        );
        // A name sort keeps its own direction and doesn't repeat the name.
        let by_name = SortSpec::by(SortColumn::Name, false, false);
        assert_eq!(
            sort_columns(&by_name).iter().map(key).collect::<Vec<_>>(),
            [(PKEY_ItemNameDisplay.pid, -1)]
        );
    }

    #[test]
    fn sort_columns_keep_secondary_keys_once() {
        let mut spec = SortSpec::by(SortColumn::Type, true, false);
        spec.then[0] = Some(SortKey {
            column: SortColumn::Extension,
            ascending: false,
        });
        spec.then[1] = Some(SortKey {
            column: SortColumn::Size,
            ascending: false,
        });
        // Type and Extension share the type text, so the second is dropped.
        assert_eq!(
            sort_columns(&spec).iter().map(key).collect::<Vec<_>>(),
            [
                (PKEY_ItemTypeText.pid, 1),
                (PKEY_Size.pid, -1),
                (PKEY_ItemNameDisplay.pid, 1)
            ]
        );
    }

    #[test]
    fn every_column_has_a_property() {
        for column in [
            SortColumn::Name,
            SortColumn::Modified,
            SortColumn::Type,
            SortColumn::Size,
            SortColumn::Created,
            SortColumn::Extension,
            SortColumn::Attributes,
        ] {
            let p = property(column);
            assert_ne!(p.pid, 0, "{column:?}");
        }
    }

    /// The site answers what the activation reads: the view of the file's folder with Arke's sort.
    #[test]
    fn site_reports_the_folder_and_sort_of_the_view() {
        // SAFETY: COM for this test thread; paired below.
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let dir = std::env::temp_dir().join(format!("arke-viewsite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("b.png");
        std::fs::write(&file, b"x").unwrap();
        {
            let spec = SortSpec::by(SortColumn::Size, true, false);
            let site = site(&file, &spec).expect("site");
            // SAFETY: valid service id.
            let view: IFolderView2 = unsafe { site.QueryService(&IFolderView::IID) }.unwrap();
            // Other services are not offered.
            // SAFETY: as above.
            let refused = unsafe { site.QueryService::<IShellItem>(&IShellItem::IID) };
            assert!(refused.is_err());

            // SAFETY: plain calls on a valid view; the id list is freed below.
            unsafe {
                assert_eq!(view.GetSortColumnCount().unwrap(), 2);
                let mut cols = [SORTCOLUMN::default(); 2];
                view.GetSortColumns(&mut cols).unwrap();
                assert_eq!(
                    cols.iter().map(key).collect::<Vec<_>>(),
                    [(PKEY_Size.pid, 1), (PKEY_ItemNameDisplay.pid, 1)]
                );
                assert_eq!(view.GetSelectedItem(0).unwrap(), 0);

                let persist: IPersistIDList = view.cast().unwrap();
                let pidl = persist.GetIDList().unwrap();
                let mut buf = [0u16; 1024];
                let ok = SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT).as_bool();
                CoTaskMemFree(Some(pidl.cast_const().cast()));
                assert!(ok);
                let len = buf.iter().position(|&c| c == 0).unwrap();
                let got = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..len]));
                assert_eq!(got.canonicalize().unwrap(), dir.canonicalize().unwrap());

                let columns: IColumnManager = view.cast().unwrap();
                let n = columns
                    .GetColumnCount(windows::Win32::UI::Shell::CM_ENUM_VISIBLE)
                    .unwrap();
                assert_eq!(n, 2, "name and size");

                let selection = view.GetSelection(false).unwrap();
                assert_eq!(selection.GetCount().unwrap(), 1);
                let item = selection.GetItemAt(0).unwrap();
                let name = item
                    .GetDisplayName(windows::Win32::UI::Shell::SIGDN_FILESYSPATH)
                    .unwrap();
                let path = name.to_string().unwrap();
                CoTaskMemFree(Some(name.0.cast_const().cast()));
                assert_eq!(
                    std::path::Path::new(&path).canonicalize().unwrap(),
                    file.canonicalize().unwrap()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }

    /// The empty-space menu's view: its selection, its background and its folder are the folder,
    /// and the selection's count agrees with its items (it used to say none were selected while
    /// handing out the folder).
    #[test]
    fn background_view_stands_for_the_folder_with_a_consistent_selection() {
        use windows::Win32::System::Com::IDataObject;
        use windows::Win32::UI::Shell::{IShellFolder, SIGDN_FILESYSPATH, SVGIO_ALLVIEW};
        // SAFETY: COM for this test thread; paired below.
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let dir = std::env::temp_dir().join(format!("arke-bgview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        {
            let site = background_site(&dir).expect("site");
            // SAFETY: valid service id.
            let view: IFolderView = unsafe { site.QueryService(&IFolderView::IID) }.unwrap();
            let path_of = |item: &IShellItem| {
                // SAFETY: plain call on a valid item; the string is freed below.
                let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.unwrap();
                // SAFETY: a valid string from the shell.
                let path = unsafe { name.to_string() }.unwrap();
                // SAFETY: allocated by the shell with the COM task allocator.
                unsafe { CoTaskMemFree(Some(name.0.cast_const().cast())) };
                std::path::PathBuf::from(path).canonicalize().unwrap()
            };
            let folder = dir.canonicalize().unwrap();
            // SAFETY: plain calls on a valid view.
            unsafe {
                // The count and the items agree: one, the folder.
                assert_eq!(view.ItemCount(SVGIO_SELECTION).unwrap(), 1);
                let selection: IShellItemArray = view.Items(SVGIO_SELECTION).unwrap();
                assert_eq!(selection.GetCount().unwrap(), 1);
                assert_eq!(path_of(&selection.GetItemAt(0).unwrap()), folder);
                let _: IDataObject = view.Items(SVGIO_SELECTION).unwrap();
                // The folder's own items aren't listed.
                assert_eq!(view.ItemCount(SVGIO_ALLVIEW).unwrap(), 0);
                assert!(view.Items::<IShellItemArray>(SVGIO_ALLVIEW).is_err());
                let background: IShellItem = view.Items(SVGIO_BACKGROUND).unwrap();
                assert_eq!(path_of(&background), folder);
                let array: IShellItemArray = view.Items(SVGIO_BACKGROUND).unwrap();
                assert_eq!(array.GetCount().unwrap(), 1);
                assert_eq!(path_of(&array.GetItemAt(0).unwrap()), folder);
                let _: IDataObject = view.Items(SVGIO_BACKGROUND).unwrap();
                let _: IShellFolder = view.GetFolder().unwrap();
                assert!(view.Item(0).is_err());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        if com.is_ok() {
            // SAFETY: pairs with the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}
