# UFLinux default shell prompt (busybox ash and bash).
#
# /etc/profile sources this drop-in for login shells (getty, serial console,
# ssh, `su -`, and `uflinux-install --chroot`). The session also exports
# ENV pointing here, so interactive ash started straight from the desktop
# (for example from the terminal emulator) picks up the same prompt.
#
# The prompt is set before any user file is loaded, so a personal
# ~/.profile (login shells) or ~/.ashrc (ENV shells) can still override it.

case $- in
    *i*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

PS1='\[\e[1;36m\]\u@\h \[\e[0m\]\w \[\e[36m\]❯ \[\e[0m\]'

if [ -n "$ENV" ] && [ -f "$HOME/.ashrc" ]; then
    . "$HOME/.ashrc"
fi
