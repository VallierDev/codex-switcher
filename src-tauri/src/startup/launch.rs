use std::ffi::OsStr;

pub const AUTOSTART_ARG: &str = "--autostart";

pub fn is_autostart_launch<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .skip(1)
        .any(|arg| arg.as_ref() == AUTOSTART_ARG)
}

pub fn should_show_existing_window<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    !cfg!(target_os = "linux") || !is_autostart_launch(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_startup_argument_hides_the_window() {
        assert!(is_autostart_launch(["codex-switcher", "--autostart"]));
        assert!(!is_autostart_launch(["--autostart"]));
        assert!(!is_autostart_launch(["codex-switcher"]));
        assert!(!is_autostart_launch([
            "codex-switcher",
            "--autostart=false"
        ]));
        assert!(!is_autostart_launch([
            "codex-switcher",
            "codexswitch://import?x=--autostart"
        ]));
    }

    #[test]
    fn normal_launch_reopens_the_window_but_linux_autostart_does_not() {
        assert!(should_show_existing_window(["codex-switcher"]));
        assert_eq!(
            should_show_existing_window(["codex-switcher", "--autostart"]),
            !cfg!(target_os = "linux"),
        );
    }
}
