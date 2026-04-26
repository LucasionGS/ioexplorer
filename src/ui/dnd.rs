use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gtk::{gdk, prelude::*};

thread_local! {
    static INTERNAL_DRAG_PATHS: RefCell<Option<Vec<PathBuf>>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DropOperation {
    Copy,
    Move,
}

pub fn install_drag_source<W, F>(widget: &W, selected_paths: F)
where
    W: IsA<gtk::Widget>,
    F: Fn(f64, f64) -> Vec<PathBuf> + 'static,
{
    let selected_paths = Rc::new(selected_paths);
    let drag_source = gtk::DragSource::builder()
        .actions(gdk::DragAction::COPY | gdk::DragAction::MOVE)
        .build();
    drag_source.set_propagation_phase(gtk::PropagationPhase::Capture);

    drag_source.connect_prepare(move |_, x, y| {
        let paths = selected_paths(x, y);
        if paths.is_empty() {
            return None;
        }

        INTERNAL_DRAG_PATHS.with(|drag_paths| {
            *drag_paths.borrow_mut() = Some(paths.clone());
        });

        let files = paths.iter().map(gio::File::for_path).collect::<Vec<_>>();
        let file_list = gdk::FileList::from_array(&files);
        Some(gdk::ContentProvider::for_value(&file_list.to_value()))
    });

    drag_source.connect_drag_end(|_, _, _| clear_internal_drag());
    drag_source.connect_drag_cancel(|_, _, _| {
        clear_internal_drag();
        false
    });

    widget.add_controller(drag_source);
}

pub fn install_drop_target<W, F>(widget: &W, on_drop: F)
where
    W: IsA<gtk::Widget>,
    F: Fn(DropOperation, Vec<PathBuf>) + 'static,
{
    let on_drop = Rc::new(on_drop);
    let drop_target = gtk::DropTarget::new(
        gdk::FileList::static_type(),
        gdk::DragAction::COPY | gdk::DragAction::MOVE,
    );

    drop_target.connect_drop(move |_, value, _, _| {
        let Ok(file_list) = value.get::<gdk::FileList>() else {
            return false;
        };

        let paths = file_list
            .files()
            .into_iter()
            .filter_map(|file| file.path())
            .collect::<Vec<_>>();

        if paths.is_empty() {
            return false;
        }

        let operation = if is_internal_drag(&paths) {
            DropOperation::Move
        } else {
            DropOperation::Copy
        };

        on_drop(operation, paths);
        true
    });

    widget.add_controller(drop_target);
}

fn clear_internal_drag() {
    INTERNAL_DRAG_PATHS.with(|drag_paths| {
        *drag_paths.borrow_mut() = None;
    });
}

fn is_internal_drag(paths: &[PathBuf]) -> bool {
    INTERNAL_DRAG_PATHS.with(|drag_paths| {
        let drag_paths = drag_paths.borrow();
        let Some(internal_paths) = drag_paths.as_ref() else {
            return false;
        };

        if internal_paths.len() != paths.len() {
            return false;
        }

        let mut internal_paths = internal_paths.clone();
        let mut drop_paths = paths.to_vec();
        internal_paths.sort();
        drop_paths.sort();
        internal_paths == drop_paths
    })
}
