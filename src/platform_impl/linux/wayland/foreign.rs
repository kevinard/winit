//! Portal dialog ownership, established before the first surface commit.
use sctk::reexports::client::{globals::GlobalList, QueueHandle};
use sctk::reexports::protocols::xdg::{
    foreign::zv2::client::{zxdg_importer_v2::ZxdgImporterV2, zxdg_imported_v2::ZxdgImportedV2},
    dialog::v1::client::{xdg_wm_dialog_v1::XdgWmDialogV1, xdg_dialog_v1::XdgDialogV1},
};
use sctk::shell::{WaylandSurface, xdg::window::Window};
use super::state::WinitState;

pub(crate) struct Manager {
    importer: Option<ZxdgImporterV2>,
    dialogs: Option<XdgWmDialogV1>,
}

impl Manager {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<WinitState>) -> Self {
        Self { importer: globals.bind(qh, 1..=1, ()).ok(), dialogs: globals.bind(qh, 1..=1, ()).ok() }
    }

    pub fn import(&self, handle: &str, modal: bool, window: &Window, qh: &QueueHandle<WinitState>) -> Option<Parent> {
        if handle.is_empty() || handle.contains('\0') { return None; }
        let imported = self.importer.as_ref()?.import_toplevel(handle.to_owned(), qh, ());
        imported.set_parent_of(window.wl_surface());
        let dialog = self.dialogs.as_ref().map(|manager| {
            let dialog = manager.get_xdg_dialog(window.xdg_toplevel(), qh, ());
            if modal { dialog.set_modal(); }
            dialog
        });
        Some(Parent { imported, dialog })
    }
}

pub(crate) struct Parent { imported: ZxdgImportedV2, dialog: Option<XdgDialogV1> }
impl Drop for Parent {
    fn drop(&mut self) {
        if let Some(dialog) = self.dialog.take() { dialog.destroy(); }
        self.imported.destroy();
    }
}

// An invalidated export removes the compositor-side relationship. The child
// remains usable; the portal independently owns cancellation and child lifetime.
wayland_client::delegate_noop!(WinitState: ignore ZxdgImporterV2);
wayland_client::delegate_noop!(WinitState: ignore ZxdgImportedV2);
wayland_client::delegate_noop!(WinitState: ignore XdgWmDialogV1);
wayland_client::delegate_noop!(WinitState: ignore XdgDialogV1);
