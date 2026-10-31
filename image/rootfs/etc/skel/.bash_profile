[ -f ~/.bashrc ] && . ~/.bashrc
# Start the AgentOS session on the first virtual console.
if [ "$(tty)" = /dev/tty1 ] && [ -z "$WAYLAND_DISPLAY" ] && [ -z "$AGENTOS_NO_SESSION" ]; then
  exec /usr/local/bin/agentos-session
fi
