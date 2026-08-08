//! The desktop's context menus.
//!
//! Composed rather than added to. `context_menu::FileEntryContext` and
//! `EmptySpaceContext` already produce exactly the items the file manager
//! shows, and their label-order tests are brittle by design — extending
//! `FileEntryActions` with desktop-only fields would churn them for every
//! caller. Wrapping keeps the shared menu untouched and puts the desktop's own
//! items around it.

use std::rc::Rc;

use crate::ui::context_menu::{ContextMenuAction, ContextMenuContext, MenuAction};

/// Prepends and appends desktop-only items around a shared context.
pub struct DesktopContext<C: ContextMenuContext> {
    inner: C,
    before: Vec<ContextMenuAction>,
    after: Vec<ContextMenuAction>,
}

impl<C: ContextMenuContext> DesktopContext<C> {
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    /// Items shown above the shared ones — where "Open" belongs.
    pub fn before(
        mut self,
        label: impl Into<String>,
        icon: Option<&'static str>,
        activate: MenuAction,
    ) -> Self {
        self.before.push(ContextMenuAction::new(
            label,
            icon,
            false,
            Rc::new(move || activate()),
        ));
        self
    }

    /// Items shown below the shared ones — arranging, sorting, refreshing.
    pub fn after(
        mut self,
        label: impl Into<String>,
        icon: Option<&'static str>,
        activate: MenuAction,
    ) -> Self {
        self.after.push(ContextMenuAction::new(
            label,
            icon,
            false,
            Rc::new(move || activate()),
        ));
        self
    }
}

impl<C: ContextMenuContext> ContextMenuContext for DesktopContext<C> {
    fn actions(&self) -> Vec<ContextMenuAction> {
        let mut actions = Vec::new();
        actions.extend(self.before.iter().map(clone_action));
        actions.extend(self.inner.actions());
        actions.extend(self.after.iter().map(clone_action));
        actions
    }
}

/// `ContextMenuAction` is not `Clone` — it holds an `Rc<dyn Fn()>` and the
/// shared menu never needed to copy one. Rebuilding from the parts is enough
/// here and avoids widening that type's API for one caller.
fn clone_action(action: &ContextMenuAction) -> ContextMenuAction {
    ContextMenuAction::new(
        action.label(),
        action.icon_name(),
        action.is_destructive(),
        action.activation(),
    )
}
