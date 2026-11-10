//! # Wayland
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::event_loop::{ActiveEventLoop, EventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

pub use crate::window::Theme;

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }
}

/// Additional methods on [`EventLoop`] that are specific to Wayland.
pub trait EventLoopExtWayland {
    /// True if the [`EventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl<T: 'static> EventLoopExtWayland for EventLoop<T> {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.event_loop.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtWayland for EventLoopBuilder<T> {
    #[inline]
    fn with_wayland(&mut self) -> &mut Self {
        self.platform_specific.forced_backend = Some(crate::platform_impl::Backend::Wayland);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// Additional methods on [`Window`] that are specific to Wayland.
///
/// [`Window`]: crate::window::Window
pub trait WindowExtWayland {
    /// Request focus-scoped inhibition. The returned guard reports actual
    /// compositor permission; it expires on window retirement or final drop.
    fn inhibit_keyboard_shortcuts(&self,changed:keyboard_shortcuts::Changed)->Option<keyboard_shortcuts::KeyboardShortcutsInhibitor>;
    /// Request a token bound to recent input on this focused source window.
    /// Returns unsupported without recent eligible input. Delivery uses
    /// `WindowEvent::ActivationTokenDone`; callers must impose a reply deadline.
    fn request_activation_token_for_input(&self) -> Result<crate::event_loop::AsyncRequestSerial, crate::error::NotSupportedError>;
    /// Returns `xdg_toplevel` of the window or [`None`] if the window is X11 window.
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>>;
}

impl WindowExtWayland for Window {
    fn inhibit_keyboard_shortcuts(&self,changed:keyboard_shortcuts::Changed)->Option<keyboard_shortcuts::KeyboardShortcutsInhibitor> {
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_)=>{let _=changed;None},
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window)=>window.inhibit_keyboard_shortcuts(changed),
        }
    }
    fn request_activation_token_for_input(&self) -> Result<crate::event_loop::AsyncRequestSerial, crate::error::NotSupportedError> {
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => Err(crate::error::NotSupportedError::new()),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.request_activation_token_for_input(),
        }
    }
    #[inline]
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>> {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => None,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.xdg_toplevel(),
        }
    }
}

/// Optional focus-scoped compositor keyboard-shortcut inhibition.
pub mod keyboard_shortcuts {
    pub use crate::platform_impl::wayland::keyboard_shortcuts::{Changed,KeyboardShortcutsInhibitor,Status};
}

/// Additional methods on [`WindowAttributes`] that are specific to Wayland.
pub trait WindowAttributesExtWayland {
    /// Sets an existing window as the non-modal transient parent.
    ///
    /// The relationship is sent before the child's initial configure request,
    /// allowing the compositor to choose auxiliary-window size and placement.
    /// The parent must belong to the same Wayland event loop. Its native lifetime
    /// is retained through creation; the application owns both windows afterward.
    fn with_transient_parent(self, parent: Arc<Window>) -> Self;

    /// Import an xdg-foreign-v2 parent before initial configuration, with an
    /// optional xdg-dialog-v1 modal hint. Unavailable protocols are ignored.
    fn with_foreign_parent(self, handle: String, modal: bool) -> Self;

    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    fn with_foreign_parent(mut self, handle: String, modal: bool) -> Self {
        self.platform_specific.wayland_foreign_parent = Some((handle, modal));
        self
    }

    #[inline]
    fn with_transient_parent(mut self, parent: Arc<Window>) -> Self {
        self.platform_specific.wayland_parent = Some(parent);
        self
    }

    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }
}

/// Additional methods on `MonitorHandle` that are specific to Wayland.
pub trait MonitorHandleExtWayland {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtWayland for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
