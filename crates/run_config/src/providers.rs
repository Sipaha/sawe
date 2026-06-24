mod debug;
mod shell;
mod task_ref;

pub(crate) fn register_builtin(cx: &mut gpui::App) {
    crate::store::register_provider(cx, shell::ShellProvider);
    crate::store::register_provider(cx, debug::DebugProvider);
    crate::store::register_provider(cx, task_ref::TaskRefProvider);
}
